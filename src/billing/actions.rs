//! What the account owner can do to their own subscription. Each action
//! checks the account's state, makes the one provider call it needs, and
//! applies the provider's answer through the same lifecycle a webhook would.

use serde_json::json;
use sqlx::PgPool;
use url::Url;

use super::lifecycle::{Billing, cancel_despite_refusal, smaller};
use super::provider::{ChangeTiming, CheckoutRequest, SubscriptionSnapshot, SubscriptionStatus};
use crate::api::error::codes;
use crate::domain::{AccountId, BillingStatus, Interval, Subscription};
use crate::error::{AppError, Result};
use crate::quotas::QuotaService;
use crate::storage::billing_events as ledger;
use crate::storage::subscriptions as store;

const ALREADY_LIVE: &str = "the account already has a subscription; change its plan instead";

impl Billing {
    pub async fn subscription(&self, pool: &PgPool, account: AccountId) -> Result<Subscription> {
        store::get(pool, account)
            .await?
            .ok_or_else(|| AppError::not_found(codes::ACCOUNT_NOT_FOUND, "account not found"))
    }

    /// A checkout for an account with no live subscription. A live one is
    /// changed in place instead, so the customer never ends up with two.
    pub async fn checkout(
        &self,
        pool: &PgPool,
        quotas: &QuotaService,
        account: AccountId,
        plan_id: &str,
        interval: Interval,
    ) -> Result<Url> {
        let sub = self.subscription(pool, account).await?;
        let refused = match self.lingering(pool, quotas, &sub).await? {
            Some(seen) => match seen.status {
                SubscriptionStatus::Canceled => None,
                SubscriptionStatus::PastDue => Some(
                    "the last subscription is still being settled at the provider; \
                     update the payment method or wait for it to end",
                ),
                SubscriptionStatus::Paused => Some(
                    "the last subscription is paused at the provider; \
                     resume or cancel it in the portal first",
                ),
                SubscriptionStatus::Active | SubscriptionStatus::Trialing => Some(ALREADY_LIVE),
            },
            None => matches!(sub.status, BillingStatus::Active | BillingStatus::PastDue)
                .then_some(ALREADY_LIVE),
        };
        if let Some(why) = refused {
            return Err(AppError::conflict(codes::SUBSCRIPTION_STATE, why));
        }
        let price_ref = self.price_for(pool, plan_id, interval).await?;
        let url = self
            .provider
            .checkout(CheckoutRequest {
                account,
                price_ref: &price_ref,
                customer_ref: sub.customer_ref.as_deref(),
            })
            .await?;
        let mut tx = pool.begin().await.map_err(|e| AppError::Other(e.into()))?;
        ledger::record_tx(
            &mut tx,
            account,
            ledger::CHECKOUT_STARTED,
            json!({ "plan": plan_id, "interval": interval.as_db_str() }),
        )
        .await?;
        tx.commit().await.map_err(|e| AppError::Other(e.into()))?;
        Ok(url)
    }

    /// The provider's portal for the account's customer: invoices, card,
    /// cancel. Only an account that has bought before has one.
    pub async fn portal(&self, pool: &PgPool, account: AccountId) -> Result<Url> {
        let sub = self.subscription(pool, account).await?;
        let Some(customer) = sub.customer_ref.as_deref() else {
            return Err(AppError::conflict(
                codes::SUBSCRIPTION_STATE,
                "the account has no billing history yet",
            ));
        };
        Ok(self
            .provider
            .portal(customer, sub.subscription_ref.as_deref())
            .await?
            .overview)
    }

    /// Only a strictly bigger plan bills now. A return to the plan already
    /// paid for, or a cadence change, must not be prorated on top of a period
    /// the customer already paid at that price.
    pub async fn change_plan(
        &self,
        pool: &PgPool,
        quotas: &QuotaService,
        account: AccountId,
        plan_id: &str,
        interval: Interval,
    ) -> Result<Subscription> {
        let sub = self.subscription(pool, account).await?;
        let subscription_ref = live_subscription_ref(&sub)?;
        if booked_to_end(&sub) {
            return Err(AppError::conflict(
                codes::SUBSCRIPTION_STATE,
                "a cancel is booked; withdraw it before changing the plan",
            ));
        }
        let price_ref = self.price_for(pool, plan_id, interval).await?;
        let mut conn = pool
            .acquire()
            .await
            .map_err(|e| AppError::Other(e.into()))?;
        let timing = if plan_id == sub.plan_id || smaller(&mut conn, plan_id, &sub.plan_id).await? {
            ChangeTiming::NextPeriod
        } else {
            ChangeTiming::Now
        };
        drop(conn);
        let snapshot = self
            .provider
            .change_price(subscription_ref, &price_ref, timing)
            .await?;
        self.apply_snapshot(pool, quotas, account, snapshot).await?;
        self.subscription(pool, account).await
    }

