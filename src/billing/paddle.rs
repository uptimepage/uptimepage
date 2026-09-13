//! Paddle Billing behind [`BillingProvider`]: signature check, event mapping,
//! and the handful of API calls the lifecycle makes. Paddle is merchant of
//! record, so tax, invoices and card retries are its; we only learn what it
//! decided and keep the plan in step.

use async_trait::async_trait;
use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Bytes;
use hyper::header::{AUTHORIZATION, CONTENT_TYPE};
use hyper::{Method, Request, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use url::Url;
use uuid::Uuid;

use super::provider::{
    BillingProvider, ChangeTiming, CheckoutRequest, EventKind, PortalLinks, ProviderEvent,
    SubscriptionSnapshot, SubscriptionStatus, WebhookRejected,
};
use crate::api::error::codes;
use crate::auth::mac::hmac_sha256_hex;
use crate::domain::AccountId;
use crate::error::{AppError, Result};
use crate::http_outbound::{OutboundHttpClient, REQUEST_TIMEOUT};

pub const NAME: &str = "paddle";
pub const SIGNATURE_HEADER: &str = "paddle-signature";
/// Paddle's SDKs default to five seconds. Replay is already closed by the
/// event id, so this only has to bound clock drift, and a few minutes costs
/// nothing where five seconds would drop every delivery on a skewed host.
pub const TOLERANCE_SECS: i64 = 5 * 60;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const CUSTOM_DATA_ACCOUNT: &str = "account_id";
const CUSTOM_DATA_TAG: &str = "account_sig";

/// `ts=<unix>;h1=<hex>[;h1=<hex>]` over `"{ts}:{body}"`, HMAC-SHA256 keyed by
/// the endpoint secret as given. More than one `h1` appears during secret
/// rotation; any of them matching is enough.
pub fn verify(secret: &str, header: &str, body: &[u8], now: i64) -> bool {
    let mut ts = None;
    let mut candidates = Vec::new();
    for part in header.split(';') {
        match part.trim().split_once('=') {
            Some(("ts", v)) => ts = v.parse::<i64>().ok(),
            Some(("h1", v)) => candidates.push(v.trim().to_owned()),
            _ => {}
        }
    }
    let Some(ts) = ts else {
        return false;
    };
    if now
        .checked_sub(ts)
        .and_then(i64::checked_abs)
        .is_none_or(|drift| drift > TOLERANCE_SECS)
    {
        return false;
    }
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(ts.to_string().as_bytes());
    mac.update(b":");
    mac.update(body);
    let expected = mac.finalize().into_bytes();
    candidates
        .iter()
        .filter_map(|c| hex::decode(c).ok())
        .any(|candidate| candidate.ct_eq(&expected).into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    Sandbox,
    Live,
}

impl Environment {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sandbox" => Some(Environment::Sandbox),
            "live" => Some(Environment::Live),
            _ => None,
        }
    }

    fn api_base(self) -> &'static str {
        match self {
            Environment::Sandbox => "https://sandbox-api.paddle.com",
            Environment::Live => "https://api.paddle.com",
        }
    }
}

pub struct PaddleProvider {
    api_key: SecretString,
    webhook_secret: SecretString,
    /// Keys the account claim on a checkout. Ours and never rotated with the
    /// endpoint secret, so a rotation strands no checkout in flight.
    checkout_secret: SecretString,
    api_base: &'static str,
    http: OutboundHttpClient,
}

