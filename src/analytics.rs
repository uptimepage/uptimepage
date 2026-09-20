//! Sign-in funnel events for the self-hosted Umami instance.
//!
//! Only the server knows a login succeeded, and by then the visitor has left
//! every tracked page. Umami keys a session on `(website, ip, user_agent, salt)`
//! and its `/api/send` accepts an `ip`, so forwarding the visitor's own address
//! and `User-Agent` files this under their existing session — closing the funnel
//! without a tracker on any authenticated page.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::Duration;

use axum::http::HeaderMap;
use axum::http::header::{ORIGIN, USER_AGENT};
use serde::Serialize;

use crate::auth::login_audit::LoginMethod;
use crate::auth::magic_link::RedeemedVia;
use crate::http_outbound::OutboundHttpClient;

/// Shared with the marketing site so a visit spanning both hosts is one session.
const WEBSITE_ID: &str = "2ef8ae40-ba4a-40a4-90bf-6d6b2b1eae2e";
const ENDPOINT: &str = "https://analytics.uptimepage.dev/api/send";
const APP_ORIGIN: &str = "https://app.uptimepage.dev";
const HOSTNAME: &str = "app.uptimepage.dev";

/// Attributes the event to the login page, not the callback URL nobody sees.
const EVENT_URL: &str = "/login";

const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// The tracker's website id, or `None` where reporting is not ours to do —
/// self-hosted and dev deployments never match the hosted origin.
pub fn website_id(public_base_url: &str) -> Option<&'static str> {
    (public_base_url.trim().trim_end_matches('/') == APP_ORIGIN).then_some(WEBSITE_ID)
}

/// One completed sign-in, as the funnel sees it.
pub struct Login<'a> {
    pub method: LoginMethod,
    pub new_user: bool,
    pub redirect_after: Option<&'a str>,
    /// Which half of a magic-link mail was used. `None` where the method offers
    /// only one way in.
    pub via: Option<RedeemedVia>,
}

/// Fire-and-forget: a failure warns and is dropped, never touching the session
/// the caller just minted. `new_user` picks the event *name* rather than riding
/// along as a property, because Umami funnel steps match on name — one mixed
/// event would count returning logins as signups.
pub fn track_login(
    client: &OutboundHttpClient,
    public_base_url: &str,
    login: Login<'_>,
    ip: IpAddr,
    headers: &HeaderMap,
) {
    if website_id(public_base_url).is_none() {
        return;
    }
    let Some(method) = method_prop(login.method) else {
        return;
    };
    // No User-Agent, no session to hash against — and Umami drops it as a bot.
    let Some(user_agent) = headers
        .get(USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .filter(|ua| !ua.is_empty())
        .map(str::to_owned)
    else {
        return;
    };

    let body = SendBody {
        kind: "event",
        payload: EventPayload {
            website: WEBSITE_ID,
            hostname: HOSTNAME,
            url: EVENT_URL,
            name: event_name(login.new_user),
            ip: ip.to_string(),
            data: props(method, &login),
        },
    };

    let client = client.clone();
    tokio::spawn(async move {
        let Ok(url) = ENDPOINT.parse() else {
            return;
        };
        let headers = beacon_headers(user_agent);
        let sent = tokio::time::timeout(
            SEND_TIMEOUT,
            crate::http_outbound::post_json_with_headers(&client, &url, &body, &headers),
        )
        .await;
        match sent {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::warn!(error = %err, "analytics send failed (non-fatal)"),
            Err(_) => tracing::warn!("analytics send timed out (non-fatal)"),
        }
    });
}

/// A send made off a browser carries no `Origin` of its own, and the edge
/// turns away beacons from hosts it does not know — keep this in step with
/// the `@foreign_beacon` allowlist in `deployment/Caddyfile`.
fn beacon_headers(user_agent: String) -> BTreeMap<String, String> {
    BTreeMap::from([
        (USER_AGENT.to_string(), user_agent),
        (ORIGIN.to_string(), APP_ORIGIN.to_string()),
    ])
}

/// `via` is absent rather than a filler value: Umami charts every value a
/// property takes, and a third slice reading "none" is not a sign-in method.
fn props(method: &'static str, login: &Login<'_>) -> BTreeMap<&'static str, &'static str> {
    let mut data = BTreeMap::from([("method", method), ("intent", intent(login.redirect_after))]);
    if let Some(via) = login.via {
        data.insert("via", via.as_str());
    }
    data
}

fn event_name(new_user: bool) -> &'static str {
    if new_user {
        "signup-complete"
    } else {
        "login-complete"
    }
}

/// Only the marketing URL box routes to a prefilled create form, so its
/// redirect target is the whole signal. Parsed, not substring-matched:
/// `curl=` contains `url=`.
fn intent(redirect_after: Option<&str>) -> &'static str {
    let carries_url = redirect_after
        .and_then(|r| r.strip_prefix("/targets/new?"))
        .is_some_and(|q| {
            url::form_urlencoded::parse(q.as_bytes()).any(|(k, v)| k == "url" && !v.is_empty())
        });
    if carries_url { "monitor-url" } else { "none" }
}

