//! Where an account stands with paid service, in our terms.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::org::AccountId;
use super::user::UserId;

/// How often a subscription bills.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Interval {
    Month,
    Year,
}

impl Interval {
    pub const ALL: [Interval; 2] = [Interval::Month, Interval::Year];

    pub fn as_db_str(self) -> &'static str {
        match self {
            Interval::Month => "month",
            Interval::Year => "year",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_db_str() == s)
    }
}

/// `None` has never paid. `Canceled` has paid before and sits on its fallback
/// plan, with the provider's customer kept so a return skips re-entering
/// details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BillingStatus {
    None,
    Active,
    PastDue,
    Canceled,
}

impl BillingStatus {
    pub const ALL: [BillingStatus; 4] = [
        BillingStatus::None,
        BillingStatus::Active,
        BillingStatus::PastDue,
        BillingStatus::Canceled,
    ];

    pub fn as_db_str(self) -> &'static str {
        match self {
            BillingStatus::None => "none",
            BillingStatus::Active => "active",
            BillingStatus::PastDue => "past_due",
            BillingStatus::Canceled => "canceled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_db_str() == s)
    }
}

/// One account's subscription columns, read under the row lock and written
/// back whole by the lifecycle. `plan_id` is here to be read; its only writer
/// stays `billing::set_plan`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub account: AccountId,
    pub owner: Option<UserId>,
    pub plan_id: String,
    pub fallback_plan_id: Option<String>,
    pub status: BillingStatus,
    pub pending_plan_id: Option<String>,
    pub plan_change_at: Option<DateTime<Utc>>,
    /// A cancel booked with the provider: paid service ends here and the
    /// account lands on its fallback.
    pub cancel_at: Option<DateTime<Utc>>,
    pub current_period_end: Option<DateTime<Utc>>,
    pub grace_until: Option<DateTime<Utc>>,
    pub dunning_stage: i16,
    pub provider: Option<String>,
    pub customer_ref: Option<String>,
    pub subscription_ref: Option<String>,
    /// The cadence of the price the plan was resolved through.
    pub interval: Option<Interval>,
    pub synced_at: Option<DateTime<Utc>>,
    /// Payment events carry no snapshot, so they keep their own watermark.
    pub payment_synced_at: Option<DateTime<Utc>>,
}

impl Subscription {
    /// Where the account lands when paid service ends: what it had before
    /// paying, or the free tier for an account that was created paying.
    pub fn landing_plan(&self) -> &str {
        self.fallback_plan_id.as_deref().unwrap_or("free")
    }

    pub fn clear_pending(&mut self) {
        self.pending_plan_id = None;
        self.plan_change_at = None;
    }

    pub fn clear_grace(&mut self) {
        self.grace_until = None;
        self.dunning_stage = 0;
    }
}