impl PaddleProvider {
    pub fn new(
        environment: Environment,
        api_key: SecretString,
        webhook_secret: SecretString,
        checkout_secret: SecretString,
        http: OutboundHttpClient,
    ) -> Self {
        Self {
            api_key,
            webhook_secret,
            checkout_secret,
            api_base: environment.api_base(),
            http,
        }
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let uri = format!("{}{path}", self.api_base);
        let payload = match &body {
            Some(v) => serde_json::to_vec(v).map_err(|e| AppError::Other(e.into()))?,
            None => Vec::new(),
        };
        let req = Request::builder()
            .method(method)
            .uri(&uri)
            .header(
                AUTHORIZATION,
                format!("Bearer {}", self.api_key.expose_secret()),
            )
            .header("Paddle-Version", "1")
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(payload)))
            .map_err(|e| AppError::Other(anyhow::anyhow!("paddle request: {e}")))?;
        let at = tokio::time::Instant::now() + REQUEST_TIMEOUT;
        let resp = tokio::time::timeout_at(at, self.http.request(req))
            .await
            .map_err(|_| unreachable(path, "no response"))?
            .map_err(|e| unreachable(path, e))?;
        let status = resp.status();
        let bytes = tokio::time::timeout_at(
            at,
            Limited::new(resp.into_body(), MAX_RESPONSE_BYTES).collect(),
        )
        .await
        .map_err(|_| unreachable(path, "body stalled"))?
        .map_err(|e| unreachable(path, format!("read body: {e}")))?
        .to_bytes();
        let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(parsed["data"].clone());
        }
        let code = parsed["error"]["code"].as_str().unwrap_or("unknown");
        let detail = parsed["error"]["detail"]
            .as_str()
            .unwrap_or("no detail given");
        Err(answer_for(status, path, code, detail))
    }

    async fn subscription_call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<SubscriptionSnapshot> {
        let data: SubscriptionData =
            serde_json::from_value(self.call(method, path, body).await?)
                .map_err(|e| AppError::Other(anyhow::anyhow!("paddle {path}: {e}")))?;
        Ok(data.snapshot())
    }
}

#[derive(Deserialize)]
struct Envelope {
    event_id: String,
    event_type: String,
    occurred_at: DateTime<Utc>,
    #[serde(default)]
    data: Value,
}

/// Paddle.js lets any visitor open a checkout carrying custom data of their
/// choosing, so the account id travels with a tag only this server can make.
/// It does not expire: a checkout link paid late must still land, and a
/// subscription once bound answers to its account whatever the tag says.
fn account_tag(secret: &str, account: AccountId) -> String {
    hmac_sha256_hex(
        secret.as_bytes(),
        &[b"checkout-account:", account.0.as_bytes()],
    )
}

fn account_tag_verifies(secret: &str, account: AccountId, tag: &str) -> bool {
    let expected = account_tag(secret, account);
    bool::from(tag.as_bytes().ct_eq(expected.as_bytes()))
}

/// The account id we attached at checkout, carried on the transaction and
/// copied to the subscription it created. Honoured only under its tag.
fn custom_account(secret: &str, event_id: &str, data: &Value) -> Option<AccountId> {
    let custom = data.get("custom_data")?;
    let account = custom
        .get(CUSTOM_DATA_ACCOUNT)
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .map(AccountId)?;
    let tag = custom
        .get(CUSTOM_DATA_TAG)
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !account_tag_verifies(secret, account, tag) {
        tracing::warn!(
            event_id,
            %account,
            "paddle: custom data names an account without our tag"
        );
        return None;
    }
    Some(account)
}

#[derive(Deserialize)]
struct TransactionData {
    #[serde(default)]
    customer_id: Option<String>,
    #[serde(default)]
    subscription_id: Option<String>,
}

