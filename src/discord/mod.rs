//! Our own Discord app's side of a button press: telling Discord's requests
//! from anyone else's, reading the interaction, and answering through the
//! webhook Discord opens for it. Alert delivery stays in
//! `crate::notifier::discord`.

use std::future::Future;
use std::time::Duration;

use ed25519_dalek::{Signature, VerifyingKey};
use hyper::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use url::Url;

use crate::error::{AppError, Result};
use crate::http_outbound::{OutboundHttpClient, send_json_for_status};
use crate::notifier::truncate_chars;
use crate::security::redaction::redact_url_paths;

/// Our app's Interactions Endpoint URL, appended to `auth.public_base_url`.
pub const INTERACTIONS_PATH: &str = "/hooks/discord/interactions";

/// How far a request's timestamp may sit from our clock, either way, before
/// it is taken for a replay.
const SIGNATURE_WINDOW_SECS: u64 = 5 * 60;

const API: &str = "https://discord.com/api/v10";

/// No try starts later than this after the first; the last one may run up to
/// the request timeout longer, still well inside the 15 minutes the
/// interaction token lives.
const REPLY_BUDGET: Duration = Duration::from_secs(120);

/// Waits between tries of the private answer after a failure that named no
/// wait: the answer can reach Discord before Discord has taken the deferral
/// the press was answered with, and until then the address is unknown to it.
const ANSWER_BACKOFF: [Duration; 3] = [
    Duration::from_millis(250),
    Duration::from_secs(1),
    Duration::from_secs(3),
];

/// Discord asking for no wait at all must not spin the loop.
const MIN_WAIT: Duration = Duration::from_millis(100);

/// Only the person who pressed sees the message.
const EPHEMERAL: u32 = 1 << 6;
/// Posted without a push or a sound for anyone.
const SUPPRESS_NOTIFICATIONS: u32 = 1 << 12;

/// Discord's signature: Ed25519 under the app's key over the timestamp
/// followed by the raw body.
pub fn signed_by_discord(
    key: &VerifyingKey,
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
    let Some(signature) = hex::decode(signature)
        .ok()
        .and_then(|b| <[u8; 64]>::try_from(b).ok())
    else {
        return false;
    };
    let message = [timestamp.as_bytes(), body].concat();
    key.verify_strict(&message, &Signature::from_bytes(&signature))
        .is_ok()
}

/// What a signed interaction asks of us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Received {
    /// Discord checking the endpoint, when the URL is saved and now and then.
    Ping,
    /// A press on a button on a message one of our webhooks posted.
    Press(Press),
    /// Anything else, by its interaction type.
    Other(u8),
}

/// A press on a button our app put on an alert, as Discord reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Press {
    /// The button's `custom_id`, signed when the alert was sent.
    pub custom_id: String,
    pub webhook_id: String,
    /// Discord's id for who pressed, the same in every server.
    pub person: String,
    pub username: Option<String>,
    /// What the channel sees them as: server nickname, else display name,
    /// else username.
    pub display_name: Option<String>,
    /// Where the answers to this press go.
    pub reply: ReplyTo,
}

