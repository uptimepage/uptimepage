//! Our own Slack app's side of a button press: telling Slack's requests from
//! anyone else's, reading the one press we act on, and answering through the
//! reply address the press carries. Alert delivery stays in
//! `crate::notifier::slack`.

use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use url::Url;

use crate::error::{AppError, Result};
use crate::http_outbound::{OutboundHttpClient, post_json};
use crate::notifier::slack::ACKNOWLEDGE_ACTION;
use crate::security::mac::hmac_sha256_hex;
use crate::security::redaction::redact_url_paths;

/// Our app's Interactivity Request URL, appended to `auth.public_base_url`.
pub const INTERACTIONS_PATH: &str = "/hooks/slack/interactions";

/// How far a request's timestamp may sit from our clock, either way, before
/// it is taken for a replay.
const SIGNATURE_WINDOW_SECS: u64 = 5 * 60;

/// Slack's `v0` signature: HMAC-SHA256 under the app's signing secret over
/// `v0:{timestamp}:{raw body}`, checked in constant time.
pub fn signed_by_slack(
    signing_secret: &str,
    timestamp: &str,
    signature: &str,
    body: &[u8],
    now: i64,
) -> bool {
    let Ok(at) = timestamp.parse::<i64>() else {
        return false;
    };
    if now.abs_diff(at) > SIGNATURE_WINDOW_SECS {
        return false;
    }
    let Some(provided) = signature.strip_prefix("v0=") else {
        return false;
    };
    let expected = hmac_sha256_hex(
        signing_secret.as_bytes(),
        &[b"v0:", timestamp.as_bytes(), b":", body],
    );
    provided
        .to_ascii_lowercase()
        .as_bytes()
        .ct_eq(expected.as_bytes())
        .into()
}

/// A press on the Acknowledge button our app put on an alert, as Slack
/// reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Press {
    /// The button's value, signed when the alert was sent.
    pub value: String,
    /// The Slack channel the alert sits in.
    pub channel_id: String,
    /// Who pressed, keyed by their own workspace: in a channel shared between
    /// workspaces the press may come from one the app was never installed in.
    pub person: String,
    /// What Slack calls them.
    pub username: Option<String>,
    /// Where the answer goes; it reaches only this conversation.
    pub response_url: String,
    /// The alert's thread, for a reply the channel should see.
    pub thread_ts: Option<String>,
}

impl Press {
    /// A direct message has one reader, who already has the answer.
    pub fn in_direct_message(&self) -> bool {
        self.channel_id.starts_with('D')
    }
}

#[derive(Debug, Deserialize)]
struct Interaction {
    #[serde(rename = "type")]
    kind: String,
    user: Option<User>,
    team: Option<Team>,
    channel: Option<Channel>,
    container: Option<Container>,
    response_url: Option<String>,
    #[serde(default)]
    actions: Vec<Action>,
}

#[derive(Debug, Deserialize)]
struct User {
    id: String,
    username: Option<String>,
    name: Option<String>,
    team_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Team {
    id: String,
}

#[derive(Debug, Deserialize)]
struct Channel {
    id: String,
}

#[derive(Debug, Deserialize)]
struct Container {
    message_ts: Option<String>,
    thread_ts: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Action {
    action_id: String,
    value: Option<String>,
}

/// The Acknowledge press in a form-encoded interaction body. `None` for
/// everything else our messages send, such as a press on a link button,
/// which Slack reports too.
pub fn acknowledge_press(body: &[u8]) -> Option<Press> {
    let payload = url::form_urlencoded::parse(body)
        .find(|(key, _)| key == "payload")
        .map(|(_, value)| value)?;
    let i: Interaction = serde_json::from_str(&payload).ok()?;
    if i.kind != "block_actions" {
        return None;
    }
    let action = i.actions.into_iter().next()?;
    if action.action_id != ACKNOWLEDGE_ACTION {
        return None;
    }
    let user = i.user?;
    let team = user.team_id.or(i.team.map(|t| t.id))?;
    let container = i.container;
    Some(Press {
        value: action.value?,
        channel_id: i.channel?.id,
        person: format!("{team}:{}", user.id),
        username: user.username.or(user.name).filter(|n| !n.trim().is_empty()),
        response_url: i.response_url?,
        thread_ts: container.and_then(|c| c.thread_ts.or(c.message_ts)),
    })
}

/// A message sent back through a press's `response_url`. It never replaces
/// the alert, which others may still need to press.
#[derive(Debug, Serialize)]
pub struct Reply<'a> {
    response_type: &'static str,
    replace_original: bool,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_ts: Option<&'a str>,
}

impl<'a> Reply<'a> {
    /// Seen only by the person who pressed.
    pub fn to_presser(text: &'a str) -> Self {
        Self {
            response_type: "ephemeral",
            replace_original: false,
            text,
            thread_ts: None,
        }
    }