#[derive(Deserialize)]
struct Period {
    ends_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct ScheduledChange {
    action: String,
    effective_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct Item {
    #[serde(default = "active")]
    status: String,
    price: Price,
}

fn active() -> String {
    "active".into()
}

#[derive(Deserialize)]
struct Price {
    id: String,
}

#[derive(Deserialize)]
struct SubscriptionData {
    id: String,
    status: SubscriptionStatus,
    customer_id: String,
    #[serde(default)]
    current_billing_period: Option<Period>,
    #[serde(default)]
    scheduled_change: Option<ScheduledChange>,
    #[serde(default)]
    items: Vec<Item>,
    updated_at: DateTime<Utc>,
}

impl SubscriptionData {
    /// Paddle's `updated_at` on both the webhook and the call path, so
    /// snapshots order on one clock. A replaced item stays on as `inactive`.
    fn snapshot(self) -> SubscriptionSnapshot {
        SubscriptionSnapshot {
            subscription_ref: self.id,
            customer_ref: self.customer_id,
            status: self.status,
            price_refs: self
                .items
                .into_iter()
                .filter(|i| matches!(i.status.as_str(), "active" | "trialing"))
                .map(|i| i.price.id)
                .collect(),
            period_end: self.current_billing_period.map(|p| p.ends_at),
            cancel_at: self
                .scheduled_change
                .filter(|c| c.action == "cancel")
                .map(|c| c.effective_at),
            taken_at: self.updated_at,
        }
    }
}

fn map_event(
    secret: &str,
    envelope: Envelope,
) -> std::result::Result<ProviderEvent, WebhookRejected> {
    let malformed = |e: serde_json::Error| WebhookRejected::Malformed(e.to_string());
    let (customer_ref, subscription_ref, kind) = match envelope.event_type.as_str() {
        "transaction.completed" | "transaction.payment_failed" => {
            let t = TransactionData::deserialize(&envelope.data).map_err(malformed)?;
            let kind = if envelope.event_type == "transaction.completed" {
                EventKind::Paid
            } else {
                EventKind::PaymentFailed
            };
            (t.customer_id, t.subscription_id, kind)
        }
        kind if kind.starts_with("subscription.") => {
            let snapshot = SubscriptionData::deserialize(&envelope.data)
                .map_err(malformed)?
                .snapshot();
            (
                Some(snapshot.customer_ref.clone()),
                Some(snapshot.subscription_ref.clone()),
                EventKind::Subscription(snapshot),
            )
        }
        _ => (None, None, EventKind::Other),
    };
    // Noise is applied to nobody, so its claim is not even read.
    let account = match kind {
        EventKind::Other => None,
        _ => custom_account(secret, &envelope.event_id, &envelope.data),
    };
    Ok(ProviderEvent {
        event_id: envelope.event_id,
        event_type: envelope.event_type,
        occurred_at: envelope.occurred_at,
        account,
        customer_ref,
        subscription_ref,
        kind,
    })
}

#[async_trait]
impl BillingProvider for PaddleProvider {
    fn name(&self) -> &'static str {
        NAME
    }

    fn parse_webhook(
        &self,
        headers: &HeaderMap,
        body: &[u8],
        now: DateTime<Utc>,
    ) -> std::result::Result<ProviderEvent, WebhookRejected> {
        let header = headers
            .get(SIGNATURE_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !verify(
            self.webhook_secret.expose_secret(),
            header,
            body,
            now.timestamp(),
        ) {
            return Err(WebhookRejected::Signature);
        }
        let envelope: Envelope =
            serde_json::from_slice(body).map_err(|e| WebhookRejected::Malformed(e.to_string()))?;
        map_event(self.checkout_secret.expose_secret(), envelope)
    }

    async fn checkout(&self, req: CheckoutRequest<'_>) -> Result<Url> {
        let mut body = json!({
            "items": [{ "price_id": req.price_ref, "quantity": 1 }],
            "custom_data": {
                CUSTOM_DATA_ACCOUNT: req.account.0.to_string(),
                CUSTOM_DATA_TAG: account_tag(self.checkout_secret.expose_secret(), req.account),
            },
        });
        if let Some(customer) = req.customer_ref {
            body["customer_id"] = json!(customer);
        }
        let data = self.call(Method::POST, "/transactions", Some(body)).await?;
        let url = data["checkout"]["url"].as_str().ok_or_else(|| {
            AppError::Other(anyhow::anyhow!(
                "paddle transaction carries no checkout url; is the default payment link set?"
            ))
        })?;
        Url::parse(url).map_err(|e| AppError::Other(anyhow::anyhow!("paddle checkout url: {e}")))
    }

    async fn portal(
        &self,
        customer_ref: &str,
        subscription_ref: Option<&str>,
    ) -> Result<PortalLinks> {
        let body = json!({ "subscription_ids": subscription_ref.into_iter().collect::<Vec<_>>() });
        let data = self
            .call(
                Method::POST,
                &format!("/customers/{customer_ref}/portal-sessions"),
                Some(body),
            )
            .await?;
        let link = |v: &Value| v.as_str().and_then(|s| Url::parse(s).ok());
        let overview = link(&data["urls"]["general"]["overview"]).ok_or_else(|| {
            AppError::Other(anyhow::anyhow!(
                "paddle portal session carries no overview url"
            ))
        })?;
        let sub = &data["urls"]["subscriptions"][0];
        Ok(PortalLinks {
            overview,
            update_payment_method: link(&sub["update_subscription_payment_method"]),
            cancel: link(&sub["cancel_subscription"]),
        })
    }

    async fn fetch_subscription(&self, subscription_ref: &str) -> Result<SubscriptionSnapshot> {
        self.subscription_call(
            Method::GET,
            &format!("/subscriptions/{subscription_ref}"),
            None,
        )
        .await
    }

    async fn change_price(
        &self,
        subscription_ref: &str,
        price_ref: &str,
        timing: ChangeTiming,
    ) -> Result<SubscriptionSnapshot> {
        let proration = match timing {
            ChangeTiming::Now => "prorated_immediately",
            ChangeTiming::NextPeriod => "do_not_bill",
        };
        self.subscription_call(
            Method::PATCH,
            &format!("/subscriptions/{subscription_ref}"),
            Some(json!({
                "items": [{ "price_id": price_ref, "quantity": 1 }],
                "proration_billing_mode": proration,
            })),
        )
        .await
    }

    async fn cancel(
        &self,
        subscription_ref: &str,
        timing: ChangeTiming,
    ) -> Result<SubscriptionSnapshot> {
        let effective_from = match timing {
            ChangeTiming::Now => "immediately",
            ChangeTiming::NextPeriod => "next_billing_period",
        };
        self.subscription_call(
            Method::POST,
            &format!("/subscriptions/{subscription_ref}/cancel"),
            Some(json!({ "effective_from": effective_from })),
        )
        .await
    }

    async fn revoke_cancel(&self, subscription_ref: &str) -> Result<SubscriptionSnapshot> {
        self.subscription_call(
            Method::PATCH,
            &format!("/subscriptions/{subscription_ref}"),
            Some(json!({ "scheduled_change": null })),
        )
        .await
    }
}

/// What a failed status means to the caller. Only a subscription's own path
/// says the subscription is gone; a 404 anywhere else (an archived customer's
/// portal) is a refusal like any other 4xx. Rate limited is a "later", not a
/// "no".
fn answer_for(status: StatusCode, path: &str, code: &str, detail: &str) -> AppError {
    let refused = match status.as_u16() {
        404 if path.starts_with("/subscriptions/") => {
            AppError::not_found(codes::SUBSCRIPTION_NOT_FOUND, detail.to_owned())
        }
        429 => return unreachable(path, format!("{status} {code}: {detail}")),
        400..=499 => {
            AppError::conflict(codes::BILLING_PROVIDER_REFUSED, format!("{code}: {detail}"))
        }
        _ => return unreachable(path, format!("{status} {code}: {detail}")),
    };
    tracing::warn!(%status, code, detail, path, "paddle refused the call");
    refused
}

/// Logged in full, answered as something to retry: the caller can do
/// nothing about the provider's silence but wait.
fn unreachable(path: &str, why: impl std::fmt::Display) -> AppError {
    tracing::warn!(path, %why, "paddle gave no usable answer");
    AppError::service_unavailable(
        codes::BILLING_PROVIDER_UNREACHABLE,
        "the payment provider did not answer; try again in a moment",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "pdl_ntfset_01gkpjp8bkm3tm53kdgkx6sms7_6h3qd3uFSi9YCD3OLYAShQI90XTI5vEI";
    const NOW: i64 = 1_700_000_000;

    fn sign(ts: i64, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
        mac.update(format!("{ts}:").as_bytes());
        mac.update(body);
        format!("ts={ts};h1={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn accepts_a_correctly_signed_delivery_and_a_rotation_pair() {
        let body = br#"{"event_type":"subscription.updated"}"#;
        assert!(verify(SECRET, &sign(NOW, body), body, NOW));
        let rotated = format!("{};h1=00ff", sign(NOW, body));
        assert!(verify(SECRET, &rotated, body, NOW + 30));
    }

    #[test]
    fn rejects_tampering_stale_stamps_and_garbage() {
        let body = b"x";
        let good = sign(NOW, body);
        assert!(!verify(SECRET, &good, b"y", NOW));
        assert!(!verify("other", &good, body, NOW));
        assert!(!verify(SECRET, &good, body, NOW + TOLERANCE_SECS + 1));
        assert!(!verify(SECRET, "h1=abcd", body, NOW));
        assert!(!verify(SECRET, "ts=soon;h1=abcd", body, NOW));
        assert!(!verify(SECRET, "", body, NOW));
        for extreme in [i64::MIN, i64::MAX] {
            assert!(!verify(SECRET, &sign(extreme, body), body, NOW));
            assert!(!verify(SECRET, &sign(NOW, body), body, extreme));
        }
    }

    const ACCOUNT: &str = "0198f0e1-0000-7000-8000-000000000000";

    fn account() -> AccountId {
        AccountId(Uuid::parse_str(ACCOUNT).unwrap())
    }

    fn custom_data(tag: &str) -> Value {
        json!({ "account_id": ACCOUNT, "account_sig": tag })
    }

    fn signed_custom_data() -> Value {
        custom_data(&account_tag(SECRET, account()))
    }

    fn envelope(event_type: &str, data: Value) -> Envelope {
        serde_json::from_value(json!({
            "event_id": "evt_01",
            "event_type": event_type,
            "occurred_at": "2026-09-12T10:00:00Z",
            "notification_id": "ntf_01",
            "data": data,
        }))
        .unwrap()
    }

    fn subscription(status: &str, scheduled: Value) -> Value {
        json!({
            "id": "sub_01",
            "status": status,
            "customer_id": "ctm_01",
            "custom_data": signed_custom_data(),
            "current_billing_period": {
                "starts_at": "2026-09-01T00:00:00Z",
                "ends_at": "2026-10-01T00:00:00Z"
            },
            "scheduled_change": scheduled,
            "items": [
                { "status": "inactive", "quantity": 1, "price": { "id": "pri_pro_month" } },
                { "status": "active", "quantity": 1, "price": { "id": "pri_team_month" } }
            ],
            "updated_at": "2026-09-12T09:59:58Z"
        })
    }

    #[test]
    fn a_subscription_event_becomes_a_snapshot_with_only_the_billed_price() {
        let event = map_event(
            SECRET,
            envelope(
                "subscription.updated",
                subscription(
                    "active",
                    json!({ "action": "cancel", "effective_at": "2026-10-01T00:00:00Z", "resume_at": null }),
                ),
            ),
        )
        .unwrap();
        assert_eq!(event.account, Some(account()));
        assert_eq!(event.subscription_ref.as_deref(), Some("sub_01"));
        let EventKind::Subscription(snap) = event.kind else {
            panic!("not a snapshot");
        };
        assert_eq!(snap.status, SubscriptionStatus::Active);
        assert_eq!(snap.price_refs, vec!["pri_team_month"]);
        assert_eq!(
            snap.cancel_at.map(|t| t.to_rfc3339()),
            Some("2026-10-01T00:00:00+00:00".into())
        );
        assert_eq!(
            snap.taken_at.to_rfc3339(),
            "2026-09-12T09:59:58+00:00",
            "ordered on the provider's clock, not the delivery's"
        );
    }

    #[test]
    fn an_account_id_is_honoured_only_under_our_tag() {
        let with = |custom: Value| {
            let mut data = subscription("active", Value::Null);
            data["custom_data"] = custom;
            map_event(SECRET, envelope("subscription.created", data))
                .unwrap()
                .account
        };
        assert_eq!(with(signed_custom_data()), Some(account()));
        assert_eq!(with(json!({ "account_id": ACCOUNT })), None, "no tag");
        assert_eq!(with(custom_data("")), None, "empty tag");
        assert_eq!(
            with(custom_data(&account_tag(SECRET, account()).to_uppercase())),
            None,
            "a tag in another spelling"
        );
        let other = AccountId(Uuid::parse_str("0198f0e1-0000-7000-8000-000000000001").unwrap());
        assert_eq!(
            with(custom_data(&account_tag(SECRET, other))),
            None,
            "a tag minted for another account"
        );
        assert_eq!(
            with(custom_data(&account_tag("other", account()))),
            None,
            "a tag under another secret"
        );
    }

    #[test]
    fn a_pause_is_not_a_cancel_and_an_unknown_status_ends_service() {
        let paused = map_event(
            SECRET,
            envelope(
                "subscription.paused",
                subscription(
                    "paused",
                    json!({ "action": "pause", "effective_at": "2026-10-01T00:00:00Z", "resume_at": null }),
                ),
            ),
        )
        .unwrap();
        let EventKind::Subscription(snap) = paused.kind else {
            panic!("not a snapshot");
        };
        assert_eq!(snap.status, SubscriptionStatus::Paused);
        assert_eq!(snap.cancel_at, None);

        let odd = map_event(
            SECRET,
            envelope(
                "subscription.updated",
                subscription("archived", Value::Null),
            ),
        );
        assert!(
            matches!(odd, Err(WebhookRejected::Malformed(_))),
            "a status we cannot read must not end anyone's service"
        );
    }

    #[test]
    fn transactions_map_to_paid_and_failed_and_the_rest_is_noise() {
        let mut txn = json!({
            "id": "txn_01",
            "status": "completed",
            "customer_id": "ctm_01",
            "subscription_id": "sub_01",
            "custom_data": signed_custom_data()
        });
        let paid = map_event(SECRET, envelope("transaction.completed", txn.clone())).unwrap();
        assert_eq!(paid.kind, EventKind::Paid);
        assert_eq!(paid.account, Some(account()));
        assert_eq!(paid.subscription_ref.as_deref(), Some("sub_01"));
        txn["custom_data"] = json!({ "account_id": "not-a-uuid" });
        let failed = map_event(SECRET, envelope("transaction.payment_failed", txn)).unwrap();
        assert_eq!(failed.kind, EventKind::PaymentFailed);
        assert_eq!(failed.account, None);
        let other = map_event(
            SECRET,
            envelope(
                "transaction.created",
                json!({ "id": "txn_02", "custom_data": signed_custom_data() }),
            ),
        )
        .unwrap();
        assert_eq!(other.kind, EventKind::Other);
        assert_eq!(other.account, None, "noise binds nobody, tagged or not");
        assert_eq!(other.subscription_ref, None);
    }

    #[test]
    fn only_a_subscriptions_own_path_says_it_is_gone() {
        let gone = |path: &str| {
            matches!(
                answer_for(StatusCode::NOT_FOUND, path, "entity_not_found", "no"),
                AppError::NotFound { code, .. } if code == codes::SUBSCRIPTION_NOT_FOUND
            )
        };
        assert!(gone("/subscriptions/sub_01"));
        assert!(gone("/subscriptions/sub_01/cancel"));
        let portal = answer_for(
            StatusCode::NOT_FOUND,
            "/customers/ctm_01/portal-sessions",
            "entity_not_found",
            "no",
        );
        assert!(
            matches!(portal, AppError::Conflict { code, .. } if code == codes::BILLING_PROVIDER_REFUSED),
            "{portal:?}"
        );
        let later = answer_for(
            StatusCode::TOO_MANY_REQUESTS,
            "/subscriptions/sub_01",
            "x",
            "y",
        );
        assert!(
            matches!(later, AppError::ServiceUnavailable { .. }),
            "{later:?}"
        );
        let down = answer_for(StatusCode::BAD_GATEWAY, "/transactions", "x", "y");
        assert!(
            matches!(down, AppError::ServiceUnavailable { .. }),
            "{down:?}"
        );
    }

    #[test]
    fn a_subscription_event_missing_its_fields_is_malformed_not_a_panic() {
        let err = map_event(
            SECRET,
            envelope("subscription.created", json!({ "id": "sub_01" })),
        )
        .unwrap_err();
        assert!(matches!(err, WebhookRejected::Malformed(_)));
    }
}
