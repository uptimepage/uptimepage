//! The seam between the subscription lifecycle and whoever collects the money.
//!
//! The lifecycle consumes [`ProviderEvent`]s and hands out
//! [`SubscriptionSnapshot`]s; nothing past this file knows a provider's wire
//! shape. A second provider is a second [`BillingProvider`] and its rows in
//! `plan_prices`.

use async_trait::async_trait;
use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::domain::AccountId;
pub use crate::domain::Interval;
use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionStatus {
    Active,
    Trialing,
    PastDue,
    Paused,
    Canceled,
}

impl SubscriptionStatus {
    /// Will be charged again unless ended.
    pub fn is_live(self) -> bool {
        matches!(
            self,
            SubscriptionStatus::Active | SubscriptionStatus::Trialing | SubscriptionStatus::PastDue
        )
    }
}

/// The provider's whole answer to "where does this subscription stand", from
/// a webhook or fetched. `taken_at` orders snapshots against each other, since
/// webhooks arrive in any order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionSnapshot {
    pub subscription_ref: String,
    pub customer_ref: String,
    pub status: SubscriptionStatus,
    /// Every recurring price on the subscription. The lifecycle picks the one
    /// `plan_prices` knows.
    pub price_refs: Vec<String>,
    pub period_end: Option<DateTime<Utc>>,
    /// Set while a cancel is scheduled for the end of the period.
    pub cancel_at: Option<DateTime<Utc>>,
    pub taken_at: DateTime<Utc>,
}

impl SubscriptionSnapshot {
    /// Ended, or booked to end with the period.
    pub fn is_ending(&self) -> bool {
        self.status == SubscriptionStatus::Canceled || self.cancel_at.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    Paid,
    PaymentFailed,
    Subscription(SubscriptionSnapshot),
    /// Acknowledged and recorded, nothing to apply.
    Other,
}

/// One verified webhook, in our terms. `account` is the claim the provider
/// carried back from checkout, already checked by the provider as its own;
/// the lifecycle honours it only for a subscription nobody holds yet, since a
/// bound `subscription_ref` answers to its account whatever the event says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderEvent {
    pub event_id: String,
    pub event_type: String,
    pub occurred_at: DateTime<Utc>,
    pub account: Option<AccountId>,
    pub customer_ref: Option<String>,
    pub subscription_ref: Option<String>,
    pub kind: EventKind,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckoutRequest<'a> {
    pub account: AccountId,
    pub price_ref: &'a str,
    /// Reuses the provider's customer when the account has bought before.
    pub customer_ref: Option<&'a str>,
}

/// Deep links into the provider's own customer portal. Session-bound and
/// short-lived, so they are minted on demand and never stored.
#[derive(Debug, Clone)]
pub struct PortalLinks {
    pub overview: Url,
    pub update_payment_method: Option<Url>,
    pub cancel: Option<Url>,
}

/// For a price, `Now` prorates and charges at once; `NextPeriod` switches
/// without a charge and the renewal collects. For a cancel, `Now` ends it on
/// the spot; `NextPeriod` lets the paid period run out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeTiming {
    Now,
    NextPeriod,
}

#[derive(Debug, thiserror::Error)]
pub enum WebhookRejected {
    #[error("bad signature")]
    Signature,
    #[error("malformed event: {0}")]
    Malformed(String),
}

#[async_trait]
pub trait BillingProvider: Send + Sync {
    fn name(&self) -> &'static str;

    /// Verifies the delivery and maps it. A signature failure is the caller's
    /// cue to answer 403; a malformed body is acknowledged, since a retry
    /// replays the same bytes.
    fn parse_webhook(
        &self,
        headers: &HeaderMap,
        body: &[u8],
        now: DateTime<Utc>,
    ) -> std::result::Result<ProviderEvent, WebhookRejected>;

    /// A checkout the customer completes on the provider's page. The account
    /// id travels with the transaction and comes back on every event.
    async fn checkout(&self, req: CheckoutRequest<'_>) -> Result<Url>;

    async fn portal(
        &self,
        customer_ref: &str,
        subscription_ref: Option<&str>,
    ) -> Result<PortalLinks>;

    async fn fetch_subscription(&self, subscription_ref: &str) -> Result<SubscriptionSnapshot>;

    async fn change_price(
        &self,
        subscription_ref: &str,
        price_ref: &str,
        timing: ChangeTiming,
    ) -> Result<SubscriptionSnapshot>;

    async fn cancel(
        &self,
        subscription_ref: &str,
        timing: ChangeTiming,
    ) -> Result<SubscriptionSnapshot>;

    /// Withdraws a scheduled cancel while the period is still running.
    async fn revoke_cancel(&self, subscription_ref: &str) -> Result<SubscriptionSnapshot>;
}

/// Test double: webhooks are the JSON of a [`ProviderEvent`] under a fixed
/// header, and every API call answers from a subscription table the test
/// fills in. Not reachable from configuration.
pub mod fake {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    pub const SIGNATURE_HEADER: &str = "x-fake-signature";
    pub const SIGNATURE: &str = "valid";