    /// Posted for the channel, in the alert's thread when it has one.
    pub fn to_channel(text: &'a str, thread_ts: Option<&'a str>) -> Self {
        Self {
            response_type: "in_channel",
            replace_original: false,
            text,
            thread_ts,
        }
    }
}

/// Send `reply` to a press's `response_url`, which must be Slack's own: the
/// address comes from the request body, so it is checked like any input.
pub async fn respond(
    http: &OutboundHttpClient,
    response_url: &str,
    reply: &Reply<'_>,
) -> Result<()> {
    let url = Url::parse(response_url)
        .ok()
        .filter(|u| u.scheme() == "https" && u.host_str() == Some("hooks.slack.com"))
        .ok_or_else(|| {
            AppError::Other(anyhow::anyhow!(
                "slack response_url is not a hooks.slack.com URL"
            ))
        })?;
    // The path lets anyone post into the conversation, and a failed request
    // names it.
    post_json(http, &url, reply)
        .await
        .map_err(|err| AppError::Other(anyhow::anyhow!(redact_url_paths(&err.to_string()))))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sign(secret: &str, timestamp: i64, body: &[u8]) -> String {
        let ts = timestamp.to_string();
        format!(
            "v0={}",
            hmac_sha256_hex(secret.as_bytes(), &[b"v0:", ts.as_bytes(), b":", body])
        )
    }

    pub(crate) fn press_body(action_id: &str, value: Option<&str>) -> Vec<u8> {
        let mut action = serde_json::json!({ "action_id": action_id, "type": "button" });
        if let Some(v) = value {
            action["value"] = v.into();
        }
        let payload = serde_json::json!({
            "type": "block_actions",
            "team": { "id": "T0INSTALL1" },
            "user": { "id": "U0OLENA01", "username": "olena", "team_id": "T0HOME001" },
            "channel": { "id": "C0OPS0001" },
            "container": { "type": "message", "message_ts": "1700000000.000100", "channel_id": "C0OPS0001" },
            "response_url": "https://hooks.slack.com/actions/T0INSTALL1/1/abc",
            "actions": [action],
        });
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("payload", &payload.to_string())
            .finish()
            .into_bytes()
    }

    #[test]
    fn a_signature_holds_only_for_its_secret_body_and_moment() {
        let body = b"payload=%7B%7D";
        let now = 1_700_000_000;
        let sig = sign("s3cret", now, body);
        let ts = now.to_string();
        assert!(signed_by_slack("s3cret", &ts, &sig, body, now));
        assert!(signed_by_slack(
            "s3cret",
            &ts,
            &sig.to_uppercase().replace("V0=", "v0="),
            body,
            now
        ));
        assert!(!signed_by_slack("other", &ts, &sig, body, now));
        assert!(!signed_by_slack(
            "s3cret",
            &ts,
            &sig,
            b"payload=%7B%7D%20",
            now
        ));
        assert!(!signed_by_slack(
            "s3cret",
            &(now + 1).to_string(),
            &sig,
            body,
            now
        ));
        assert!(!signed_by_slack(
            "s3cret",
            &ts,
            sig.trim_start_matches("v0="),
            body,
            now
        ));
        assert!(!signed_by_slack("s3cret", "", &sig, body, now));
        assert!(!signed_by_slack("s3cret", &ts, "", body, now));
        // A captured request replayed later, or one stamped ahead of us.
        let window = SIGNATURE_WINDOW_SECS as i64;
        assert!(signed_by_slack("s3cret", &ts, &sig, body, now + window));
        assert!(!signed_by_slack(
            "s3cret",
            &ts,
            &sig,
            body,
            now + window + 1
        ));
        assert!(!signed_by_slack(
            "s3cret",
            &ts,
            &sig,
            body,
            now - window - 1
        ));
        // Read before the signature, so a stamp at the edge of the range must
        // neither overflow nor wrap into the window.
        let edge = i64::MIN.to_string();
        assert!(!signed_by_slack("s3cret", &edge, &sig, body, now));
    }

    #[test]
    fn a_press_is_read_with_the_pressers_own_workspace() {
        let press = acknowledge_press(&press_body(ACKNOWLEDGE_ACTION, Some("a-signed"))).unwrap();
        assert_eq!(press.value, "a-signed");
        assert_eq!(press.channel_id, "C0OPS0001");
        assert_eq!(press.person, "T0HOME001:U0OLENA01");
        assert_eq!(press.username.as_deref(), Some("olena"));
        assert_eq!(press.thread_ts.as_deref(), Some("1700000000.000100"));
        assert!(!press.in_direct_message());
    }

    #[test]
    fn every_other_click_on_our_messages_is_no_press() {
        assert_eq!(acknowledge_press(&press_body("view_incident", None)), None);
        assert_eq!(
            acknowledge_press(&press_body("acknowledge_page", None)),
            None
        );
        assert_eq!(
            acknowledge_press(&press_body(ACKNOWLEDGE_ACTION, None)),
            None
        );
        assert_eq!(acknowledge_press(b"payload=not-json"), None);
        assert_eq!(acknowledge_press(b""), None);
    }

    #[test]
    fn a_reply_never_replaces_the_alert() {
        let json = serde_json::to_value(Reply::to_presser("Acknowledged.")).unwrap();
        assert_eq!(json["response_type"], "ephemeral");
        assert_eq!(json["replace_original"], false);
        assert!(json.get("thread_ts").is_none());
        let json =
            serde_json::to_value(Reply::to_channel("Acknowledged by olena.", Some("1.2"))).unwrap();
        assert_eq!(json["response_type"], "in_channel");
        assert_eq!(json["replace_original"], false);
        assert_eq!(json["thread_ts"], "1.2");
    }

    #[tokio::test]
    async fn a_reply_goes_only_to_slack() {
        let http = crate::http_outbound::build_outbound_client(
            crate::security::SsrfGuard::relaxed_for_tests(),
        );
        for url in [
            "https://evil.example/actions/1",
            "http://hooks.slack.com/actions/1",
            "https://hooks.slack.com.evil.example/actions/1",
            "not a url",
        ] {
            assert!(
                respond(&http, url, &Reply::to_presser("x")).await.is_err(),
                "{url}"
            );
        }
    }
}