/// Mirrors the values the login page's browser events send, so both ends of the
/// funnel break down by the same property. `None` never comes from a browser.
fn method_prop(method: LoginMethod) -> Option<&'static str> {
    match method {
        LoginMethod::GithubOauth => Some("github"),
        LoginMethod::GoogleOauth => Some("google"),
        LoginMethod::MicrosoftOauth => Some("microsoft"),
        LoginMethod::GitlabOauth => Some("gitlab"),
        LoginMethod::Passkey => Some("passkey"),
        LoginMethod::MagicLink => Some("magic-link"),
        LoginMethod::ApiToken => None,
    }
}

#[derive(Serialize)]
struct SendBody {
    #[serde(rename = "type")]
    kind: &'static str,
    payload: EventPayload,
}

#[derive(Serialize)]
struct EventPayload {
    website: &'static str,
    hostname: &'static str,
    url: &'static str,
    name: &'static str,
    ip: String,
    data: BTreeMap<&'static str, &'static str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_only_from_the_hosted_app_origin() {
        assert_eq!(website_id(APP_ORIGIN), Some(WEBSITE_ID));
        assert_eq!(website_id("https://app.uptimepage.dev/"), Some(WEBSITE_ID));
        assert_eq!(
            website_id("  https://app.uptimepage.dev  "),
            Some(WEBSITE_ID)
        );
        assert_eq!(website_id("http://localhost:8080"), None);
        assert_eq!(website_id("https://status.acme.example"), None);
        assert_eq!(website_id("https://app.uptimepage.dev.evil.example"), None);
        assert_eq!(website_id("http://app.uptimepage.dev"), None);
    }

    #[test]
    fn beacon_names_the_origin_the_edge_allows() {
        let headers = beacon_headers("Mozilla/5.0".to_string());
        assert_eq!(headers[ORIGIN.as_str()], APP_ORIGIN);
        assert_eq!(headers[USER_AGENT.as_str()], "Mozilla/5.0");
    }

    #[test]
    fn the_edge_allows_the_origin_the_beacon_sends() {
        let caddy =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/deployment/Caddyfile"))
                .expect("read Caddyfile");
        let domain = APP_ORIGIN
            .strip_prefix("https://app.")
            .expect("app origin is a subdomain of the product domain");
        let templated = APP_ORIGIN.replace(domain, "{$UPTIMEPAGE_DOMAIN}");
        assert!(
            caddy.contains(&format!("header Origin {templated}")),
            "the edge must allow {templated}, or every funnel beacon 403s with only a warn log"
        );
    }

    #[test]
    fn the_edge_refuses_replayed_share_tokens() {
        let caddy =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/deployment/Caddyfile"))
                .expect("read Caddyfile");
        assert!(
            caddy.contains("header x-umami-share-token *"),
            "the edge must refuse this header, or an analytics build older than 3.2.0 reads out              every website to anyone holding a public website id"
        );
    }

    #[test]
    fn payload_carries_the_visitor_ip_and_method() {
        let body = SendBody {
            kind: "event",
            payload: EventPayload {
                website: WEBSITE_ID,
                hostname: HOSTNAME,
                url: EVENT_URL,
                name: event_name(true),
                ip: "203.0.113.7".to_string(),
                data: BTreeMap::from([(
                    "method",
                    method_prop(LoginMethod::GithubOauth).expect("oauth is a browser method"),
                )]),
            },
        };
        let json = serde_json::to_value(&body).expect("serializes");
        assert_eq!(json["type"], "event");
        assert_eq!(json["payload"]["name"], "signup-complete");
        assert_eq!(json["payload"]["ip"], "203.0.113.7");
        assert_eq!(json["payload"]["hostname"], HOSTNAME);
        assert_eq!(json["payload"]["data"]["method"], "github");
    }

    #[test]
    fn only_the_emailed_methods_say_which_half_was_used() {
        let sent = |method, via| {
            props(
                method,
                &Login {
                    method: LoginMethod::MagicLink,
                    new_user: false,
                    redirect_after: None,
                    via,
                },
            )
        };
        assert_eq!(
            sent("magic-link", Some(RedeemedVia::Link)).get("via"),
            Some(&"link")
        );
        assert_eq!(
            sent("magic-link", Some(RedeemedVia::Code)).get("via"),
            Some(&"code")
        );
        assert!(!sent("github", None).contains_key("via"));
    }

    #[test]
    fn only_a_create_form_carrying_a_url_counts_as_monitor_intent() {
        assert_eq!(
            intent(Some("/targets/new?kind=http&url=https%3A%2F%2Fa.io%2F")),
            "monitor-url"
        );
        assert_eq!(intent(Some("/targets/new?url=a.io")), "monitor-url");

        for other in [
            "/targets/new",
            "/targets/new?kind=http",
            "/targets/new?url=",
            // Substring matching would read this as a URL.
            "/targets/new?curl=1",
            "/settings/account?url=a.io",
            "/",
        ] {
            assert_eq!(intent(Some(other)), "none", "{other}");
        }
        assert_eq!(intent(None), "none");
    }

    #[test]
    fn returning_login_is_a_distinct_event_name() {
        assert_eq!(event_name(false), "login-complete");
        assert_eq!(method_prop(LoginMethod::MagicLink), Some("magic-link"));
    }

    #[test]
    fn api_token_logins_are_not_funnel_events() {
        assert_eq!(method_prop(LoginMethod::ApiToken), None);
    }
}