    #[derive(Default)]
    pub struct FakeProvider {
        pub subscriptions: Mutex<HashMap<String, SubscriptionSnapshot>>,
        pub checkout_url: Mutex<Option<String>>,
        pub calls: Mutex<Vec<String>>,
        /// Answers every cancel with a refusal, the snapshot untouched. A
        /// cancel while one is booked is refused regardless, as Paddle does.
        pub cancel_refused: AtomicBool,
        /// Answers every lookup as a timeout would.
        pub fetch_fails: AtomicBool,
        /// Answers every lookup only after a stalled connection's worth of
        /// waiting.
        pub fetch_stalls: AtomicBool,
    }

    impl FakeProvider {
        pub fn with_subscription(self, snapshot: SubscriptionSnapshot) -> Self {
            self.subscriptions
                .lock()
                .expect("fake provider")
                .insert(snapshot.subscription_ref.clone(), snapshot);
            self
        }

        fn note(&self, call: String) {
            self.calls.lock().expect("fake provider").push(call);
        }

        fn update(
            &self,
            subscription_ref: &str,
            change: impl FnOnce(&mut SubscriptionSnapshot),
        ) -> Result<SubscriptionSnapshot> {
            let mut subs = self.subscriptions.lock().expect("fake provider");
            let snapshot = subs.get_mut(subscription_ref).ok_or_else(|| {
                crate::error::AppError::not_found(
                    crate::api::error::codes::SUBSCRIPTION_NOT_FOUND,
                    "no such subscription",
                )
            })?;
            change(snapshot);
            snapshot.taken_at = Utc::now();
            Ok(snapshot.clone())
        }
    }

    #[async_trait]
    impl BillingProvider for FakeProvider {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn parse_webhook(
            &self,
            headers: &HeaderMap,
            body: &[u8],
            _now: DateTime<Utc>,
        ) -> std::result::Result<ProviderEvent, WebhookRejected> {
            if headers.get(SIGNATURE_HEADER).and_then(|v| v.to_str().ok()) != Some(SIGNATURE) {
                return Err(WebhookRejected::Signature);
            }
            serde_json::from_slice(body).map_err(|e| WebhookRejected::Malformed(e.to_string()))
        }

        async fn checkout(&self, req: CheckoutRequest<'_>) -> Result<Url> {
            self.note(format!("checkout:{}:{}", req.account.0, req.price_ref));
            let url = self
                .checkout_url
                .lock()
                .expect("fake provider")
                .clone()
                .unwrap_or_else(|| format!("https://pay.example.test/?_ptxn={}", req.price_ref));
            Ok(url.parse().expect("fake checkout url"))
        }

        async fn portal(
            &self,
            customer_ref: &str,
            subscription_ref: Option<&str>,
        ) -> Result<PortalLinks> {
            self.note(format!("portal:{customer_ref}"));
            let base = format!("https://portal.example.test/{customer_ref}");
            Ok(PortalLinks {
                overview: base.parse().expect("fake portal url"),
                update_payment_method: subscription_ref
                    .map(|s| format!("{base}/{s}/payment").parse().expect("fake url")),
                cancel: subscription_ref
                    .map(|s| format!("{base}/{s}/cancel").parse().expect("fake url")),
            })
        }

        async fn fetch_subscription(&self, subscription_ref: &str) -> Result<SubscriptionSnapshot> {
            self.note(format!("fetch:{subscription_ref}"));
            if self.fetch_stalls.load(Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
            if self.fetch_fails.load(Ordering::Relaxed) {
                return Err(crate::error::AppError::service_unavailable(
                    crate::api::error::codes::BILLING_PROVIDER_UNREACHABLE,
                    "fake provider: no response",
                ));
            }
            self.update(subscription_ref, |_| {})
        }

        async fn change_price(
            &self,
            subscription_ref: &str,
            price_ref: &str,
            timing: ChangeTiming,
        ) -> Result<SubscriptionSnapshot> {
            self.note(format!("change:{subscription_ref}:{price_ref}:{timing:?}"));
            self.update(subscription_ref, |s| {
                s.price_refs = vec![price_ref.to_owned()]
            })
        }

        async fn cancel(
            &self,
            subscription_ref: &str,
            timing: ChangeTiming,
        ) -> Result<SubscriptionSnapshot> {
            self.note(format!("cancel:{subscription_ref}:{timing:?}"));
            let booked = self
                .subscriptions
                .lock()
                .expect("fake provider")
                .get(subscription_ref)
                .is_some_and(|s| s.cancel_at.is_some());
            if self.cancel_refused.load(Ordering::Relaxed) || booked {
                return Err(crate::error::AppError::conflict(
                    crate::api::error::codes::BILLING_PROVIDER_REFUSED,
                    "fake provider: cancel refused",
                ));
            }
            self.update(subscription_ref, |s| match timing {
                ChangeTiming::Now => {
                    s.status = SubscriptionStatus::Canceled;
                    s.cancel_at = None;
                }
                ChangeTiming::NextPeriod => s.cancel_at = s.period_end,
            })
        }

        async fn revoke_cancel(&self, subscription_ref: &str) -> Result<SubscriptionSnapshot> {
            self.note(format!("revoke_cancel:{subscription_ref}"));
            self.update(subscription_ref, |s| s.cancel_at = None)
        }
    }
}