#[derive(Debug, Deserialize)]
struct Interaction {
    #[serde(rename = "type")]
    kind: u8,
    application_id: Option<String>,
    token: Option<String>,
    data: Option<Data>,
    member: Option<Member>,
    user: Option<User>,
    message: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Data {
    custom_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Member {
    user: Option<User>,
    nick: Option<String>,
}

#[derive(Debug, Deserialize)]
struct User {
    id: String,
    username: Option<String>,
    global_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Message {
    webhook_id: Option<String>,
}

const PING: u8 = 1;
const MESSAGE_COMPONENT: u8 = 3;

/// The interaction in a body Discord signed. `None` when it is not one.
pub fn read(body: &[u8]) -> Option<Received> {
    let i: Interaction = serde_json::from_slice(body).ok()?;
    Some(match i.kind {
        PING => Received::Ping,
        MESSAGE_COMPONENT => press(i).map_or(Received::Other(MESSAGE_COMPONENT), Received::Press),
        other => Received::Other(other),
    })
}

fn press(i: Interaction) -> Option<Press> {
    let nick = i.member.as_ref().and_then(|m| m.nick.clone());
    // `member` in a server, `user` anywhere else.
    let user = i.member.and_then(|m| m.user).or(i.user)?;
    let named = |n: Option<String>| n.filter(|n| !n.trim().is_empty());
    let username = named(user.username);
    Some(Press {
        custom_id: i.data?.custom_id?,
        webhook_id: i.message?.webhook_id?,
        person: user.id,
        display_name: named(nick)
            .or_else(|| named(user.global_name))
            .or_else(|| username.clone()),
        username,
        reply: ReplyTo {
            application_id: i.application_id?,
            token: i.token?,
        },
    })
}

/// Our answer in the body of the interaction's own response.
#[derive(Debug, Serialize)]
pub struct Response {
    #[serde(rename = "type")]
    kind: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<ResponseData>,
}

#[derive(Debug, Default, Serialize)]
struct ResponseData {
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    flags: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_mentions: Option<AllowedMentions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    choices: Option<&'static [&'static str]>,
}

impl Response {
    pub fn pong() -> Self {
        Self {
            kind: 1,
            data: None,
        }
    }

    /// "Thinking…", seen only by the presser, until [`answer_privately`]
    /// replaces it.
    pub fn deferred_private() -> Self {
        Self {
            kind: 5,
            data: Some(ResponseData {
                flags: Some(EPHEMERAL),
                ..Default::default()
            }),
        }
    }

    /// A valid answer to any other interaction `kind`: our app registers no
    /// commands and opens no forms, so there is nothing to do.
    pub fn nothing_for(kind: u8) -> Self {
        const AUTOCOMPLETE: u8 = 4;
        if kind == AUTOCOMPLETE {
            return Self {
                kind: 8,
                data: Some(ResponseData {
                    choices: Some(&[]),
                    ..Default::default()
                }),
            };
        }
        Self {
            kind: 4,
            data: Some(ResponseData {
                content: Some(match kind {
                    MESSAGE_COMPONENT => "This button no longer works.",
                    _ => "Nothing to do here.",
                }),
                flags: Some(EPHEMERAL),
                allowed_mentions: Some(AllowedMentions::default()),
                ..Default::default()
            }),
        }
    }
}

/// Parses nothing: text from a press, a name included, never pings.
#[derive(Debug, Default, Serialize)]
struct AllowedMentions {
    parse: &'static [&'static str],
}

#[derive(Debug, Serialize)]
struct Answer<'a> {
    content: &'a str,
    allowed_mentions: AllowedMentions,
    #[serde(skip_serializing_if = "Option::is_none")]
    flags: Option<u32>,
}

/// The webhook Discord opens for one interaction, for 15 minutes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyTo {
    application_id: String,
    token: String,
}

impl ReplyTo {
    /// Both parts come from the body, so each is one encoded segment of a
    /// path on Discord's API, never a way out of it.
    fn url(&self, tail: &[&str]) -> Result<Url> {
        let unusable = || {
            AppError::Other(anyhow::anyhow!(
                "discord interaction carries no usable reply address"
            ))
        };
        let id_like = !self.application_id.is_empty()
            && self.application_id.bytes().all(|b| b.is_ascii_digit());
        if !id_like || matches!(self.token.as_str(), "" | "." | "..") {
            return Err(unusable());
        }
        let mut url = Url::parse(API).map_err(|_| unusable())?;
        url.path_segments_mut()
            .map_err(|()| unusable())?
            .push("webhooks")
            .push(&self.application_id)
            .push(&self.token)
            .extend(tail);
        Ok(url)
    }
}

/// The token is in the URL a failed request names, and would let whoever
/// reads the log post into the channel as our app.
fn scrubbed(err: AppError) -> AppError {
    AppError::Other(anyhow::anyhow!(redact_url_paths(&err.to_string())))
}

/// Replace the deferred "thinking…" with `text`, still seen only by the
/// presser. It replaces a message, so any failure may be tried again.
pub async fn answer_privately(http: &OutboundHttpClient, to: &ReplyTo, text: &str) -> Result<()> {
    let answer = Answer {
        content: text,
        allowed_mentions: AllowedMentions::default(),
        flags: None,
    };
    let url = to.url(&["messages", "@original"])?;
    retried(Retry::AnyFailure, || {
        try_send(http, Method::PATCH, &url, &answer)
    })
    .await
}

/// Post `text` for the channel, as a follow-up Discord shows attached to the
/// alert that was pressed. It wakes nobody. Sent once [`answer_privately`]
/// landed, so Discord already knows the address. Tried again only when
/// throttled, which Discord did not carry out, so it never posts twice.
pub async fn announce(http: &OutboundHttpClient, to: &ReplyTo, text: &str) -> Result<()> {
    let answer = Answer {
        content: text,
        allowed_mentions: AllowedMentions::default(),
        flags: Some(SUPPRESS_NOTIFICATIONS),
    };
    let url = to.url(&[])?;
    retried(Retry::Throttled, || {
        try_send(http, Method::POST, &url, &answer)
    })
    .await
}