    pub async fn cancel(
        &self,
        pool: &PgPool,
        quotas: &QuotaService,
        account: AccountId,
    ) -> Result<Subscription> {
        let sub = self.subscription(pool, account).await?;
        let subscription_ref = live_subscription_ref(&sub)?;
        let snapshot = cancel_despite_refusal(
            self.provider.as_ref(),
            subscription_ref,
            ChangeTiming::NextPeriod,
        )
        .await?;
        self.apply_snapshot(pool, quotas, account, snapshot).await?;
        self.subscription(pool, account).await
    }

    /// Paid-up ends at the period boundary so a change of heart inside the
    /// deletion grace loses nothing; unpaid ends at once so the provider
    /// stops retrying the card.
    pub async fn end_for_deletion(
        &self,
        pool: &PgPool,
        quotas: &QuotaService,
        account: AccountId,
    ) -> Result<()> {
        let sub = self.subscription(pool, account).await?;
        let Some(subscription_ref) = sub.subscription_ref.as_deref() else {
            return Ok(());
        };
        let timing = match sub.status {
            BillingStatus::Active => ChangeTiming::NextPeriod,
            BillingStatus::PastDue => ChangeTiming::Now,
            BillingStatus::None => return Ok(()),
            BillingStatus::Canceled => {
                match self.lingering(pool, quotas, &sub).await?.map(|s| s.status) {
                    None | Some(SubscriptionStatus::Canceled) => return Ok(()),
                    Some(SubscriptionStatus::Active | SubscriptionStatus::Trialing) => {
                        ChangeTiming::NextPeriod
                    }
                    Some(SubscriptionStatus::PastDue | SubscriptionStatus::Paused) => {
                        ChangeTiming::Now
                    }
                }
            }
        };
        let snapshot =
            cancel_despite_refusal(self.provider.as_ref(), subscription_ref, timing).await?;
        self.apply_snapshot(pool, quotas, account, snapshot).await?;
        Ok(())
    }

    pub async fn revoke_cancel(
        &self,
        pool: &PgPool,
        quotas: &QuotaService,
        account: AccountId,
    ) -> Result<Subscription> {
        let sub = self.subscription(pool, account).await?;
        let subscription_ref = live_subscription_ref(&sub)?;
        if !booked_to_end(&sub) {
            return Err(AppError::conflict(
                codes::SUBSCRIPTION_STATE,
                "no cancel is scheduled",
            ));
        }
        let snapshot = self.provider.revoke_cancel(subscription_ref).await?;
        self.apply_snapshot(pool, quotas, account, snapshot).await?;
        self.subscription(pool, account).await
    }

    /// The last subscription may still be alive at the provider (event not
    /// yet delivered, cancel that failed to land), so a `Canceled` row is
    /// checked against the provider's view, and what it says is recorded,
    /// before anything is decided on the row. The provider's status is the
    /// answer: the row stays `Canceled` for an unpaid or paused one. A
    /// subscription the provider no longer knows charges nobody; anything
    /// short of an answer is the caller's error.
    async fn lingering(
        &self,
        pool: &PgPool,
        quotas: &QuotaService,
        sub: &Subscription,
    ) -> Result<Option<SubscriptionSnapshot>> {
        let (BillingStatus::Canceled, Some(subscription_ref)) =
            (sub.status, sub.subscription_ref.as_deref())
        else {
            return Ok(None);
        };
        if sub.provider.as_deref() != Some(self.provider.name()) {
            return Ok(None);
        }
        let seen = match self.provider.fetch_subscription(subscription_ref).await {
            Ok(seen) => seen,
            Err(AppError::NotFound { .. }) => return Ok(None),
            Err(err) => return Err(err),
        };
        self.apply_snapshot(pool, quotas, sub.account, seen.clone())
            .await?;
        Ok(Some(seen))
    }

    async fn price_for(&self, pool: &PgPool, plan_id: &str, interval: Interval) -> Result<String> {
        store::price_for_plan(pool, self.provider.name(), plan_id, interval)
            .await?
            .ok_or_else(|| {
                AppError::unprocessable(
                    codes::PLAN_NOT_FOR_SALE,
                    format!("plan {plan_id:?} is not sold {}ly", interval.as_db_str()),
                )
            })
    }
}

fn booked_to_end(sub: &Subscription) -> bool {
    sub.cancel_at.is_some()
}

fn live_subscription_ref(sub: &Subscription) -> Result<&str> {
    match (sub.status, sub.subscription_ref.as_deref()) {
        (BillingStatus::Active, Some(subscription_ref)) => Ok(subscription_ref),
        (BillingStatus::PastDue, _) => Err(AppError::conflict(
            codes::SUBSCRIPTION_STATE,
            "the last payment failed; update the payment method first",
        )),
        _ => Err(AppError::conflict(
            codes::SUBSCRIPTION_STATE,
            "the account has no active subscription",
        )),
    }
}
