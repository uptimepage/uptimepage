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
use crate::observability::metrics::names;

pub const NAME: &str = "paddle";
pub const SIGNATURE_HEADER: &str = "paddle-signature";
/// Paddle's SDKs default to five seconds. Replay is already closed by the
/// event id, so this only has to bound clock drift, and a few minutes costs
/// nothing where five seconds would drop every delivery on a skewed host.
pub const TOLERANCE_SECS: i64 = 5 * 60;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// Before the one retry of a call that failed, long enough for a rate
/// limit or a lock the previous call left to clear.
const RETRY_PAUSE: std::time::Duration = std::time::Duration::from_secs(1);
/// A period end this close is a renewal about to run, which a swap would
/// race.
const RENEWAL_MARGIN: chrono::Duration = chrono::Duration::minutes(1);
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
    api_base: String,
    http: OutboundHttpClient,
    retry_pause: std::time::Duration,
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
            api_base: environment.api_base().to_owned(),
            http,
            retry_pause: RETRY_PAUSE,
        }
    }

    #[cfg(test)]
    fn at(api_base: String, http: OutboundHttpClient) -> Self {
        Self {
            api_key: SecretString::from("key"),
            webhook_secret: SecretString::from("secret"),
            checkout_secret: SecretString::from("checkout"),
            api_base,
            http,
            retry_pause: std::time::Duration::ZERO,
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

    /// A look that is tried twice, for a state a call may have changed
    /// without answering.
    async fn look_again(&self, subscription_ref: &str) -> Result<SubscriptionSnapshot> {
        if let Ok(seen) = self.fetch_subscription(subscription_ref).await {
            return Ok(seen);
        }
        tokio::time::sleep(self.retry_pause).await;
        self.fetch_subscription(subscription_ref).await
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

/// Saving a card runs a zero-value transaction that completes or fails like
/// any other; it settles nothing, so it is neither a payment nor a missed one.
const CARD_CHANGE_ORIGIN: &str = "subscription_payment_method_change";

#[derive(Deserialize)]
struct TransactionData {
    #[serde(default)]
    customer_id: Option<String>,
    #[serde(default)]
    subscription_id: Option<String>,
    #[serde(default)]
    origin: Option<String>,
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
            if t.origin.as_deref() == Some(CARD_CHANGE_ORIGIN) {
                (None, None, EventKind::Other)
            } else {
                let kind = if envelope.event_type == "transaction.completed" {
                    EventKind::Paid
                } else {
                    EventKind::PaymentFailed
                };
                (t.customer_id, t.subscription_id, kind)
            }
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

    /// Paddle swaps the item at once either way; `NextPeriod` only tells it
    /// not to bill. A price on another cadence also restarts the billing
    /// period from now, which would push the next charge a whole cadence
    /// out (or pull it in) for nothing paid, so the period end is read
    /// first and put back when the swap has moved it, and only then, so a
    /// period Paddle holds differently from the row is left alone. It has
    /// to be a second request: Paddle answers `items` with `next_billed_at`
    /// in one with 400 "not possible to change items and billing date in
    /// the same request", and only `prorated_immediately`,
    /// `full_immediately` and `do_not_bill` are allowed when the billing
    /// cycle changes. The date put back is the caller's row's while still
    /// ahead, since Paddle's own is the one a swap that was never put back
    /// has moved; Paddle's is taken when the row's is over. A period end
    /// about to pass is a renewal in flight, which the swap would race, so
    /// the change is refused until it settles. A swap that fails to answer
    /// is checked against what Paddle now holds, since a call can land and
    /// still not answer; one that cannot be checked either is counted, as
    /// it may have moved the date with nobody to put it back. Once the swap
    /// is in, a date that will not go back is counted for someone to set by
    /// hand, and the answer is the swap with the period end the customer
    /// paid through, as retrying cannot put it back and the move must still
    /// land on that date.
    async fn change_price(
        &self,
        subscription_ref: &str,
        price_ref: &str,
        timing: ChangeTiming,
        paid_until: Option<DateTime<Utc>>,
    ) -> Result<SubscriptionSnapshot> {
        let path = format!("/subscriptions/{subscription_ref}");
        let (proration, before) = match timing {
            ChangeTiming::Now => ("prorated_immediately", None),
            ChangeTiming::NextPeriod => {
                let end = self.fetch_subscription(subscription_ref).await?.period_end;
                if end.is_some_and(|at| at <= Utc::now() + RENEWAL_MARGIN) {
                    return Err(AppError::conflict(
                        codes::SUBSCRIPTION_STATE,
                        "the subscription is renewing right now; try again in a minute",
                    ));
                }
                ("do_not_bill", end)
            }
        };
        let paid_until = before.map(|end| paid_until.unwrap_or(end));
        let swapped = self
            .subscription_call(
                Method::PATCH,
                &path,
                Some(json!({
                    "items": [{ "price_id": price_ref, "quantity": 1 }],
                    "proration_billing_mode": proration,
                })),
            )
            .await;
        let changed = match swapped {
            Ok(changed) => changed,
            Err(err @ AppError::ServiceUnavailable { .. }) => {
                match self.look_again(subscription_ref).await {
                    Ok(seen) if seen.price_refs == [price_ref] => seen,
                    Ok(_) => return Err(err),
                    Err(_) => {
                        if let Some(at) = paid_until {
                            metrics::counter!(names::BILLING_PROVIDER_DATE_FAILED).increment(1);
                            tracing::error!(
                                subscription_ref,
                                %at,
                                error = %err,
                                "paddle: billing date left moved by a price change if it landed unanswered; check and put it back by hand"
                            );
                        }
                        return Err(err);
                    }
                }
            }
            Err(err) => return Err(err),
        };
        let moved = changed.period_end != before;
        let Some(at) = paid_until.filter(|at| moved && changed.period_end != Some(*at)) else {
            return Ok(changed);
        };
        let pin = json!({ "next_billed_at": at, "proration_billing_mode": "do_not_bill" });
        let mut answer = self
            .subscription_call(Method::PATCH, &path, Some(pin.clone()))
            .await;
        if answer.as_ref().is_err_and(worth_another_try) {
            tokio::time::sleep(self.retry_pause).await;
            answer = self
                .subscription_call(Method::PATCH, &path, Some(pin))
                .await;
        }
        let why = match answer {
            Ok(pinned) if pinned.period_end == Some(at) => return Ok(pinned),
            Ok(pinned) => format!("answered with {:?}", pinned.period_end),
            Err(err) => err.to_string(),
        };
        if let Ok(seen) = self.fetch_subscription(subscription_ref).await
            && seen.period_end == Some(at)
        {
            return Ok(seen);
        }
        metrics::counter!(names::BILLING_PROVIDER_DATE_FAILED).increment(1);
        tracing::error!(
            subscription_ref,
            %at,
            why,
            "paddle: billing date left moved by a price change; put it back by hand"
        );
        Ok(SubscriptionSnapshot {
            period_end: Some(at),
            ..changed
        })
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

/// No answer, or a lock the previous call left (`subscription_locked_*`)
/// that clears on its own; a refusal proper answers the same way again.
fn worth_another_try(err: &AppError) -> bool {
    match err {
        AppError::ServiceUnavailable { .. } => true,
        AppError::Conflict { message, .. } => message.starts_with("subscription_locked"),
        _ => false,
    }
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
    fn a_card_change_settles_nothing() {
        let txn = json!({
            "id": "txn_01",
            "status": "completed",
            "origin": CARD_CHANGE_ORIGIN,
            "customer_id": "ctm_01",
            "subscription_id": "sub_01",
            "custom_data": signed_custom_data()
        });
        for event_type in ["transaction.completed", "transaction.payment_failed"] {
            let saved = map_event(SECRET, envelope(event_type, txn.clone())).unwrap();
            assert_eq!(saved.kind, EventKind::Other, "{event_type}");
            assert_eq!(saved.account, None);
            assert_eq!(saved.subscription_ref, None);
        }
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

    /// Answers each request from the script in order and keeps what was
    /// asked: `METHOD /path` and the JSON body.
    async fn scripted_api(
        script: Vec<(StatusCode, Value)>,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
    ) {
        use std::sync::{Arc, Mutex};
        let asked: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
        let script = Arc::new(Mutex::new(script.into_iter()));
        let seen = asked.clone();
        let app = axum::Router::new().fallback(
            move |method: Method, uri: axum::http::Uri, body: String| {
                let seen = seen.clone();
                let script = script.clone();
                async move {
                    let body: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    seen.lock()
                        .unwrap()
                        .push((format!("{method} {}", uri.path()), body));
                    let (status, answer) = script
                        .lock()
                        .unwrap()
                        .next()
                        .unwrap_or((StatusCode::INTERNAL_SERVER_ERROR, json!({})));
                    (status, axum::Json(answer))
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), asked)
    }

    fn sub(price: &str, ends_at: DateTime<Utc>) -> Value {
        json!({ "data": {
            "id": "sub_01",
            "status": "active",
            "customer_id": "ctm_01",
            "current_billing_period": { "starts_at": "2026-09-14T12:00:00Z", "ends_at": ends_at },
            "scheduled_change": null,
            "items": [{ "status": "active", "price": { "id": price } }],
            "updated_at": "2026-09-14T12:30:00Z"
        }})
    }

    fn from_now(ahead: chrono::Duration) -> DateTime<Utc> {
        use chrono::Timelike;
        (Utc::now() + ahead).with_nanosecond(0).unwrap()
    }

    fn days_from_now(days: i64) -> DateTime<Utc> {
        from_now(chrono::Duration::days(days))
    }

    fn provider_at(base: String) -> PaddleProvider {
        let http = crate::http_outbound::build_outbound_client(
            crate::security::SsrfGuard::relaxed_for_tests(),
        );
        PaddleProvider::at(base, http)
    }

    fn down() -> (StatusCode, Value) {
        (
            StatusCode::BAD_GATEWAY,
            json!({ "error": { "code": "internal", "detail": "x" } }),
        )
    }

    fn refused() -> (StatusCode, Value) {
        (
            StatusCode::BAD_REQUEST,
            json!({ "error": { "code": "bad_request", "detail": "no" } }),
        )
    }

    fn locked() -> (StatusCode, Value) {
        (
            StatusCode::CONFLICT,
            json!({ "error": { "code": "subscription_locked_processing", "detail": "wait" } }),
        )
    }

    #[tokio::test]
    async fn a_price_on_another_cadence_keeps_the_paid_period_end() {
        let (paid, moved) = (days_from_now(29), days_from_now(365));
        let (base, asked) = scripted_api(vec![
            (StatusCode::OK, sub("pri_team_month", paid)),
            (StatusCode::OK, sub("pri_pro_year", moved)),
            (StatusCode::OK, sub("pri_pro_year", paid)),
        ])
        .await;
        let snapshot = provider_at(base)
            .change_price(
                "sub_01",
                "pri_pro_year",
                ChangeTiming::NextPeriod,
                Some(paid),
            )
            .await
            .unwrap();

        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 3, "read, change, pin: {asked:?}");
        assert_eq!(asked[0].0, "GET /subscriptions/sub_01");
        assert_eq!(asked[1].0, "PATCH /subscriptions/sub_01");
        assert_eq!(asked[1].1["proration_billing_mode"], "do_not_bill");
        assert_eq!(asked[2].0, "PATCH /subscriptions/sub_01");
        assert_eq!(
            asked[2].1,
            json!({ "next_billed_at": paid, "proration_billing_mode": "do_not_bill" })
        );
        assert_eq!(snapshot.period_end, Some(paid));
        assert_eq!(snapshot.price_refs, vec!["pri_pro_year"]);
    }

    #[tokio::test]
    async fn the_date_put_back_is_the_rows_not_the_one_paddle_held() {
        let (paid, drifted, moved) = (days_from_now(29), days_from_now(60), days_from_now(365));
        let (base, asked) = scripted_api(vec![
            (StatusCode::OK, sub("pri_team_month", drifted)),
            (StatusCode::OK, sub("pri_pro_year", moved)),
            (StatusCode::OK, sub("pri_pro_year", paid)),
        ])
        .await;
        let snapshot = provider_at(base)
            .change_price(
                "sub_01",
                "pri_pro_year",
                ChangeTiming::NextPeriod,
                Some(paid),
            )
            .await
            .unwrap();
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 3, "read, change, pin: {asked:?}");
        assert_eq!(asked[2].1["next_billed_at"], json!(paid));
        assert_eq!(snapshot.period_end, Some(paid));
    }

    #[tokio::test]
    async fn a_row_whose_period_is_over_has_paddle_asked_for_it_first() {
        let (paid, moved) = (days_from_now(29), days_from_now(365));
        let (base, asked) = scripted_api(vec![
            (StatusCode::OK, sub("pri_team_month", paid)),
            (StatusCode::OK, sub("pri_pro_year", moved)),
            (StatusCode::OK, sub("pri_pro_year", paid)),
        ])
        .await;
        let snapshot = provider_at(base)
            .change_price("sub_01", "pri_pro_year", ChangeTiming::NextPeriod, None)
            .await
            .unwrap();
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 3, "read, change, pin: {asked:?}");
        assert_eq!(asked[2].1["next_billed_at"], json!(paid));
        assert_eq!(snapshot.period_end, Some(paid));
    }

    #[tokio::test]
    async fn a_price_on_the_same_cadence_keeps_its_period_and_needs_no_pin() {
        let paid = days_from_now(29);
        let (base, asked) = scripted_api(vec![
            (StatusCode::OK, sub("pri_team_month", paid)),
            (StatusCode::OK, sub("pri_pro_month", paid)),
        ])
        .await;
        let snapshot = provider_at(base)
            .change_price(
                "sub_01",
                "pri_pro_month",
                ChangeTiming::NextPeriod,
                Some(paid),
            )
            .await
            .unwrap();
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 2, "read, change: {asked:?}");
        assert_eq!(asked[1].1["proration_billing_mode"], "do_not_bill");
        assert_eq!(snapshot.period_end, Some(paid));
    }

    #[tokio::test]
    async fn a_period_paddle_holds_differently_is_left_alone_when_the_swap_kept_it() {
        let (paid, drifted) = (days_from_now(29), days_from_now(60));
        let (base, asked) = scripted_api(vec![
            (StatusCode::OK, sub("pri_team_month", drifted)),
            (StatusCode::OK, sub("pri_pro_month", drifted)),
        ])
        .await;
        let snapshot = provider_at(base)
            .change_price(
                "sub_01",
                "pri_pro_month",
                ChangeTiming::NextPeriod,
                Some(paid),
            )
            .await
            .unwrap();
        assert_eq!(asked.lock().unwrap().len(), 2, "no pin");
        assert_eq!(snapshot.period_end, Some(drifted));
    }

    #[tokio::test]
    async fn an_immediate_change_that_landed_without_an_answer_is_found_by_asking() {
        let (base, asked) = scripted_api(vec![
            down(),
            (StatusCode::OK, sub("pri_team_year", days_from_now(365))),
        ])
        .await;
        let snapshot = provider_at(base)
            .change_price("sub_01", "pri_team_year", ChangeTiming::Now, None)
            .await
            .unwrap();
        assert_eq!(asked.lock().unwrap().len(), 2);
        assert_eq!(snapshot.price_refs, vec!["pri_team_year"]);
    }

    #[tokio::test]
    async fn an_immediate_change_is_one_prorated_call() {
        let (base, asked) = scripted_api(vec![(
            StatusCode::OK,
            sub("pri_team_year", days_from_now(365)),
        )])
        .await;
        provider_at(base)
            .change_price(
                "sub_01",
                "pri_team_year",
                ChangeTiming::Now,
                Some(days_from_now(29)),
            )
            .await
            .unwrap();
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].1["proration_billing_mode"], "prorated_immediately");
    }

    #[tokio::test]
    async fn a_pin_that_fails_once_is_tried_again() {
        let (paid, moved) = (days_from_now(29), days_from_now(365));
        for first in [down(), locked()] {
            let (base, asked) = scripted_api(vec![
                (StatusCode::OK, sub("pri_team_month", paid)),
                (StatusCode::OK, sub("pri_pro_year", moved)),
                first,
                (StatusCode::OK, sub("pri_pro_year", paid)),
            ])
            .await;
            let snapshot = provider_at(base)
                .change_price(
                    "sub_01",
                    "pri_pro_year",
                    ChangeTiming::NextPeriod,
                    Some(paid),
                )
                .await
                .unwrap();
            assert_eq!(asked.lock().unwrap().len(), 4);
            assert_eq!(snapshot.period_end, Some(paid));
        }
    }

    #[tokio::test]
    async fn a_pin_refused_outright_is_not_tried_again_but_checked() {
        let (paid, moved) = (days_from_now(29), days_from_now(365));
        let (base, asked) = scripted_api(vec![
            (StatusCode::OK, sub("pri_team_month", paid)),
            (StatusCode::OK, sub("pri_pro_year", moved)),
            refused(),
            (StatusCode::OK, sub("pri_pro_year", moved)),
        ])
        .await;
        let snapshot = provider_at(base)
            .change_price(
                "sub_01",
                "pri_pro_year",
                ChangeTiming::NextPeriod,
                Some(paid),
            )
            .await
            .unwrap();
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 4, "read, change, pin, look: {asked:?}");
        assert_eq!(asked[3].0, "GET /subscriptions/sub_01");
        assert_eq!(snapshot.period_end, Some(paid));
    }

    #[tokio::test]
    async fn a_pin_that_keeps_failing_is_checked_counted_and_the_swap_stands() {
        let (paid, moved) = (days_from_now(29), days_from_now(365));
        let (base, asked) = scripted_api(vec![
            (StatusCode::OK, sub("pri_team_month", paid)),
            (StatusCode::OK, sub("pri_pro_year", moved)),
            down(),
            down(),
            (StatusCode::OK, sub("pri_pro_year", moved)),
        ])
        .await;
        let snapshot = provider_at(base)
            .change_price(
                "sub_01",
                "pri_pro_year",
                ChangeTiming::NextPeriod,
                Some(paid),
            )
            .await
            .unwrap();
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 5, "read, change, pin, pin, look: {asked:?}");
        assert_eq!(asked[4].0, "GET /subscriptions/sub_01");
        assert_eq!(snapshot.price_refs, vec!["pri_pro_year"]);
        assert_eq!(
            snapshot.period_end,
            Some(paid),
            "the swap is in force and the move lands on the date paid through; Paddle's is the operator's to fix"
        );
    }

    #[tokio::test]
    async fn a_pin_answered_with_another_date_is_checked_and_counted() {
        let (paid, moved) = (days_from_now(29), days_from_now(365));
        let (base, asked) = scripted_api(vec![
            (StatusCode::OK, sub("pri_team_month", paid)),
            (StatusCode::OK, sub("pri_pro_year", moved)),
            (StatusCode::OK, sub("pri_pro_year", moved)),
            (StatusCode::OK, sub("pri_pro_year", moved)),
        ])
        .await;
        let snapshot = provider_at(base)
            .change_price(
                "sub_01",
                "pri_pro_year",
                ChangeTiming::NextPeriod,
                Some(paid),
            )
            .await
            .unwrap();
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 4, "read, change, pin, look: {asked:?}");
        assert_eq!(asked[3].0, "GET /subscriptions/sub_01");
        assert_eq!(snapshot.period_end, Some(paid));
    }

    #[tokio::test]
    async fn a_renewal_in_flight_or_about_to_run_refuses_the_change_before_the_swap() {
        for end in [days_from_now(-1), from_now(chrono::Duration::seconds(30))] {
            let (base, asked) =
                scripted_api(vec![(StatusCode::OK, sub("pri_team_month", end))]).await;
            let err = provider_at(base)
                .change_price(
                    "sub_01",
                    "pri_pro_year",
                    ChangeTiming::NextPeriod,
                    Some(days_from_now(29)),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(&err, AppError::Conflict { code, .. } if *code == codes::SUBSCRIPTION_STATE),
                "{err:?}"
            );
            assert_eq!(asked.lock().unwrap().len(), 1, "read only");
        }
    }

    #[tokio::test]
    async fn a_pin_that_landed_without_an_answer_is_found_by_asking() {
        let (paid, moved) = (days_from_now(29), days_from_now(365));
        let (base, asked) = scripted_api(vec![
            (StatusCode::OK, sub("pri_team_month", paid)),
            (StatusCode::OK, sub("pri_pro_year", moved)),
            down(),
            refused(),
            (StatusCode::OK, sub("pri_pro_year", paid)),
        ])
        .await;
        let snapshot = provider_at(base)
            .change_price(
                "sub_01",
                "pri_pro_year",
                ChangeTiming::NextPeriod,
                Some(paid),
            )
            .await
            .unwrap();
        assert_eq!(asked.lock().unwrap().len(), 5);
        assert_eq!(snapshot.period_end, Some(paid));
    }

    #[tokio::test]
    async fn an_item_change_that_landed_without_an_answer_is_still_pinned() {
        let (paid, moved) = (days_from_now(29), days_from_now(365));
        let (base, asked) = scripted_api(vec![
            (StatusCode::OK, sub("pri_team_month", paid)),
            down(),
            (StatusCode::OK, sub("pri_pro_year", moved)),
            (StatusCode::OK, sub("pri_pro_year", paid)),
        ])
        .await;
        let snapshot = provider_at(base)
            .change_price(
                "sub_01",
                "pri_pro_year",
                ChangeTiming::NextPeriod,
                Some(paid),
            )
            .await
            .unwrap();
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 4, "read, change, look, pin: {asked:?}");
        assert_eq!(asked[3].1["next_billed_at"], json!(paid));
        assert_eq!(snapshot.period_end, Some(paid));
    }

    #[tokio::test]
    async fn an_item_change_neither_answered_nor_confirmed_is_looked_for_twice_then_the_callers_error()
     {
        let (base, asked) = scripted_api(vec![
            (StatusCode::OK, sub("pri_team_month", days_from_now(29))),
            down(),
            down(),
            down(),
        ])
        .await;
        let err = provider_at(base)
            .change_price(
                "sub_01",
                "pri_pro_year",
                ChangeTiming::NextPeriod,
                Some(days_from_now(29)),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, AppError::ServiceUnavailable { .. }),
            "{err:?}"
        );
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 4, "read, change, look, look: {asked:?}");
        assert_eq!(asked[3].0, "GET /subscriptions/sub_01");
    }

    #[tokio::test]
    async fn an_item_change_refused_outright_is_the_callers_error() {
        let (base, asked) = scripted_api(vec![
            (StatusCode::OK, sub("pri_team_month", days_from_now(29))),
            refused(),
        ])
        .await;
        let err = provider_at(base)
            .change_price(
                "sub_01",
                "pri_pro_year",
                ChangeTiming::NextPeriod,
                Some(days_from_now(29)),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Conflict { .. }), "{err:?}");
        assert_eq!(asked.lock().unwrap().len(), 2, "a refusal is not re-read");
    }
}