/// What one try at a reply came to.
enum Tried {
    Sent,
    /// Refused for now; the next try may go after this wait, no sooner.
    Throttled(Duration),
    Failed(AppError),
}

/// Which failed tries are worth another.
#[derive(Debug, Clone, Copy)]
enum Retry {
    /// Any, on [`ANSWER_BACKOFF`] unless Discord named a wait.
    AnyFailure,
    /// Only a throttled one.
    Throttled,
}

/// Try `send` until it lands or the next try would start past
/// [`REPLY_BUDGET`]. A wait Discord asks for is kept whole: rather than
/// shorten it, the reply gives up.
async fn retried<F, Fut>(retry: Retry, mut send: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Tried>,
{
    let deadline = Instant::now() + REPLY_BUDGET;
    let mut backoff = ANSWER_BACKOFF.iter().copied();
    loop {
        let (wait, cause) = match (send().await, retry) {
            (Tried::Sent, _) => return Ok(()),
            (Tried::Throttled(wait), _) => (
                wait.max(MIN_WAIT),
                AppError::Other(anyhow::anyhow!(
                    "discord asked to wait {wait:?} before the reply"
                )),
            ),
            (Tried::Failed(err), Retry::AnyFailure) => match backoff.next() {
                Some(wait) => (wait, err),
                None => return Err(err),
            },
            (Tried::Failed(err), Retry::Throttled) => return Err(err),
        };
        if Instant::now() + wait > deadline {
            return Err(cause);
        }
        tokio::time::sleep(wait).await;
    }
}

async fn try_send(
    http: &OutboundHttpClient,
    method: Method,
    url: &Url,
    answer: &Answer<'_>,
) -> Tried {
    match send_json_for_status(http, method, url, answer).await {
        Ok((status, _)) if status.is_success() => Tried::Sent,
        Ok((StatusCode::TOO_MANY_REQUESTS, body)) => Tried::Throttled(retry_after(&body)),
        Ok((status, body)) => Tried::Failed(AppError::Other(anyhow::anyhow!(
            "discord answered {status}: {}",
            truncate_chars(&String::from_utf8_lossy(&body), 256)
        ))),
        Err(err) => Tried::Failed(scrubbed(err)),
    }
}

/// Discord's `retry_after`, in seconds with a fraction, rounded up to the
/// millisecond so the next try never goes early. A body without one waits a
/// second.
fn retry_after(body: &[u8]) -> Duration {
    #[derive(Deserialize)]
    struct RateLimited {
        retry_after: f64,
    }
    serde_json::from_slice::<RateLimited>(body)
        .ok()
        .map(|r| r.retry_after)
        .filter(|secs| secs.is_finite() && *secs >= 0.0)
        .map_or(Duration::from_secs(1), |secs| {
            Duration::from_millis((secs * 1000.0).ceil() as u64)
        })
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};

    use super::*;

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7; 32])
    }

    fn sign(key: &SigningKey, timestamp: i64, body: &[u8]) -> String {
        let message = [timestamp.to_string().as_bytes(), body].concat();
        hex::encode(key.sign(&message).to_bytes())
    }

    fn press_body(custom_id: &str) -> Vec<u8> {
        serde_json::json!({
            "type": 3,
            "id": "1300000000000000001",
            "application_id": "1200000000000000001",
            "token": "aW50ZXJhY3Rpb24.dG9rZW4-x_y",
            "guild_id": "1100000000000000001",
            "channel_id": "1100000000000000002",
            "member": {
                "nick": "Olena on call",
                "user": { "id": "1000000000000000001", "username": "olena", "global_name": "Olena" }
            },
            "message": { "id": "1300000000000000002", "webhook_id": "1112223334445556667" },
            "data": { "component_type": 2, "custom_id": custom_id }
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn a_signature_holds_only_for_its_key_body_and_moment() {
        let signer = signing_key();
        let key = signer.verifying_key();
        let body = br#"{"type":1}"#;
        let now = 1_700_000_000;
        let sig = sign(&signer, now, body);
        let ts = now.to_string();
        assert!(signed_by_discord(&key, &ts, &sig, body, now));
        assert!(signed_by_discord(&key, &ts, &sig.to_uppercase(), body, now));
        let other = SigningKey::from_bytes(&[9; 32]).verifying_key();
        assert!(!signed_by_discord(&other, &ts, &sig, body, now));
        assert!(!signed_by_discord(&key, &ts, &sig, br#"{"type":2}"#, now));
        assert!(!signed_by_discord(
            &key,
            &(now + 1).to_string(),
            &sig,
            body,
            now
        ));
        assert!(!signed_by_discord(&key, "", &sig, body, now));
        assert!(!signed_by_discord(&key, &ts, "", body, now));
        assert!(!signed_by_discord(&key, &ts, &sig[2..], body, now));
        // A captured request replayed later, or one stamped ahead of us.
        let window = SIGNATURE_WINDOW_SECS as i64;
        assert!(signed_by_discord(&key, &ts, &sig, body, now + window));
        assert!(!signed_by_discord(&key, &ts, &sig, body, now + window + 1));
        assert!(!signed_by_discord(&key, &ts, &sig, body, now - window - 1));
        let edge = i64::MIN.to_string();
        assert!(!signed_by_discord(&key, &edge, &sig, body, now));
    }

    #[test]
    fn a_press_is_read_with_the_name_the_server_shows() {
        let Some(Received::Press(press)) = read(&press_body("a-signed")) else {
            panic!("a press");
        };
        assert_eq!(press.custom_id, "a-signed");
        assert_eq!(press.webhook_id, "1112223334445556667");
        assert_eq!(press.person, "1000000000000000001");
        assert_eq!(press.username.as_deref(), Some("olena"));
        assert_eq!(press.display_name.as_deref(), Some("Olena on call"));
    }

    #[test]
    fn outside_a_server_the_press_names_the_user() {
        let mut body: serde_json::Value = serde_json::from_slice(&press_body("a")).unwrap();
        body.as_object_mut().unwrap().remove("member");
        body["user"] = serde_json::json!({ "id": "1000000000000000009", "username": "taras" });
        let Some(Received::Press(press)) = read(body.to_string().as_bytes()) else {
            panic!("a press");
        };
        assert_eq!(press.person, "1000000000000000009");
        assert_eq!(press.display_name.as_deref(), Some("taras"));
    }

    #[test]
    fn everything_but_a_press_on_a_webhook_message_is_something_else() {
        assert_eq!(read(br#"{"type":1}"#), Some(Received::Ping));
        assert_eq!(read(br#"{"type":2}"#), Some(Received::Other(2)));
        let mut body: serde_json::Value = serde_json::from_slice(&press_body("a")).unwrap();
        body["message"] = serde_json::json!({ "id": "1" });
        assert_eq!(
            read(body.to_string().as_bytes()),
            Some(Received::Other(MESSAGE_COMPONENT))
        );
        assert_eq!(read(b"not json"), None);
    }

    #[test]
    fn every_answer_is_one_discord_accepts() {
        let json = |r: Response| serde_json::to_value(r).unwrap();
        assert_eq!(json(Response::pong()), serde_json::json!({ "type": 1 }));
        assert_eq!(
            json(Response::deferred_private()),
            serde_json::json!({ "type": 5, "data": { "flags": 64 } })
        );
        assert_eq!(
            json(Response::nothing_for(4)),
            serde_json::json!({ "type": 8, "data": { "choices": [] } })
        );
        for kind in [2, 3, 5, 99] {
            let v = json(Response::nothing_for(kind));
            assert_eq!(v["type"], 4, "{kind}");
            assert_eq!(v["data"]["flags"], 64, "{kind}");
            assert_eq!(
                v["data"]["allowed_mentions"]["parse"],
                serde_json::json!([])
            );
        }
    }

    /// Plays `results` back as the tries of a reply, noting when each went.
    async fn replay(retry: Retry, results: Vec<Tried>) -> (Result<()>, Vec<Duration>) {
        let start = Instant::now();
        let went = std::cell::RefCell::new(Vec::new());
        let mut results = results.into_iter();
        let outcome = retried(retry, || {
            went.borrow_mut().push(start.elapsed());
            std::future::ready(results.next().expect("tried again after the last result"))
        })
        .await;
        (outcome, went.into_inner())
    }

    fn failed() -> Tried {
        Tried::Failed(AppError::Other(anyhow::anyhow!("discord answered 500")))
    }

    #[tokio::test(start_paused = true)]
    async fn a_throttled_reply_waits_as_long_as_discord_asks() {
        for retry in [Retry::AnyFailure, Retry::Throttled] {
            let (outcome, went) = replay(
                retry,
                vec![Tried::Throttled(Duration::from_millis(10_250)), Tried::Sent],
            )
            .await;
            assert!(outcome.is_ok(), "{retry:?}");
            assert_eq!(went, [Duration::ZERO, Duration::from_millis(10_250)]);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_wait_past_the_budget_gives_up_instead_of_going_early() {
        let (outcome, went) = replay(
            Retry::AnyFailure,
            vec![Tried::Throttled(REPLY_BUDGET + Duration::from_secs(1))],
        )
        .await;
        assert!(outcome.is_err());
        assert_eq!(went, [Duration::ZERO]);

        let throttled = (0..20)
            .map(|_| Tried::Throttled(Duration::from_secs(50)))
            .collect();
        let (outcome, went) = replay(Retry::Throttled, throttled).await;
        assert!(outcome.is_err());
        assert_eq!(
            went,
            [
                Duration::ZERO,
                Duration::from_secs(50),
                Duration::from_secs(100)
            ],
            "a third wait would end past the budget"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_private_answer_backs_off_and_stops_once_it_lands() {
        let (outcome, went) =
            replay(Retry::AnyFailure, vec![failed(), failed(), Tried::Sent]).await;
        assert!(outcome.is_ok());
        assert_eq!(
            went,
            [
                Duration::ZERO,
                Duration::from_millis(250),
                Duration::from_millis(1_250)
            ]
        );
        let (outcome, went) = replay(Retry::AnyFailure, (0..4).map(|_| failed()).collect()).await;
        assert!(outcome.is_err());
        assert_eq!(went.len(), 4, "the backoff runs out");
    }

    #[tokio::test(start_paused = true)]
    async fn an_announcement_that_failed_is_never_sent_again() {
        let (outcome, went) = replay(Retry::Throttled, vec![failed()]).await;
        assert!(outcome.is_err());
        assert_eq!(went, [Duration::ZERO]);
    }

    #[test]
    fn discords_wait_is_read_whole_and_rounded_up() {
        assert_eq!(
            retry_after(
                br#"{"message":"You are being rate limited.","retry_after":0.25,"global":false}"#
            ),
            Duration::from_millis(250)
        );
        assert_eq!(
            retry_after(br#"{"retry_after":10.0001}"#),
            Duration::from_millis(10_001)
        );
        assert_eq!(retry_after(br#"{"retry_after":7}"#), Duration::from_secs(7));
        for body in [&b"<html>"[..], br#"{"retry_after":-1}"#, b""] {
            assert_eq!(retry_after(body), Duration::from_secs(1));
        }
    }

    #[test]
    fn a_failed_reply_never_logs_its_token() {
        let to = ReplyTo {
            application_id: "1200000000000000001".into(),
            token: "aW50ZXJhY3Rpb24.dG9rZW4".into(),
        };
        let url = to.url(&["messages", "@original"]).unwrap();
        let msg = scrubbed(AppError::Other(anyhow::anyhow!(
            "request to {url} exceeded 30s"
        )))
        .to_string();
        assert!(!msg.contains("aW50ZXJhY3Rpb24"), "{msg}");
        assert!(msg.contains("https://discord.com"), "{msg}");
    }

    #[test]
    fn a_reply_goes_only_to_discords_api() {
        let reply = |app: &str, token: &str| ReplyTo {
            application_id: app.into(),
            token: token.into(),
        };
        assert_eq!(
            reply("1200000000000000001", "a.b-c_d")
                .url(&["messages", "@original"])
                .unwrap()
                .as_str(),
            "https://discord.com/api/v10/webhooks/1200000000000000001/a.b-c_d/messages/@original"
        );
        for (app, token) in [
            ("12/../evil", "t"),
            ("", "t"),
            ("1", ""),
            ("1", "."),
            ("1", ".."),
        ] {
            assert!(reply(app, token).url(&[]).is_err(), "{app} {token}");
        }
        // Whatever the token holds stays one segment under the webhook.
        for token in ["t/../../x", "t?x=1", "t#x", "a=b+c"] {
            let url = reply("1", token).url(&[]).unwrap();
            assert_eq!(url.host_str(), Some("discord.com"), "{token}");
            assert_eq!(url.path_segments().unwrap().count(), 5, "{url}");
            assert!(url.query().is_none() && url.fragment().is_none(), "{url}");
        }
    }
}
