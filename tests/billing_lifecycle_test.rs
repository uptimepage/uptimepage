//! The paid lifecycle end to end against a fake provider: what a provider's
//! events and our own clock do to an account, and the owner's actions in
//! front of it. Live PG only; no-ops without `DATABASE_URL`.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::{DateTime, Duration, Utc};
use common::{
    body_json, build_test_app_with_pg_store_anon_tweaked, build_test_app_with_pg_store_tweaked,
    default_http_check, drop_test_db, fresh_test_db, make_user, open_test_pool, pg_pool_from_env,
    unique_slug, with_session,
};
use common::{metric_value, metrics_handle};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uptimepage::billing::lifecycle::{ACK_BUDGET, EVENT_RETENTION_DAYS, GRACE_DAYS, Outcome};
use uptimepage::billing::mail::Mailer;
use uptimepage::billing::provider::fake::{FakeProvider, SIGNATURE, SIGNATURE_HEADER};
use uptimepage::billing::provider::{
    EventKind, Interval, Money, Payment, ProviderEvent, Refund, RefundKind, RefundStatus,
    SubscriptionSnapshot, SubscriptionStatus,
};
use uptimepage::billing::{Actor, Billing, PlanRequest, set_plan};
use uptimepage::config::AppConfig;
use uptimepage::domain::{
    AccountId, BillingStatus, CheckSpec, ExpectedStatus, Landing, OrgId, UserId,
};
use uptimepage::email::{EmailTemplate, InMemoryEmailSender};
use uptimepage::notifier::EmailDelivery;
use uptimepage::quotas::QuotaService;
use uptimepage::storage::subscriptions;
use uuid::Uuid;

const TEAM_MONTH: &str = "pri_team_month";
const TEAM_YEAR: &str = "pri_team_year";
const PRO_MONTH: &str = "pri_pro_month";
const PRO_YEAR: &str = "pri_pro_year";

fn quotas(pool: &PgPool) -> QuotaService {
    let cfg = AppConfig::load().expect("config");
    QuotaService::new(&cfg, Some(pool.clone()))
}

struct Harness {
    pool: PgPool,
    quotas: QuotaService,
    provider: Arc<FakeProvider>,
    billing: Arc<Billing>,
    mail: InMemoryEmailSender,
    own_db: Option<String>,
}

impl Harness {
    async fn finish(self) {
        drop(self.pool);
        if let Some(db) = self.own_db {
            drop_test_db(&db).await;
        }
    }
}

async fn harness() -> Option<Harness> {
    let pool = pg_pool_from_env().await?;
    Some(harness_on(pool, None).await)
}

/// `sweep` acts on every due account it can see, so tests that age one
/// cannot share a database.
async fn isolated(prefix: &str) -> Option<Harness> {
    let (url, name) = fresh_test_db(prefix).await?;
    let pool = open_test_pool(&url).await;
    sqlx::migrate!("./migrations/postgres")
        .run(&pool)
        .await
        .expect("migrate");
    Some(harness_on(pool, Some(name)).await)
}

async fn harness_on(pool: PgPool, own_db: Option<String>) -> Harness {
    seed_prices(&pool).await;
    let provider = Arc::new(FakeProvider::default());
    let mail = InMemoryEmailSender::new();
    let billing = Arc::new(Billing::new(
        provider.clone(),
        Mailer {
            delivery: EmailDelivery {
                sender: Arc::new(mail.clone()),
                from_address: "no-reply@example.test".into(),
                from_name: "Uptimepage".into(),
            },
            public_base_url: "https://app.example.test".into(),
        },
    ));
    Harness {
        quotas: quotas(&pool),
        pool,
        provider,
        billing,
        mail,
        own_db,
    }
}

async fn seed_prices(pool: &PgPool) {
    for (price, plan, interval, amount) in [
        (TEAM_MONTH, "team", "month", 1900),
        (TEAM_YEAR, "team", "year", 19000),
        (PRO_MONTH, "pro", "month", 900),
        (PRO_YEAR, "pro", "year", 9000),
    ] {
        sqlx::query(
            "INSERT INTO plan_prices (provider, price_ref, plan_id, interval, amount_minor, currency) \
             VALUES ('fake', $1, $2, $3, $4, 'USD') ON CONFLICT DO NOTHING",
        )
        .bind(price)
        .bind(plan)
        .bind(interval)
        .bind(amount)
        .execute(pool)
        .await
        .expect("seed price");
    }
}

/// An account on `plan` with one org and `n` monitors.
async fn account(pool: &PgPool, plan: &str, n: usize) -> (AccountId, OrgId, UserId) {
    let user = make_user(pool, "bill").await;
    let (account,): (Uuid,) = sqlx::query_as(
        "INSERT INTO accounts (owner_user_id, plan_id) VALUES ($1, $2) \
         ON CONFLICT (owner_user_id) WHERE owner_user_id IS NOT NULL \
         DO UPDATE SET plan_id = excluded.plan_id RETURNING id",
    )
    .bind(user.0)
    .bind(plan)
    .fetch_one(pool)
    .await
    .expect("account");
    let (org,): (Uuid,) = sqlx::query_as(
        "INSERT INTO organizations (slug, name, account_id) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(unique_slug("bil"))
    .bind("Billed")
    .bind(account)
    .fetch_one(pool)
    .await
    .expect("org");
    let spec = serde_json::to_value(CheckSpec::Http(default_http_check(
        "https://example.com".parse().expect("url"),
        ExpectedStatus::Exact(200),
    )))
    .expect("spec");
    for i in 0..n {
        sqlx::query(
            "INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled, created_at) \
             VALUES ($1, $2, $3, 300, true, now() - make_interval(secs => $4))",
        )
        .bind(org)
        .bind(format!("m{i}"))
        .bind(&spec)
        .bind((1000 - i) as f64)
        .execute(pool)
        .await
        .expect("target");
    }
    (AccountId(account), OrgId(org), user)
}

fn snapshot(
    subscription_ref: &str,
    status: SubscriptionStatus,
    price: &str,
    taken_at: DateTime<Utc>,
) -> SubscriptionSnapshot {
    SubscriptionSnapshot {
        subscription_ref: subscription_ref.into(),
        customer_ref: "ctm_1".into(),
        status,
        price_refs: vec![price.into()],
        period_end: Some(taken_at + Duration::days(30)),
        cancel_at: None,
        taken_at,
    }
}

fn sub_ref() -> String {
    format!("sub_{}", Uuid::now_v7().simple())
}

/// Now as the row will hand it back: Postgres keeps microseconds.
fn now_us() -> DateTime<Utc> {
    use chrono::Timelike;
    let now = Utc::now();
    now.with_nanosecond(now.nanosecond() / 1000 * 1000)
        .expect("in range")
}

fn event(account: AccountId, subscription_ref: &str, kind: EventKind) -> ProviderEvent {
    let occurred_at = match &kind {
        EventKind::Subscription(s) => s.taken_at,
        _ => Utc::now(),
    };
    ProviderEvent {
        event_id: format!("evt_{}", Uuid::now_v7().simple()),
        event_type: "test".into(),
        occurred_at,
        account: Some(account),
        customer_ref: Some("ctm_1".into()),
        subscription_ref: Some(subscription_ref.into()),
        kind,
    }
}

fn paid() -> EventKind {
    paid_as(&format!("txn_{}", Uuid::now_v7().simple()))
}

fn paid_as(transaction_ref: &str) -> EventKind {
    EventKind::Paid(Payment {
        transaction_ref: transaction_ref.into(),
        total: Some(Money {
            amount_minor: 1900,
            currency: "USD".into(),
        }),
    })
}

fn refund(transaction_ref: &str, approved: bool, full: bool) -> EventKind {
    EventKind::Refunded(Refund {
        adjustment_ref: format!("adj_{}", Uuid::now_v7().simple()),
        transaction_ref: transaction_ref.into(),
        action: RefundKind::Refund,
        status: if approved {
            RefundStatus::Approved
        } else {
            RefundStatus::Pending
        },
        full,
        total: Some(Money {
            amount_minor: 1900,
            currency: "USD".into(),
        }),
    })
}

async fn row(pool: &PgPool, account: AccountId) -> uptimepage::domain::Subscription {
    subscriptions::get(pool, account)
        .await
        .expect("row")
        .expect("account exists")
}

async fn held_count(pool: &PgPool, org: OrgId) -> i64 {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM targets WHERE org_id = $1 AND plan_hold_at IS NOT NULL",
    )
    .bind(org.0)
    .fetch_one(pool)
    .await
    .expect("held");
    n
}

async fn ledger_kinds(pool: &PgPool, account: AccountId) -> Vec<String> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT kind FROM account_billing_events WHERE account_id = $1 ORDER BY id")
            .bind(account.0)
            .fetch_all(pool)
            .await
            .expect("ledger");
    rows.into_iter().map(|(k,)| k).collect()
}

fn subjects(mail: &InMemoryEmailSender) -> Vec<&'static str> {
    mail.sent()
        .iter()
        .map(|m| match &m.template {
            EmailTemplate::PaymentFailed { .. } => "payment_failed",
            EmailTemplate::PaymentRecovered { .. } => "payment_recovered",
            EmailTemplate::DowngradeScheduled { .. } => "downgrade_scheduled",
            EmailTemplate::DowngradeApplied { .. } => "downgrade_applied",
            EmailTemplate::SubscriptionCanceled { .. } => "subscription_canceled",
            _ => "other",
        })
        .collect()
}

async fn activate(h: &Harness, account: AccountId, sub: &str, price: &str) -> Outcome {
    let snap = snapshot(sub, SubscriptionStatus::Active, price, Utc::now());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, sub, EventKind::Subscription(snap)),
        )
        .await
        .expect("apply")
}

#[tokio::test]
#[ignore]
async fn a_first_payment_puts_the_account_on_the_plan_and_binds_the_subscription() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();

    let outcome = activate(&h, account, &sub, TEAM_MONTH).await;
    assert_eq!(outcome, Outcome::Applied);

    let s = row(&h.pool, account).await;
    assert_eq!(s.plan_id, "team");
    assert_eq!(s.fallback_plan_id.as_deref(), Some("founding"));
    assert_eq!(s.status, BillingStatus::Active);
    assert_eq!(s.subscription_ref.as_deref(), Some(sub.as_str()));
    assert_eq!(s.customer_ref.as_deref(), Some("ctm_1"));
    assert_eq!(s.interval, Some(Interval::Month));
    assert!(s.current_period_end.is_some());
    assert_eq!(ledger_kinds(&h.pool, account).await, vec!["plan_changed"]);
    assert!(
        h.mail.is_empty(),
        "an upgrade needs no mail: {:?}",
        subjects(&h.mail)
    );
}

#[tokio::test]
#[ignore]
async fn a_redelivered_event_is_acknowledged_without_a_second_effect() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    let snap = snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, Utc::now());
    let ev = event(account, &sub, EventKind::Subscription(snap));

    let first = h
        .billing
        .apply_event(&h.pool, &h.quotas, ev.clone())
        .await
        .expect("first");
    let again = h
        .billing
        .apply_event(&h.pool, &h.quotas, ev)
        .await
        .expect("again");
    assert_eq!((first, again), (Outcome::Applied, Outcome::Duplicate));
    assert_eq!(ledger_kinds(&h.pool, account).await, vec!["plan_changed"]);
}

#[tokio::test]
#[ignore]
async fn an_older_snapshot_never_rewinds_the_account() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    let now = Utc::now();
    activate(&h, account, &sub, TEAM_MONTH).await;

    let earlier = snapshot(
        &sub,
        SubscriptionStatus::PastDue,
        PRO_MONTH,
        now - Duration::minutes(5),
    );
    let outcome = h
        .billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(earlier)),
        )
        .await
        .expect("apply");
    assert_eq!(outcome, Outcome::Stale);
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.status),
        ("team", BillingStatus::Active)
    );
    assert_eq!(s.grace_until, None);
}

#[tokio::test]
#[ignore]
async fn a_smaller_plan_waits_for_the_period_end_and_the_sweep_then_holds_the_excess() {
    let Some(h) = isolated("billing").await else {
        return;
    };
    let (account, org, _) = account(&h.pool, "founding", 52).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;
    assert_eq!(held_count(&h.pool, org).await, 0);
    let period_end = row(&h.pool, account).await.current_period_end;

    let down = snapshot(&sub, SubscriptionStatus::Active, PRO_YEAR, Utc::now());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(down)),
        )
        .await
        .expect("apply");
    let s = row(&h.pool, account).await;
    assert_eq!(s.plan_id, "team", "keeps what was paid for");
    assert_eq!(s.pending_plan_id.as_deref(), Some("pro"));
    assert_eq!(s.plan_change_at, period_end);
    assert_eq!(s.pending_interval, Some(Interval::Year));
    assert_eq!(
        s.interval,
        Some(Interval::Month),
        "the cadence shown is the one being paid"
    );
    assert_eq!(subjects(&h.mail), vec!["downgrade_scheduled"]);
    match &h.mail.sent()[0].template {
        EmailTemplate::DowngradeScheduled {
            over_monitors,
            keep_url,
            ..
        } => {
            assert_eq!(*over_monitors, 2);
            assert_eq!(keep_url, "https://app.example.test/settings/usage");
        }
        other => panic!("{other:?}"),
    }

    // A renewal snapshot on the smaller price must not push the date out.
    let renewal = snapshot(&sub, SubscriptionStatus::Active, PRO_YEAR, Utc::now());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(renewal)),
        )
        .await
        .expect("renewal");
    assert_eq!(row(&h.pool, account).await.plan_change_at, period_end);

    assert_eq!(h.billing.sweep(&h.pool, &h.quotas).await.expect("sweep"), 0);
    sqlx::query("UPDATE accounts SET plan_change_at = now() - interval '1 second' WHERE id = $1")
        .bind(account.0)
        .execute(&h.pool)
        .await
        .expect("age the change");
    assert_eq!(h.billing.sweep(&h.pool, &h.quotas).await.expect("sweep"), 1);

    let s = row(&h.pool, account).await;
    assert_eq!(s.plan_id, "pro");
    assert_eq!(s.pending_plan_id, None);
    assert_eq!(s.pending_interval, None);
    assert_eq!(
        s.interval,
        Some(Interval::Year),
        "landed on the cadence the move was booked at"
    );
    assert_eq!(s.status, BillingStatus::Active);
    assert_eq!(held_count(&h.pool, org).await, 2);
    assert_eq!(
        subjects(&h.mail),
        vec!["downgrade_scheduled", "downgrade_applied"]
    );
    // The provider's next view of the same price now names the cadence.
    let landed = snapshot(&sub, SubscriptionStatus::Active, PRO_YEAR, Utc::now());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(landed)),
        )
        .await
        .expect("landed");
    assert_eq!(row(&h.pool, account).await.interval, Some(Interval::Year));
    match &h.mail.sent()[1].template {
        EmailTemplate::DowngradeApplied {
            held_monitors,
            landing,
            ..
        } => assert_eq!((*held_monitors, *landing), (2, Landing::Scheduled)),
        other => panic!("{other:?}"),
    }
    h.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_booked_move_landed_by_the_providers_renewal_mails_what_was_held() {
    let Some(h) = harness().await else { return };
    let (account, org, _) = account(&h.pool, "founding", 52).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;
    let down = snapshot(&sub, SubscriptionStatus::Active, PRO_YEAR, Utc::now());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(down)),
        )
        .await
        .expect("book");
    sqlx::query("UPDATE accounts SET plan_change_at = now() - interval '1 second' WHERE id = $1")
        .bind(account.0)
        .execute(&h.pool)
        .await
        .expect("age the change");

    let renewal = snapshot(&sub, SubscriptionStatus::Active, PRO_YEAR, Utc::now());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(renewal)),
        )
        .await
        .expect("renewal");
    let s = row(&h.pool, account).await;
    assert_eq!(s.plan_id, "pro");
    assert_eq!(s.pending_plan_id, None);
    assert_eq!(s.interval, Some(Interval::Year));
    assert_eq!(held_count(&h.pool, org).await, 2);
    assert_eq!(
        subjects(&h.mail),
        vec!["downgrade_scheduled", "downgrade_applied"],
        "whichever lands it, the mail names what was held"
    );
    match &h.mail.sent()[1].template {
        EmailTemplate::DowngradeApplied { held_monitors, .. } => assert_eq!(*held_monitors, 2),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
#[ignore]
async fn a_downgrade_snapshot_without_a_period_end_never_cuts_service_early() {
    let Some(h) = harness().await else { return };
    let (account, org, _) = account(&h.pool, "founding", 52).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;

    let mut down = snapshot(&sub, SubscriptionStatus::Active, PRO_MONTH, Utc::now());
    down.period_end = None;
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(down)),
        )
        .await
        .expect("apply");
    let s = row(&h.pool, account).await;
    assert_eq!(
        s.plan_id, "team",
        "no boundary to defer to, so the paid plan stays"
    );
    assert_eq!(s.pending_plan_id, None);
    assert_eq!(held_count(&h.pool, org).await, 0);
    assert!(h.mail.is_empty());
}

#[tokio::test]
#[ignore]
async fn a_move_to_a_fallback_that_is_on_sale_is_a_move_not_a_cancel() {
    let Some(h) = isolated("billing_fallback").await else {
        return;
    };
    // Comped onto pro, then bought team: pro is where a cancel would land.
    let (account, _, _) = account(&h.pool, "pro", 0).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;
    let s = row(&h.pool, account).await;
    assert_eq!((s.plan_id.as_str(), s.landing_plan()), ("team", "pro"));

    let downgrade = snapshot(&sub, SubscriptionStatus::Active, PRO_MONTH, Utc::now());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(downgrade)),
        )
        .await
        .expect("downgrade");
    let s = row(&h.pool, account).await;
    assert_eq!(s.pending_plan_id.as_deref(), Some("pro"));
    assert_eq!(s.cancel_at, None, "a move to the fallback is still a move");
    let err = h
        .billing
        .revoke_cancel(&h.pool, &h.quotas, account)
        .await
        .expect_err("nothing to withdraw");
    assert!(err.to_string().contains("no cancel is scheduled"), "{err}");

    sqlx::query("UPDATE accounts SET plan_change_at = now() - interval '1 second' WHERE id = $1")
        .bind(account.0)
        .execute(&h.pool)
        .await
        .expect("age the move");
    assert_eq!(h.billing.sweep(&h.pool, &h.quotas).await.expect("sweep"), 1);
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.status, s.interval),
        ("pro", BillingStatus::Active, Some(Interval::Month)),
        "the subscription runs on at the lower price"
    );
    assert_eq!(
        subjects(&h.mail),
        vec!["downgrade_scheduled", "downgrade_applied"]
    );
    h.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_scheduled_cancel_landing_by_the_clock_ends_service_not_a_plain_downgrade() {
    let Some(h) = isolated("billing").await else {
        return;
    };
    let (account, org, _) = account(&h.pool, "founding", 52).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;
    let mut canceling = snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, Utc::now());
    canceling.cancel_at = canceling.period_end;
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(canceling)),
        )
        .await
        .expect("cancel");
    assert_eq!(subjects(&h.mail), vec!["subscription_canceled"]);

    sqlx::query("UPDATE accounts SET cancel_at = now() - interval '1 second' WHERE id = $1")
        .bind(account.0)
        .execute(&h.pool)
        .await
        .expect("age the cancel");
    assert_eq!(h.billing.sweep(&h.pool, &h.quotas).await.expect("sweep"), 1);

    let s = row(&h.pool, account).await;
    assert_eq!(s.plan_id, "founding");
    assert_eq!(
        s.status,
        BillingStatus::Canceled,
        "the clock ends service, not just moves the plan"
    );
    assert_eq!(s.pending_plan_id, None);
    assert_eq!(s.current_period_end, None, "nothing renews");
    assert_eq!(held_count(&h.pool, org).await, 2);
    assert_eq!(
        subjects(&h.mail),
        vec!["subscription_canceled", "downgrade_applied"]
    );

    // The provider's own view, still active with the cancel date behind it,
    // arrives after the clock (a late event, or a checkout asking): over is
    // over.
    let mut lagging = snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, Utc::now());
    lagging.cancel_at = Some(Utc::now() - Duration::seconds(1));
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(lagging)),
        )
        .await
        .expect("lagging view");
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.status, s.cancel_at),
        ("founding", BillingStatus::Canceled, None)
    );
    assert_eq!(held_count(&h.pool, org).await, 2);
    assert_eq!(
        subjects(&h.mail),
        vec!["subscription_canceled", "downgrade_applied"],
        "nothing is booked or announced twice"
    );
    h.finish().await;
}

#[tokio::test]
#[ignore]
async fn the_sweep_forgets_events_the_provider_stopped_redelivering() {
    let Some(h) = isolated("billing").await else {
        return;
    };
    let remember = |event_id: &'static str, age: Duration| {
        sqlx::query(
            "INSERT INTO provider_events (provider, event_id, event_type, occurred_at, received_at) \
             VALUES ('fake', $1, 'transaction.completed', now() - $2, now() - $2)",
        )
        .bind(event_id)
        .bind(age)
        .execute(&h.pool)
    };
    remember("evt_old", Duration::days(EVENT_RETENTION_DAYS + 1))
        .await
        .expect("old event");
    remember("evt_recent", Duration::days(EVENT_RETENTION_DAYS - 1))
        .await
        .expect("recent event");

    h.billing.sweep(&h.pool, &h.quotas).await.expect("sweep");

    let seen = |id| subscriptions::event_seen(&h.pool, "fake", id);
    assert!(
        !seen("evt_old").await.expect("old"),
        "past retention is forgotten"
    );
    assert!(
        seen("evt_recent").await.expect("recent"),
        "inside retention is kept"
    );
    h.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_failed_payment_opens_a_grace_window_the_sweep_closes_and_a_payment_reopens() {
    let Some(h) = isolated("billing").await else {
        return;
    };
    let (account, org, _) = account(&h.pool, "founding", 52).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;

    let outcome = h
        .billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::PaymentFailed),
        )
        .await
        .expect("failed");
    assert_eq!(outcome, Outcome::Applied);
    let s = row(&h.pool, account).await;
    assert_eq!(s.status, BillingStatus::PastDue);
    assert_eq!(s.plan_id, "team", "day zero cuts nothing");
    let until = s.grace_until.expect("grace");
    assert!(
        (until - Utc::now() - Duration::days(GRACE_DAYS))
            .num_seconds()
            .abs()
            < 5
    );
    assert_eq!(s.dunning_stage, 1);
    assert_eq!(subjects(&h.mail), vec!["payment_failed"]);

    // A second failure does not move the deadline.
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::PaymentFailed),
        )
        .await
        .expect("failed again");
    assert_eq!(row(&h.pool, account).await.grace_until, Some(until));

    h.provider.subscriptions.lock().unwrap().insert(
        sub.clone(),
        snapshot(&sub, SubscriptionStatus::PastDue, TEAM_MONTH, Utc::now()),
    );
    sqlx::query("UPDATE accounts SET grace_until = now() - interval '1 second' WHERE id = $1")
        .bind(account.0)
        .execute(&h.pool)
        .await
        .expect("age the grace");
    assert_eq!(h.billing.sweep(&h.pool, &h.quotas).await.expect("sweep"), 1);
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.status),
        ("founding", BillingStatus::Canceled)
    );
    assert_eq!(s.grace_until, None);
    assert_eq!(held_count(&h.pool, org).await, 2);
    assert!(matches!(
        h.mail.sent()[1].template,
        EmailTemplate::DowngradeApplied {
            landing: Landing::Unpaid,
            ..
        }
    ));
    assert!(
        ledger_kinds(&h.pool, account)
            .await
            .contains(&"grace_expired".to_string())
    );
    assert!(
        h.provider
            .calls
            .lock()
            .unwrap()
            .contains(&format!("cancel:{sub}:Now")),
        "the provider stops retrying the card: {:?}",
        h.provider.calls.lock().unwrap()
    );
    assert_eq!(
        h.provider.subscriptions.lock().unwrap()[&sub].status,
        SubscriptionStatus::Canceled
    );

    // A return's payment can land before its subscription event.
    let fresh = sub_ref();
    h.provider.subscriptions.lock().unwrap().insert(
        fresh.clone(),
        snapshot(&fresh, SubscriptionStatus::Active, TEAM_MONTH, Utc::now()),
    );
    let outcome = h
        .billing
        .apply_event(&h.pool, &h.quotas, event(account, &fresh, paid()))
        .await
        .expect("paid");
    assert_eq!(outcome, Outcome::Applied);
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.status),
        ("team", BillingStatus::Active)
    );
    assert_eq!(s.subscription_ref.as_deref(), Some(fresh.as_str()));
    assert_eq!(held_count(&h.pool, org).await, 0);
    assert!(
        h.provider
            .calls
            .lock()
            .unwrap()
            .contains(&format!("fetch:{fresh}"))
    );
    h.finish().await;
}

/// A past-due account whose grace window has just run out, the provider
/// holding its subscription as `status`.
async fn expired_grace(h: &Harness, status: SubscriptionStatus) -> (AccountId, String) {
    let (account, _, _) = account(&h.pool, "founding", 52).await;
    let sub = sub_ref();
    activate(h, account, &sub, TEAM_MONTH).await;
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::PaymentFailed),
        )
        .await
        .expect("failed");
    h.provider
        .subscriptions
        .lock()
        .unwrap()
        .insert(sub.clone(), snapshot(&sub, status, TEAM_MONTH, Utc::now()));
    sqlx::query("UPDATE accounts SET grace_until = now() - interval '1 second' WHERE id = $1")
        .bind(account.0)
        .execute(&h.pool)
        .await
        .expect("age the grace");
    h.provider.calls.lock().unwrap().clear();
    (account, sub)
}

#[tokio::test]
#[ignore]
async fn a_retry_paid_before_its_webhook_recovers_the_account_at_grace_expiry() {
    let Some(h) = isolated("billing").await else {
        return;
    };
    let (account, sub) = expired_grace(&h, SubscriptionStatus::Active).await;

    assert_eq!(h.billing.sweep(&h.pool, &h.quotas).await.expect("sweep"), 1);
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.status, s.grace_until),
        ("team", BillingStatus::Active, None)
    );
    assert_eq!(
        *h.provider.calls.lock().unwrap(),
        vec![format!("fetch:{sub}")],
        "nothing paid for is cancelled"
    );
    assert_eq!(
        subjects(&h.mail),
        vec!["payment_failed", "payment_recovered"]
    );
    h.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_grace_expiry_waits_for_the_provider_to_answer() {
    let Some(h) = isolated("billing").await else {
        return;
    };
    let (account, sub) = expired_grace(&h, SubscriptionStatus::PastDue).await;

    h.provider.fetch_fails.store(true, Ordering::Relaxed);
    assert_eq!(h.billing.sweep(&h.pool, &h.quotas).await.expect("sweep"), 0);
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.status),
        ("team", BillingStatus::PastDue)
    );
    assert!(
        !h.provider
            .calls
            .lock()
            .unwrap()
            .contains(&format!("cancel:{sub}:Now"))
    );

    h.provider.fetch_fails.store(false, Ordering::Relaxed);
    assert_eq!(h.billing.sweep(&h.pool, &h.quotas).await.expect("sweep"), 1);
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.status),
        ("founding", BillingStatus::Canceled)
    );
    assert!(
        h.provider
            .calls
            .lock()
            .unwrap()
            .contains(&format!("cancel:{sub}:Now"))
    );
    h.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_day_of_provider_silence_past_the_window_ends_the_wait() {
    let Some(h) = isolated("billing").await else {
        return;
    };
    let (account, sub) = expired_grace(&h, SubscriptionStatus::PastDue).await;
    sqlx::query("UPDATE accounts SET grace_until = now() - interval '25 hours' WHERE id = $1")
        .bind(account.0)
        .execute(&h.pool)
        .await
        .expect("age the grace a day more");

    h.provider.fetch_fails.store(true, Ordering::Relaxed);
    assert_eq!(h.billing.sweep(&h.pool, &h.quotas).await.expect("sweep"), 1);
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.status),
        ("founding", BillingStatus::Canceled)
    );
    let calls = h.provider.calls.lock().unwrap().clone();
    assert_eq!(
        calls.first(),
        Some(&format!("fetch:{sub}")),
        "still asked first"
    );
    assert!(calls.contains(&format!("cancel:{sub}:Now")));
    h.finish().await;
}

const CANCEL_FAILED: &str = "uptimepage_billing_provider_cancel_failed_total";

fn cancel_failures() -> f64 {
    metric_value(&metrics_handle().render(), CANCEL_FAILED).unwrap_or(0.0)
}

/// A grace window run out with the provider refusing the cancel, its own
/// view of the subscription being `status`, or none at all.
async fn grace_expiry_with_cancel_refused(
    h: &Harness,
    status: Option<SubscriptionStatus>,
) -> AccountId {
    let (account, _, _) = account(&h.pool, "founding", 52).await;
    let sub = sub_ref();
    activate(h, account, &sub, TEAM_MONTH).await;
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::PaymentFailed),
        )
        .await
        .expect("failed");
    if let Some(status) = status {
        h.provider
            .subscriptions
            .lock()
            .unwrap()
            .insert(sub.clone(), snapshot(&sub, status, TEAM_MONTH, Utc::now()));
    }
    h.provider.cancel_refused.store(true, Ordering::Relaxed);
    sqlx::query("UPDATE accounts SET grace_until = now() - interval '1 second' WHERE id = $1")
        .bind(account.0)
        .execute(&h.pool)
        .await
        .expect("age the grace");
    assert_eq!(h.billing.sweep(&h.pool, &h.quotas).await.expect("sweep"), 1);
    let calls = h.provider.calls.lock().unwrap();
    assert!(calls.contains(&format!("cancel:{sub}:Now")));
    assert!(
        calls.contains(&format!("fetch:{sub}")),
        "a refusal is checked against the provider's view"
    );
    account
}

/// One test: the counter is process-wide, so the cases run in sequence.
#[tokio::test]
#[ignore]
async fn a_refused_cancel_is_counted_unless_the_provider_shows_the_subscription_ended() {
    let Some(h) = isolated("billing").await else {
        return;
    };

    let before = cancel_failures();
    let account = grace_expiry_with_cancel_refused(&h, Some(SubscriptionStatus::PastDue)).await;
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.status),
        ("founding", BillingStatus::Canceled),
        "our side ends regardless of the provider"
    );
    assert_eq!(
        subjects(&h.mail),
        vec!["payment_failed", "downgrade_applied"]
    );
    assert_eq!(cancel_failures(), before + 1.0, "still live: counted");

    let before = cancel_failures();
    grace_expiry_with_cancel_refused(&h, Some(SubscriptionStatus::Canceled)).await;
    assert_eq!(
        cancel_failures(),
        before,
        "already canceled: nobody is charged"
    );

    let before = cancel_failures();
    grace_expiry_with_cancel_refused(&h, Some(SubscriptionStatus::Paused)).await;
    assert_eq!(cancel_failures(), before + 1.0, "paused resumes and bills");

    let before = cancel_failures();
    grace_expiry_with_cancel_refused(&h, None).await;
    assert_eq!(
        cancel_failures(),
        before + 1.0,
        "no view of it at all: counted"
    );

    // Booked through the portal to end with the period: the sweep takes that
    // end, since a card retry succeeding before the date brings the account
    // back live for the time it paid.
    h.provider.cancel_refused.store(false, Ordering::Relaxed);
    let (unpaid, _, _) = self::account(&h.pool, "founding", 52).await;
    let sub = sub_ref();
    activate(&h, unpaid, &sub, TEAM_MONTH).await;
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(unpaid, &sub, EventKind::PaymentFailed),
        )
        .await
        .expect("failed");
    let mut booked = snapshot(&sub, SubscriptionStatus::PastDue, TEAM_MONTH, Utc::now());
    booked.cancel_at = booked.period_end;
    h.provider
        .subscriptions
        .lock()
        .unwrap()
        .insert(sub.clone(), booked);
    h.provider.calls.lock().unwrap().clear();
    sqlx::query("UPDATE accounts SET grace_until = now() - interval '1 second' WHERE id = $1")
        .bind(unpaid.0)
        .execute(&h.pool)
        .await
        .expect("age the grace");
    let before = cancel_failures();
    assert_eq!(h.billing.sweep(&h.pool, &h.quotas).await.expect("sweep"), 1);
    assert_eq!(
        *h.provider.calls.lock().unwrap(),
        vec![
            format!("fetch:{sub}"),
            format!("cancel:{sub}:Now"),
            format!("fetch:{sub}")
        ]
    );
    assert_eq!(
        cancel_failures(),
        before,
        "already ending: nothing to count"
    );
    h.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_payment_inside_the_window_recovers_the_account_and_says_so() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::PaymentFailed),
        )
        .await
        .expect("failed");

    h.billing
        .apply_event(&h.pool, &h.quotas, event(account, &sub, paid()))
        .await
        .expect("paid");
    let s = row(&h.pool, account).await;
    assert_eq!(s.status, BillingStatus::Active);
    assert_eq!(s.grace_until, None);
    assert_eq!(s.dunning_stage, 0);
    assert_eq!(
        subjects(&h.mail),
        vec!["payment_failed", "payment_recovered"]
    );
    assert!(
        h.provider.calls.lock().unwrap().is_empty(),
        "nothing to fetch"
    );
}

#[tokio::test]
#[ignore]
async fn reminders_follow_the_grace_clock() {
    let Some(h) = isolated("billing").await else {
        return;
    };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::PaymentFailed),
        )
        .await
        .expect("failed");
    h.mail.clear();

    assert_eq!(h.billing.sweep(&h.pool, &h.quotas).await.expect("sweep"), 0);
    assert!(h.mail.is_empty());

    // Three days and an hour in: the second reminder. The hour keeps a
    // database clock a little ahead of ours from rounding the day down.
    sqlx::query(
        "UPDATE accounts SET grace_until = now() + make_interval(days => $2) - interval '1 hour' \
         WHERE id = $1",
    )
    .bind(account.0)
    .bind((GRACE_DAYS - 3) as i32)
    .execute(&h.pool)
    .await
    .expect("age");
    assert_eq!(h.billing.sweep(&h.pool, &h.quotas).await.expect("sweep"), 1);
    assert_eq!(h.billing.sweep(&h.pool, &h.quotas).await.expect("sweep"), 0);
    assert_eq!(subjects(&h.mail), vec!["payment_failed"]);
    assert_eq!(row(&h.pool, account).await.dunning_stage, 2);
    h.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_cancel_is_booked_for_the_period_end_and_can_be_withdrawn() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;

    let mut canceling = snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, now_us());
    canceling.cancel_at = canceling.period_end;
    let ends_at = canceling.period_end;
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(canceling)),
        )
        .await
        .expect("cancel");
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.status),
        ("team", BillingStatus::Active)
    );
    assert_eq!(s.cancel_at, ends_at);
    assert_eq!(s.pending_plan_id, None);
    assert_eq!(subjects(&h.mail), vec!["subscription_canceled"]);

    let kept = snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, Utc::now());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(kept)),
        )
        .await
        .expect("revoke");
    let s = row(&h.pool, account).await;
    assert_eq!(s.cancel_at, None);
    assert_eq!(s.pending_plan_id, None);
    assert!(
        ledger_kinds(&h.pool, account)
            .await
            .contains(&"pending_change_cleared".to_string())
    );
}

#[tokio::test]
#[ignore]
async fn the_provider_ending_the_subscription_lands_the_account_once() {
    let Some(h) = harness().await else { return };
    let (account, org, _) = account(&h.pool, "founding", 52).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;

    let ended = snapshot(&sub, SubscriptionStatus::Canceled, TEAM_MONTH, Utc::now());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(ended)),
        )
        .await
        .expect("ended");
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.status),
        ("founding", BillingStatus::Canceled)
    );
    assert_eq!(held_count(&h.pool, org).await, 2);
    assert_eq!(subjects(&h.mail), vec!["downgrade_applied"]);

    let again = snapshot(&sub, SubscriptionStatus::Canceled, TEAM_MONTH, Utc::now());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(again)),
        )
        .await
        .expect("ended again");
    assert_eq!(
        subjects(&h.mail),
        vec!["downgrade_applied"],
        "no second mail"
    );
}

#[tokio::test]
#[ignore]
async fn a_payment_naming_no_subscription_cannot_touch_a_live_account() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;
    let nameless = |kind| {
        let mut ev = event(account, &sub, kind);
        ev.subscription_ref = None;
        ev
    };

    let outcome = h
        .billing
        .apply_event(&h.pool, &h.quotas, nameless(EventKind::PaymentFailed))
        .await
        .expect("failed");
    assert_eq!(outcome, Outcome::Foreign, "a declined stray checkout");
    let s = row(&h.pool, account).await;
    assert_eq!(s.status, BillingStatus::Active);
    assert_eq!(s.grace_until, None);

    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::PaymentFailed),
        )
        .await
        .expect("failed");
    let outcome = h
        .billing
        .apply_event(&h.pool, &h.quotas, nameless(paid()))
        .await
        .expect("paid");
    assert_eq!(
        outcome,
        Outcome::Foreign,
        "a stray payment recovers nothing"
    );
    assert_eq!(row(&h.pool, account).await.status, BillingStatus::PastDue);
    assert!(h.provider.calls.lock().unwrap().is_empty());
    assert_eq!(subjects(&h.mail), vec!["payment_failed"]);
}

#[tokio::test]
#[ignore]
async fn a_bound_subscription_answers_to_its_account_whatever_the_event_names() {
    let Some(h) = harness().await else { return };
    let (holder, _, _) = account(&h.pool, "founding", 0).await;
    let (named, _, _) = account(&h.pool, "free", 0).await;
    let sub = sub_ref();
    activate(&h, holder, &sub, TEAM_MONTH).await;

    let ended = snapshot(&sub, SubscriptionStatus::Canceled, TEAM_MONTH, Utc::now());
    let outcome = h
        .billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(named, &sub, EventKind::Subscription(ended)),
        )
        .await
        .expect("apply");
    assert_eq!(outcome, Outcome::Applied);
    assert_eq!(row(&h.pool, holder).await.status, BillingStatus::Canceled);
    let other = row(&h.pool, named).await;
    assert_eq!(other.status, BillingStatus::None);
    assert_eq!(other.subscription_ref, None);
    assert_eq!(subjects(&h.mail), vec!["downgrade_applied"]);
}

#[tokio::test]
#[ignore]
async fn a_strangers_subscription_cannot_touch_a_live_account_and_is_ended() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;

    let intruder = snapshot(
        "sub_evil",
        SubscriptionStatus::Active,
        PRO_MONTH,
        Utc::now(),
    );
    h.provider
        .subscriptions
        .lock()
        .unwrap()
        .insert("sub_evil".into(), intruder.clone());
    let outcome = h
        .billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, "sub_evil", EventKind::Subscription(intruder)),
        )
        .await
        .expect("apply");
    assert_eq!(outcome, Outcome::Foreign);
    let failed = h
        .billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, "sub_evil", EventKind::PaymentFailed),
        )
        .await
        .expect("apply");
    assert_eq!(failed, Outcome::Foreign);
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.status),
        ("team", BillingStatus::Active)
    );
    assert_eq!(s.pending_plan_id, None);
    assert_eq!(s.subscription_ref.as_deref(), Some(sub.as_str()));
    assert_eq!(
        *h.provider.calls.lock().unwrap(),
        vec!["cancel:sub_evil:Now".to_string()],
        "a second subscription can serve nobody, so it is ended rather than billed on"
    );
}

#[tokio::test]
#[ignore]
async fn the_older_of_two_purchases_is_ended_when_the_newer_one_bound_first() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let now = Utc::now();
    let older = sub_ref();
    let newer = sub_ref();
    let older_activation = snapshot(
        &older,
        SubscriptionStatus::Active,
        PRO_MONTH,
        now - Duration::seconds(5),
    );
    h.provider
        .subscriptions
        .lock()
        .unwrap()
        .insert(older.clone(), older_activation.clone());

    let newer_activation = snapshot(&newer, SubscriptionStatus::Active, TEAM_MONTH, now);
    let outcome = h
        .billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &newer, EventKind::Subscription(newer_activation)),
        )
        .await
        .expect("newer");
    assert_eq!(outcome, Outcome::Applied);

    let outcome = h
        .billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &older, EventKind::Subscription(older_activation)),
        )
        .await
        .expect("older");
    assert_eq!(
        outcome,
        Outcome::Foreign,
        "older than the watermark, but the watermark is the other subscription's"
    );
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.status),
        ("team", BillingStatus::Active)
    );
    assert_eq!(s.subscription_ref.as_deref(), Some(newer.as_str()));
    assert_eq!(
        *h.provider.calls.lock().unwrap(),
        vec![format!("cancel:{older}:Now")],
        "the purchase nobody is served by is ended, not left charging"
    );
    assert_eq!(
        ledger_kinds(&h.pool, account)
            .await
            .last()
            .map(String::as_str),
        Some("foreign_subscription_ignored")
    );
}

#[tokio::test]
#[ignore]
async fn a_cancel_that_outran_its_activation_moves_nothing() {
    let Some(h) = harness().await else { return };
    let (account, org, _) = account(&h.pool, "founding", 52).await;
    let sub = sub_ref();
    let now = Utc::now();

    let ended = snapshot(&sub, SubscriptionStatus::Canceled, TEAM_MONTH, now);
    let outcome = h
        .billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(ended)),
        )
        .await
        .expect("ended");
    assert_eq!(outcome, Outcome::Applied);
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.status),
        ("founding", BillingStatus::None),
        "a subscription that never served the account cannot take its plan"
    );
    assert_eq!(s.subscription_ref, None);
    assert_eq!(held_count(&h.pool, org).await, 0);
    assert!(h.mail.is_empty(), "{:?}", subjects(&h.mail));
    assert_eq!(ledger_kinds(&h.pool, account).await, Vec::<String>::new());

    let late = snapshot(
        &sub,
        SubscriptionStatus::Active,
        TEAM_MONTH,
        now - Duration::seconds(2),
    );
    let outcome = h
        .billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(late)),
        )
        .await
        .expect("late activation");
    assert_eq!(
        outcome,
        Outcome::Stale,
        "already ended, so it never goes live"
    );
    assert_eq!(row(&h.pool, account).await.plan_id, "founding");
}

#[tokio::test]
#[ignore]
async fn a_payment_never_hides_an_earlier_snapshot() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    let now = Utc::now();
    activate(&h, account, &sub, TEAM_MONTH).await;

    // Delivered out of order: the renewal's payment first, the cancel booked
    // a moment before it second.
    let mut paid = event(account, &sub, paid());
    paid.occurred_at = now + Duration::seconds(2);
    h.billing
        .apply_event(&h.pool, &h.quotas, paid)
        .await
        .expect("paid");

    let mut canceling = snapshot(
        &sub,
        SubscriptionStatus::Active,
        TEAM_MONTH,
        now + Duration::seconds(1),
    );
    canceling.cancel_at = canceling.period_end;
    let outcome = h
        .billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(canceling)),
        )
        .await
        .expect("cancel");
    assert_eq!(outcome, Outcome::Applied);
    let s = row(&h.pool, account).await;
    assert!(s.cancel_at.is_some());
    assert_eq!(s.landing_plan(), "founding");
    assert_eq!(subjects(&h.mail), vec!["subscription_canceled"]);
}

#[tokio::test]
#[ignore]
async fn a_late_payment_event_never_undoes_a_newer_one() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;
    let now = Utc::now();
    let at = |secs: i64, kind: EventKind| {
        let mut e = event(account, &sub, kind);
        e.occurred_at = now + Duration::seconds(secs);
        e
    };
    let apply = |e: ProviderEvent| h.billing.apply_event(&h.pool, &h.quotas, e);

    assert_eq!(
        apply(at(1, EventKind::PaymentFailed))
            .await
            .expect("failed"),
        Outcome::Applied
    );
    assert_eq!(apply(at(3, paid())).await.expect("paid"), Outcome::Applied);
    assert_eq!(
        apply(at(2, EventKind::PaymentFailed))
            .await
            .expect("late failure"),
        Outcome::Stale,
        "a failure the payment already cleared reopens nothing"
    );
    let s = row(&h.pool, account).await;
    assert_eq!((s.status, s.grace_until), (BillingStatus::Active, None));
    assert_eq!(
        s.payment_synced_at.map(|t| t.timestamp_micros()),
        Some((now + Duration::seconds(3)).timestamp_micros())
    );

    assert_eq!(
        apply(at(5, EventKind::PaymentFailed))
            .await
            .expect("failed"),
        Outcome::Applied
    );
    let until = row(&h.pool, account).await.grace_until.expect("grace");
    assert_eq!(
        apply(at(4, paid())).await.expect("late payment"),
        Outcome::Stale,
        "a payment older than the failure recovers nothing"
    );
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.status, s.grace_until),
        (BillingStatus::PastDue, Some(until))
    );
    assert_eq!(
        subjects(&h.mail),
        vec!["payment_failed", "payment_recovered", "payment_failed"]
    );
}

#[tokio::test]
#[ignore]
async fn a_snapshot_older_than_the_last_payment_event_moves_no_status() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;
    let now = Utc::now();
    let at = |millis: i64, kind: EventKind| {
        let mut e = event(account, &sub, kind);
        e.occurred_at = now + Duration::milliseconds(millis);
        e
    };
    let taken = |millis: i64, status: SubscriptionStatus| {
        let snap = snapshot(
            &sub,
            status,
            TEAM_MONTH,
            now + Duration::milliseconds(millis),
        );
        event(account, &sub, EventKind::Subscription(snap))
    };
    let apply = |e: ProviderEvent| h.billing.apply_event(&h.pool, &h.quotas, e);

    apply(at(1000, EventKind::PaymentFailed))
        .await
        .expect("failed");
    let until = row(&h.pool, account).await.grace_until.expect("grace");
    assert_eq!(
        apply(taken(500, SubscriptionStatus::Active))
            .await
            .expect("older live snapshot"),
        Outcome::Applied
    );
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.status, s.grace_until),
        (BillingStatus::PastDue, Some(until)),
        "a view from before the failure recovers nothing"
    );
    assert_eq!(
        s.current_period_end.map(|t| t.timestamp_micros()),
        Some((now + Duration::milliseconds(500) + Duration::days(30)).timestamp_micros()),
        "but it is still applied"
    );

    apply(at(3000, paid())).await.expect("paid");
    assert_eq!(
        apply(taken(2000, SubscriptionStatus::PastDue))
            .await
            .expect("older past_due snapshot"),
        Outcome::Applied
    );
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.status, s.grace_until),
        (BillingStatus::Active, None),
        "a view from before the payment reopens nothing"
    );

    apply(taken(4000, SubscriptionStatus::PastDue))
        .await
        .expect("newer past_due snapshot");
    assert_eq!(
        row(&h.pool, account).await.status,
        BillingStatus::PastDue,
        "a past_due view newer than the payment is a new failure"
    );
    assert_eq!(
        subjects(&h.mail),
        vec!["payment_failed", "payment_recovered", "payment_failed"]
    );
}

#[tokio::test]
#[ignore]
async fn a_checkout_asks_after_the_last_subscription_before_selling_another() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;
    let ended = snapshot(&sub, SubscriptionStatus::Canceled, TEAM_MONTH, Utc::now());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(ended)),
        )
        .await
        .expect("ended");
    assert_eq!(row(&h.pool, account).await.status, BillingStatus::Canceled);

    h.provider.subscriptions.lock().unwrap().insert(
        sub.clone(),
        snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, Utc::now()),
    );
    let err = h
        .billing
        .checkout(&h.pool, &h.quotas, account, "pro", Interval::Month)
        .await
        .expect_err("a live subscription is not doubled");
    assert!(
        err.to_string().contains("already has a subscription"),
        "{err}"
    );
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.status),
        ("team", BillingStatus::Active)
    );
    assert!(
        h.provider
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|c| !c.starts_with("checkout:")),
        "{:?}",
        h.provider.calls.lock().unwrap()
    );

    let over = snapshot(&sub, SubscriptionStatus::Canceled, TEAM_MONTH, Utc::now());
    h.provider
        .subscriptions
        .lock()
        .unwrap()
        .insert(sub.clone(), over.clone());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(over)),
        )
        .await
        .expect("over");
    // Unpaid or paused there still charges or resumes; the row stays ended
    // either way, so the provider's word decides.
    for (status, way_out) in [
        (SubscriptionStatus::PastDue, "update the payment method"),
        (
            SubscriptionStatus::Paused,
            "resume or cancel it in the portal",
        ),
    ] {
        h.provider
            .subscriptions
            .lock()
            .unwrap()
            .insert(sub.clone(), snapshot(&sub, status, TEAM_MONTH, Utc::now()));
        let err = h
            .billing
            .checkout(&h.pool, &h.quotas, account, "pro", Interval::Month)
            .await
            .expect_err("still runs at the provider");
        assert!(err.to_string().contains(way_out), "{status:?}: {err}");
        assert_eq!(row(&h.pool, account).await.status, BillingStatus::Canceled);
    }
    h.provider.fetch_fails.store(true, Ordering::Relaxed);
    let err = h
        .billing
        .checkout(&h.pool, &h.quotas, account, "pro", Interval::Month)
        .await
        .expect_err("no answer, no sale");
    assert!(err.to_string().contains("no response"), "{err}");
    h.provider.fetch_fails.store(false, Ordering::Relaxed);
    assert!(
        h.provider
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|c| !c.starts_with("checkout:")),
        "{:?}",
        h.provider.calls.lock().unwrap()
    );

    h.provider.subscriptions.lock().unwrap().insert(
        sub.clone(),
        snapshot(&sub, SubscriptionStatus::Canceled, TEAM_MONTH, Utc::now()),
    );
    h.billing
        .checkout(&h.pool, &h.quotas, account, "pro", Interval::Month)
        .await
        .expect("checkout");
    assert_eq!(row(&h.pool, account).await.status, BillingStatus::Canceled);
    assert!(
        h.provider
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("checkout:"))
    );

    // One the provider no longer knows charges nobody.
    h.provider.subscriptions.lock().unwrap().remove(&sub);
    h.billing
        .checkout(&h.pool, &h.quotas, account, "pro", Interval::Month)
        .await
        .expect("checkout after the provider lost it");
}

#[tokio::test]
#[ignore]
async fn a_plan_that_shrinks_only_flow_checks_is_a_downgrade() {
    let Some(h) = isolated("billing_flow").await else {
        return;
    };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    sqlx::query(
        "INSERT INTO plans SELECT * FROM jsonb_populate_record(NULL::plans, \
           (SELECT to_jsonb(p) || '{\"id\": \"team_lite\", \"name\": \"Team lite\", \"max_flow_checks\": 1}' \
            FROM plans p WHERE p.id = 'team'))",
    )
    .execute(&h.pool)
    .await
    .expect("plan");
    sqlx::query(
        "INSERT INTO plan_prices (provider, price_ref, plan_id, interval, amount_minor, currency) \
         VALUES ('fake', 'pri_team_lite_month', 'team_lite', 'month', 1500, 'USD')",
    )
    .execute(&h.pool)
    .await
    .expect("price");
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;

    let down = snapshot(
        &sub,
        SubscriptionStatus::Active,
        "pri_team_lite_month",
        Utc::now(),
    );
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(down)),
        )
        .await
        .expect("apply");
    let s = row(&h.pool, account).await;
    assert_eq!(s.plan_id, "team", "fewer flow checks is a cut, so it waits");
    assert_eq!(s.pending_plan_id.as_deref(), Some("team_lite"));
    assert_eq!(subjects(&h.mail), vec!["downgrade_scheduled"]);
    h.finish().await;
}

#[tokio::test]
#[ignore]
async fn noise_without_an_account_is_not_an_unmatched_purchase() {
    let Some(h) = harness().await else { return };
    let mut noise = event(AccountId(Uuid::nil()), "sub_none", EventKind::Other);
    noise.account = None;
    noise.subscription_ref = None;
    let outcome = h
        .billing
        .apply_event(&h.pool, &h.quotas, noise.clone())
        .await
        .expect("noise");
    assert_eq!(outcome, Outcome::Applied);

    let mut paid = event(AccountId(Uuid::nil()), "sub_none", paid());
    paid.account = None;
    paid.subscription_ref = None;
    let outcome = h
        .billing
        .apply_event(&h.pool, &h.quotas, paid)
        .await
        .expect("paid");
    assert_eq!(
        outcome,
        Outcome::Unmatched,
        "money with nobody to give it to"
    );

    // The scheduled end of a purged account's subscription, and the same for
    // one whose id was never ours.
    let purged = AccountId(Uuid::now_v7());
    for (account, status, expected) in [
        (Some(purged), SubscriptionStatus::Canceled, Outcome::Applied),
        (None, SubscriptionStatus::Canceled, Outcome::Applied),
        (Some(purged), SubscriptionStatus::Active, Outcome::Unmatched),
        (None, SubscriptionStatus::Active, Outcome::Unmatched),
        (Some(purged), SubscriptionStatus::Paused, Outcome::Unmatched),
    ] {
        let snap = snapshot("sub_gone", status, TEAM_MONTH, Utc::now());
        let mut ev = event(
            AccountId(Uuid::nil()),
            "sub_gone",
            EventKind::Subscription(snap),
        );
        ev.account = account;
        let outcome = h
            .billing
            .apply_event(&h.pool, &h.quotas, ev)
            .await
            .expect("unowned");
        assert_eq!(outcome, expected, "{account:?} {status:?}");
    }
}

#[tokio::test]
#[ignore]
async fn two_plan_prices_on_one_subscription_are_refused() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    let mut snap = snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, Utc::now());
    snap.price_refs.push(PRO_MONTH.into());
    let ev = event(account, &sub, EventKind::Subscription(snap));
    let err = h
        .billing
        .apply_event(&h.pool, &h.quotas, ev)
        .await
        .expect_err("which plan is anyone's guess");
    assert!(
        err.to_string().contains("more than one plan price"),
        "{err}"
    );
    assert_eq!(row(&h.pool, account).await.plan_id, "founding");
}

#[tokio::test]
#[ignore]
async fn an_unknown_price_is_refused_so_the_provider_retries_after_the_row_is_added() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    let price = format!("pri_new_{}", Uuid::now_v7().simple());
    let snap = snapshot(&sub, SubscriptionStatus::Active, &price, Utc::now());
    let ev = event(account, &sub, EventKind::Subscription(snap));

    assert!(
        h.billing
            .apply_event(&h.pool, &h.quotas, ev.clone())
            .await
            .is_err()
    );
    assert_eq!(row(&h.pool, account).await.plan_id, "founding");

    sqlx::query(
        "INSERT INTO plan_prices (provider, price_ref, plan_id, interval, amount_minor, currency) \
         VALUES ('fake', $1, 'team', 'year', 19000, 'USD') \
         ON CONFLICT (provider, plan_id, interval) DO UPDATE SET price_ref = EXCLUDED.price_ref",
    )
    .bind(&price)
    .execute(&h.pool)
    .await
    .expect("add price");
    let outcome = h
        .billing
        .apply_event(&h.pool, &h.quotas, ev)
        .await
        .expect("retry");
    assert_eq!(
        outcome,
        Outcome::Applied,
        "the failed attempt did not claim the id"
    );
    assert_eq!(row(&h.pool, account).await.plan_id, "team");
}

#[tokio::test]
#[ignore]
async fn an_operator_move_keeps_the_fallback_a_later_cancel_lands_on() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    set_plan(
        &h.pool,
        &h.quotas,
        account,
        PlanRequest {
            plan_id: "pro",
            fallback_plan_id: None,
            reason: "granted",
            actor: Actor::Operator,
        },
    )
    .await
    .expect("grant");
    activate(&h, account, &sub, TEAM_MONTH).await;
    let ended = snapshot(&sub, SubscriptionStatus::Canceled, TEAM_MONTH, Utc::now());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(ended)),
        )
        .await
        .expect("ended");
    assert_eq!(row(&h.pool, account).await.plan_id, "founding");
}

#[tokio::test]
#[ignore]
async fn an_operator_cannot_move_a_paying_account() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;
    let grant = || PlanRequest {
        plan_id: "pro",
        fallback_plan_id: None,
        reason: "granted",
        actor: Actor::Operator,
    };
    let refused = |err: uptimepage::error::AppError| matches!(err, uptimepage::error::AppError::Conflict { code, .. } if code == "SUBSCRIPTION_STATE");

    let err = set_plan(&h.pool, &h.quotas, account, grant())
        .await
        .expect_err("paid");
    assert!(refused(err));
    assert_eq!(row(&h.pool, account).await.plan_id, "team");
    set_plan(
        &h.pool,
        &h.quotas,
        account,
        PlanRequest {
            plan_id: "team",
            fallback_plan_id: Some("pro"),
            reason: "lands on pro when this ends",
            actor: Actor::Operator,
        },
    )
    .await
    .expect("the fallback alone is still the operator's");
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.fallback_plan_id.as_deref()),
        ("team", Some("pro"))
    );

    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::PaymentFailed),
        )
        .await
        .expect("failed");
    let err = set_plan(&h.pool, &h.quotas, account, grant())
        .await
        .expect_err("unpaid still holds the plan");
    assert!(refused(err));

    let ended = snapshot(&sub, SubscriptionStatus::Canceled, TEAM_MONTH, Utc::now());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(ended)),
        )
        .await
        .expect("ended");
    set_plan(&h.pool, &h.quotas, account, grant())
        .await
        .expect("nothing paid any more");
    assert_eq!(row(&h.pool, account).await.plan_id, "pro");
}

#[tokio::test]
#[ignore]
async fn the_provider_ending_an_unpaid_subscription_says_the_payment_never_came() {
    let Some(h) = harness().await else { return };
    let (account, org, _) = account(&h.pool, "founding", 52).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::PaymentFailed),
        )
        .await
        .expect("failed");

    let ended = snapshot(&sub, SubscriptionStatus::Canceled, TEAM_MONTH, Utc::now());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(ended)),
        )
        .await
        .expect("ended");
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.status, s.grace_until),
        ("founding", BillingStatus::Canceled, None)
    );
    assert_eq!(held_count(&h.pool, org).await, 2);
    assert_eq!(
        subjects(&h.mail),
        vec!["payment_failed", "downgrade_applied"]
    );
    assert!(matches!(
        h.mail.sent()[1].template,
        EmailTemplate::DowngradeApplied {
            landing: Landing::Unpaid,
            ..
        }
    ));
    let kinds = ledger_kinds(&h.pool, account).await;
    assert!(!kinds.contains(&"grace_expired".to_string()), "{kinds:?}");

    // The same ending with a cancel booked is that cancel landing.
    let (other, _, _) = crate::account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    activate(&h, other, &sub, TEAM_MONTH).await;
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(other, &sub, EventKind::PaymentFailed),
        )
        .await
        .expect("failed");
    let mut booked = snapshot(&sub, SubscriptionStatus::PastDue, TEAM_MONTH, Utc::now());
    booked.cancel_at = Some(Utc::now() + Duration::days(3));
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(other, &sub, EventKind::Subscription(booked)),
        )
        .await
        .expect("booked");
    let ended = snapshot(&sub, SubscriptionStatus::Canceled, TEAM_MONTH, Utc::now());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(other, &sub, EventKind::Subscription(ended)),
        )
        .await
        .expect("ended");
    assert_eq!(row(&h.pool, other).await.status, BillingStatus::Canceled);
    assert!(matches!(
        h.mail.sent().last().expect("landed").template,
        EmailTemplate::DowngradeApplied {
            landing: Landing::Canceled,
            ..
        }
    ));
}

#[tokio::test]
#[ignore]
async fn an_unpaid_account_leaving_is_not_told_its_payment_missed_a_deadline() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::PaymentFailed),
        )
        .await
        .expect("failed");
    h.provider.subscriptions.lock().unwrap().insert(
        sub.clone(),
        snapshot(&sub, SubscriptionStatus::PastDue, TEAM_MONTH, Utc::now()),
    );
    h.billing
        .end_for_deletion(&h.pool, &h.quotas, account)
        .await
        .expect("ended");
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.status),
        ("founding", BillingStatus::Canceled)
    );
    assert!(matches!(
        h.mail.sent().last().expect("landed").template,
        EmailTemplate::DowngradeApplied {
            landing: Landing::Canceled,
            ..
        }
    ));
}

#[tokio::test]
#[ignore]
async fn a_cancel_booked_while_unpaid_lands_on_the_row_and_can_be_withdrawn() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::PaymentFailed),
        )
        .await
        .expect("failed");

    let deadline = row(&h.pool, account).await.grace_until.expect("grace");
    let ends_at = now_us() + Duration::days(30);
    let mut booked = snapshot(&sub, SubscriptionStatus::PastDue, TEAM_MONTH, Utc::now());
    booked.cancel_at = Some(ends_at);
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(booked)),
        )
        .await
        .expect("booked");
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.status, s.cancel_at, s.plan_id.as_str()),
        (BillingStatus::PastDue, Some(ends_at), "team")
    );
    assert_eq!(
        subjects(&h.mail),
        vec!["payment_failed", "subscription_canceled"]
    );
    assert!(
        matches!(
            h.mail.sent()[1].template,
            EmailTemplate::SubscriptionCanceled { ends_at, .. } if ends_at == deadline
        ),
        "service ends at the deadline unless paid, whatever the cancel date"
    );
    assert!(
        ledger_kinds(&h.pool, account)
            .await
            .contains(&"cancel_scheduled".to_string())
    );

    let withdrawn = snapshot(&sub, SubscriptionStatus::PastDue, TEAM_MONTH, Utc::now());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(withdrawn)),
        )
        .await
        .expect("withdrawn");
    let s = row(&h.pool, account).await;
    assert_eq!((s.status, s.cancel_at), (BillingStatus::PastDue, None));
    assert_eq!(
        ledger_kinds(&h.pool, account)
            .await
            .last()
            .map(String::as_str),
        Some("pending_change_cleared")
    );

    // A view whose cancel date has passed is over, not a booking to announce.
    let mut over = snapshot(&sub, SubscriptionStatus::PastDue, TEAM_MONTH, Utc::now());
    over.cancel_at = Some(Utc::now() - Duration::seconds(1));
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(over)),
        )
        .await
        .expect("over");
    let s = row(&h.pool, account).await;
    assert_eq!(
        (s.plan_id.as_str(), s.status, s.cancel_at),
        ("founding", BillingStatus::Canceled, None)
    );
    assert_eq!(
        subjects(&h.mail),
        vec![
            "payment_failed",
            "subscription_canceled",
            "downgrade_applied"
        ]
    );
    assert!(matches!(
        h.mail.sent()[2].template,
        EmailTemplate::DowngradeApplied {
            landing: Landing::Canceled,
            ..
        }
    ));
}

// The HTTP surface: the receiver and the owner's actions.

/// The receiver answers before its after-effects run, so a test that posts
/// a webhook asserts rows; provider calls and mails are asserted through the
/// harness, whose `apply_event` waits for them.
async fn app_with_fake(pool: &PgPool, fake: Arc<FakeProvider>) -> (axum::Router, OrgId) {
    build_test_app_with_pg_store_tweaked(
        pool.clone(),
        |_| {},
        move |state| state.with_billing_provider(fake),
    )
    .await
}

async fn owner_account(pool: &PgPool, org: OrgId) -> AccountId {
    uptimepage::storage::accounts::account_for_org(pool, org)
        .await
        .expect("account")
}

fn webhook(body: &ProviderEvent, signed: bool) -> Request<Body> {
    let mut req =
        Request::post("/hooks/billing/fake").header(header::CONTENT_TYPE, "application/json");
    if signed {
        req = req.header(SIGNATURE_HEADER, SIGNATURE);
    }
    req.body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn the_receiver_checks_the_signature_then_applies_and_acknowledges() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    seed_prices(&pool).await;
    let (app, org) = app_with_fake(&pool, Arc::new(FakeProvider::default())).await;
    let account = owner_account(&pool, org).await;
    let sub = sub_ref();
    let snap = snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, Utc::now());
    let ev = event(account, &sub, EventKind::Subscription(snap));

    let resp = app.clone().oneshot(webhook(&ev, false)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(row(&pool, account).await.plan_id, "free");

    let resp = app.clone().oneshot(webhook(&ev, true)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(row(&pool, account).await.plan_id, "team");

    let resp = app.clone().oneshot(webhook(&ev, true)).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a redelivery is acknowledged"
    );

    let garbage = Request::post("/hooks/billing/fake")
        .header(SIGNATURE_HEADER, SIGNATURE)
        .body(Body::from("not json"))
        .unwrap();
    let resp = app.clone().oneshot(garbage).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a retry would replay the same bytes"
    );

    let other = Request::post("/hooks/billing/stripe")
        .header(SIGNATURE_HEADER, SIGNATURE)
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.oneshot(other).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore]
async fn a_first_payment_whose_lookup_stalls_is_refused_within_the_providers_patience() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    seed_prices(&pool).await;
    let fake = Arc::new(FakeProvider::default());
    let (app, org) = app_with_fake(&pool, fake.clone()).await;
    let account = owner_account(&pool, org).await;
    let sub = sub_ref();
    fake.subscriptions.lock().unwrap().insert(
        sub.clone(),
        snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, Utc::now()),
    );
    let paid = event(account, &sub, paid());
    let stalled = || {
        metric_value(
            &metrics_handle().render(),
            r#"uptimepage_billing_webhook_rejected_total{reason="stalled"}"#,
        )
        .unwrap_or(0.0)
    };
    let before = stalled();

    fake.fetch_stalls.store(true, Ordering::Relaxed);
    let asked = std::time::Instant::now();
    let resp = app.clone().oneshot(webhook(&paid, true)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        asked.elapsed() < ACK_BUDGET + std::time::Duration::from_secs(1),
        "answered in {:?}",
        asked.elapsed()
    );
    assert_eq!(row(&pool, account).await.status, BillingStatus::None);
    assert_eq!(stalled(), before + 1.0, "counted apart from a failure");

    fake.fetch_stalls.store(false, Ordering::Relaxed);
    let resp = app.clone().oneshot(webhook(&paid, true)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "the redelivery is taken");
    let s = row(&pool, account).await;
    assert_eq!(s.status, BillingStatus::Active);
    assert_eq!(s.plan_id, "team");
}

/// A price on another cadence is two provider requests, and the webhook for
/// the first lands before the second returns. It waits for the change's
/// turn, then reads as older than the answer already written, so the
/// landing date and the one mail carry the paid period end, not the
/// provider's in-between state.
#[tokio::test]
#[ignore]
async fn a_webhook_racing_a_change_waits_and_is_then_stale() {
    let Some(h) = isolated("billing").await else {
        return;
    };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    let live = snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, now_us());
    let paid_until = live.period_end.expect("a billed period");
    h.provider
        .subscriptions
        .lock()
        .unwrap()
        .insert(sub.clone(), live.clone());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(live)),
        )
        .await
        .expect("apply");
    h.provider.change_stall_ms.store(2000, Ordering::Relaxed);

    let (changed, raced) = tokio::join!(
        h.billing
            .change_plan(&h.pool, &h.quotas, account, "pro", Interval::Year),
        async {
            h.provider.change_entered.notified().await;
            let mut between = snapshot(&sub, SubscriptionStatus::Active, PRO_YEAR, now_us());
            between.period_end = Some(paid_until + Duration::days(335));
            h.billing
                .apply_event(
                    &h.pool,
                    &h.quotas,
                    event(account, &sub, EventKind::Subscription(between)),
                )
                .await
        }
    );
    changed.expect("change");
    assert_eq!(raced.expect("webhook"), Outcome::Stale);

    let s = row(&h.pool, account).await;
    assert_eq!(s.plan_id, "team");
    assert_eq!(s.pending_plan_id.as_deref(), Some("pro"));
    assert_eq!(s.pending_interval, Some(Interval::Year));
    assert_eq!(s.plan_change_at, Some(paid_until));
    assert_eq!(subjects(&h.mail), vec!["downgrade_scheduled"]);
    match &h.mail.sent()[0].template {
        EmailTemplate::DowngradeScheduled { at, .. } => assert_eq!(*at, paid_until),
        other => panic!("{other:?}"),
    }
    h.finish().await;
}

/// Two processes hold no common turn, so the in-between state of a change
/// can still be read first there. A move read that way lands on the period
/// end the row already paid for, whichever way the provider's restarted
/// period points, and a later period end never pushes a booked move out.
#[tokio::test]
#[ignore]
async fn a_move_read_in_between_lands_on_the_period_paid_for() {
    let Some(h) = isolated("billing").await else {
        return;
    };
    for (paid, booked, restarted) in [
        (TEAM_MONTH, PRO_YEAR, Duration::days(365)),
        (TEAM_YEAR, PRO_MONTH, Duration::days(30)),
    ] {
        let (account, _, _) = account(&h.pool, "founding", 0).await;
        let sub = sub_ref();
        let mut live = snapshot(&sub, SubscriptionStatus::Active, paid, now_us());
        if paid == TEAM_YEAR {
            live.period_end = Some(live.taken_at + Duration::days(365));
        }
        let paid_until = live.period_end.expect("a billed period");
        h.billing
            .apply_event(
                &h.pool,
                &h.quotas,
                event(account, &sub, EventKind::Subscription(live)),
            )
            .await
            .expect("apply");
        h.mail.clear();

        let mut between = snapshot(&sub, SubscriptionStatus::Active, booked, now_us());
        between.period_end = Some(between.taken_at + restarted);
        h.billing
            .apply_event(
                &h.pool,
                &h.quotas,
                event(account, &sub, EventKind::Subscription(between)),
            )
            .await
            .expect("apply");
        let s = row(&h.pool, account).await;
        assert_eq!(s.plan_id, "team");
        assert_eq!(s.pending_plan_id.as_deref(), Some("pro"));
        assert_eq!(s.plan_change_at, Some(paid_until), "{paid} -> {booked}");
        assert_eq!(subjects(&h.mail), vec!["downgrade_scheduled"]);
        match &h.mail.sent()[0].template {
            EmailTemplate::DowngradeScheduled { at, .. } => assert_eq!(*at, paid_until),
            other => panic!("{other:?}"),
        }

        let mut pinned = snapshot(&sub, SubscriptionStatus::Active, booked, now_us());
        pinned.period_end = Some(paid_until);
        h.billing
            .apply_event(
                &h.pool,
                &h.quotas,
                event(account, &sub, EventKind::Subscription(pinned)),
            )
            .await
            .expect("apply");
        let mut renewal = snapshot(&sub, SubscriptionStatus::Active, booked, now_us());
        renewal.period_end = Some(paid_until + Duration::days(400));
        h.billing
            .apply_event(
                &h.pool,
                &h.quotas,
                event(account, &sub, EventKind::Subscription(renewal)),
            )
            .await
            .expect("apply");
        let s = row(&h.pool, account).await;
        assert_eq!(s.plan_change_at, Some(paid_until), "{paid} -> {booked}");
        assert_eq!(subjects(&h.mail), vec!["downgrade_scheduled"], "told once");
    }
    h.finish().await;
}

/// Two submissions for one account decide one after the other, on the row
/// as the first left it: the second cannot pass a check the first just
/// closed, nor bill at once for a price the first made a cadence change.
#[tokio::test]
#[ignore]
async fn a_second_change_decides_on_the_row_the_first_left() {
    let Some(h) = isolated("billing").await else {
        return;
    };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    let live = snapshot(&sub, SubscriptionStatus::Active, PRO_MONTH, Utc::now());
    h.provider
        .subscriptions
        .lock()
        .unwrap()
        .insert(sub.clone(), live.clone());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(live)),
        )
        .await
        .expect("apply");
    h.provider.change_stall_ms.store(2000, Ordering::Relaxed);

    let (first, second) = tokio::join!(
        h.billing
            .change_plan(&h.pool, &h.quotas, account, "team", Interval::Month),
        async {
            h.provider.change_entered.notified().await;
            h.billing
                .change_plan(&h.pool, &h.quotas, account, "team", Interval::Year)
                .await
        }
    );
    first.expect("first");
    second.expect("second");
    let calls = h.provider.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec![
            format!("change:{sub}:pri_team_month:Now"),
            format!("change:{sub}:pri_team_year:NextPeriod"),
        ],
        "the yearly price is a cadence change on the plan already bought, not an upgrade to bill now"
    );
    h.finish().await;
}

/// A move is booked on the period end the row already paid for, whatever
/// period the provider shows for it; only a row whose period is over takes
/// the provider's, since that is a renewal the row has not heard of.
#[tokio::test]
#[ignore]
async fn a_move_lands_on_the_rows_period_end_unless_that_is_over() {
    let Some(h) = isolated("billing").await else {
        return;
    };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    let mut live = snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, now_us());
    live.period_end = Some(live.taken_at - Duration::days(1));
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(live.clone())),
        )
        .await
        .expect("apply");
    let renewed_until = live.taken_at + Duration::days(29);
    let mut renewed = live.clone();
    renewed.period_end = Some(renewed_until);
    h.provider
        .subscriptions
        .lock()
        .unwrap()
        .insert(sub.clone(), renewed);

    h.billing
        .change_plan(&h.pool, &h.quotas, account, "pro", Interval::Month)
        .await
        .expect("book");
    let s = row(&h.pool, account).await;
    assert_eq!(s.pending_plan_id.as_deref(), Some("pro"));
    assert_eq!(
        s.plan_change_at,
        Some(renewed_until),
        "the provider's, the row's being over"
    );
    assert_eq!(s.current_period_end, Some(renewed_until));

    h.provider
        .subscriptions
        .lock()
        .unwrap()
        .get_mut(&sub)
        .unwrap()
        .period_end = Some(renewed_until + Duration::days(335));
    h.billing
        .change_plan(&h.pool, &h.quotas, account, "pro", Interval::Year)
        .await
        .expect("rebook");
    let s = row(&h.pool, account).await;
    assert_eq!(s.pending_interval, Some(Interval::Year));
    assert_eq!(
        s.plan_change_at,
        Some(renewed_until),
        "the row's, still ahead"
    );
    assert_eq!(s.current_period_end, Some(renewed_until));
    assert_eq!(subjects(&h.mail), vec!["downgrade_scheduled"], "told once");
    h.finish().await;
}

/// A redelivery of an event already applied is answered at once, without
/// waiting for a change that holds the account.
#[tokio::test]
#[ignore]
async fn a_redelivery_is_answered_without_the_accounts_turn() {
    let Some(h) = isolated("billing").await else {
        return;
    };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    let live = snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, Utc::now());
    h.provider
        .subscriptions
        .lock()
        .unwrap()
        .insert(sub.clone(), live.clone());
    let ev = event(account, &sub, EventKind::Subscription(live));
    h.billing
        .apply_event(&h.pool, &h.quotas, ev.clone())
        .await
        .expect("apply");
    h.provider.change_stall_ms.store(2000, Ordering::Relaxed);

    let (changed, again) = tokio::join!(
        h.billing
            .change_plan(&h.pool, &h.quotas, account, "pro", Interval::Month),
        async {
            h.provider.change_entered.notified().await;
            let started = std::time::Instant::now();
            let outcome = h.billing.apply_event(&h.pool, &h.quotas, ev).await;
            (outcome, started.elapsed())
        }
    );
    changed.expect("change");
    let (outcome, took) = again;
    assert_eq!(outcome.expect("redelivery"), Outcome::Duplicate);
    assert!(took < std::time::Duration::from_secs(1), "waited {took:?}");
    h.finish().await;
}

/// A change waits for the one ahead of it, no longer: past the patience it
/// is refused rather than queued behind a provider that is slow to answer.
#[tokio::test]
#[ignore]
async fn a_change_behind_a_slow_one_is_refused_not_queued() {
    let Some(h) = isolated("billing").await else {
        return;
    };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    let live = snapshot(&sub, SubscriptionStatus::Active, PRO_MONTH, Utc::now());
    h.provider
        .subscriptions
        .lock()
        .unwrap()
        .insert(sub.clone(), live.clone());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &sub, EventKind::Subscription(live)),
        )
        .await
        .expect("apply");
    h.provider.change_stall_ms.store(6000, Ordering::Relaxed);

    let (first, second) = tokio::join!(
        h.billing
            .change_plan(&h.pool, &h.quotas, account, "team", Interval::Month),
        async {
            h.provider.change_entered.notified().await;
            h.billing
                .change_plan(&h.pool, &h.quotas, account, "team", Interval::Year)
                .await
        }
    );
    first.expect("first");
    let err = second.expect_err("second");
    assert!(
        matches!(&err, uptimepage::error::AppError::Conflict { code, .. } if *code == "SUBSCRIPTION_STATE"),
        "{err:?}"
    );
    let calls = h.provider.calls.lock().unwrap().clone();
    assert_eq!(calls, vec![format!("change:{sub}:pri_team_month:Now")]);
    assert_eq!(row(&h.pool, account).await.plan_id, "team");
    h.finish().await;
}

async fn body_text(resp: axum::http::Response<Body>) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 8 << 20)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

fn api(method: &str, path: &str, body: Option<Value>) -> Request<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("X-Requested-With", "uptimepage");
    if body.is_some() {
        req = req.header(header::CONTENT_TYPE, "application/json");
    }
    req.body(match body {
        Some(v) => Body::from(v.to_string()),
        None => Body::empty(),
    })
    .unwrap()
}

#[tokio::test]
#[ignore]
async fn the_owner_buys_moves_up_moves_down_and_cancels_through_the_api() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    seed_prices(&pool).await;
    let fake = Arc::new(FakeProvider::default());
    let (app, org) = app_with_fake(&pool, fake.clone()).await;
    let account = owner_account(&pool, org).await;
    let sub = sub_ref();

    let resp = app
        .clone()
        .oneshot(api("GET", "/api/v1/account/billing", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let view = body_json(resp).await;
    assert_eq!(view["status"], "none");
    assert_eq!(view["portal_available"], false);
    assert!(
        view["offers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["plan_id"] == "team"
                && o["interval"] == "month"
                && o["amount_minor"] == 1900
                && o["currency"] == "USD"),
        "{view}"
    );

    let resp = app
        .clone()
        .oneshot(api(
            "POST",
            "/api/v1/account/billing/checkout",
            // "free" is never priced, so this exercises the not-for-sale path
            // without depending on which prices other tests leave in the
            // shared plan_prices table.
            Some(json!({ "plan_id": "free", "interval": "month", "accept_terms": true })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(resp).await["error"]["code"], "PLAN_NOT_FOR_SALE");

    let resp = app
        .clone()
        .oneshot(api(
            "POST",
            "/api/v1/account/billing/checkout",
            Some(json!({ "plan_id": "pro", "interval": "month" })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(resp).await["error"]["code"], "TERMS_NOT_ACCEPTED");

    let resp = app
        .clone()
        .oneshot(api(
            "POST",
            "/api/v1/account/billing/checkout",
            Some(json!({ "plan_id": "pro", "interval": "month", "accept_terms": true })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let url = body_json(resp).await["url"].as_str().unwrap().to_owned();
    assert!(url.starts_with("https://pay.example.test/"), "{url}");
    assert!(
        ledger_kinds(&pool, account)
            .await
            .contains(&"checkout_started".to_string())
    );

    // The provider confirms the purchase.
    let snap = snapshot(&sub, SubscriptionStatus::Active, PRO_MONTH, Utc::now());
    fake.subscriptions
        .lock()
        .unwrap()
        .insert(sub.clone(), snap.clone());
    let resp = app
        .clone()
        .oneshot(webhook(
            &event(account, &sub, EventKind::Subscription(snap)),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(api(
            "POST",
            "/api/v1/account/billing/checkout",
            Some(json!({ "plan_id": "team", "interval": "month", "accept_terms": true })),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "one subscription per account"
    );

    let resp = app
        .clone()
        .oneshot(api(
            "PUT",
            "/api/v1/account/billing/plan",
            Some(json!({ "plan_id": "team", "interval": "month" })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let view = body_json(resp).await;
    assert_eq!(view["plan_id"], "team", "an upgrade applies at once");
    assert_eq!(view["pending_plan_id"], Value::Null);
    assert_eq!(view["interval"], "month");
    assert!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| *c == format!("change:{sub}:pri_team_month:Now"))
    );

    let calls_before = fake.calls.lock().unwrap().len();
    let resp = app
        .clone()
        .oneshot(api(
            "PUT",
            "/api/v1/account/billing/plan",
            Some(json!({ "plan_id": "team", "interval": "month" })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT, "nothing to change");
    assert_eq!(body_json(resp).await["error"]["code"], "SUBSCRIPTION_STATE");
    assert_eq!(fake.calls.lock().unwrap().len(), calls_before);

    let resp = app
        .clone()
        .oneshot(api(
            "PUT",
            "/api/v1/account/billing/plan",
            Some(json!({ "plan_id": "pro", "interval": "month" })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let view = body_json(resp).await;
    assert_eq!(view["plan_id"], "team", "a downgrade waits");
    assert_eq!(view["pending_plan_id"], "pro");
    assert_eq!(view["pending_interval"], "month");
    assert!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| *c == format!("change:{sub}:pri_pro_month:NextPeriod"))
    );

    let calls_before = fake.calls.lock().unwrap().len();
    let resp = app
        .clone()
        .oneshot(api(
            "PUT",
            "/api/v1/account/billing/plan",
            Some(json!({ "plan_id": "pro", "interval": "month" })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT, "that move is booked");
    assert_eq!(body_json(resp).await["error"]["code"], "SUBSCRIPTION_STATE");
    assert_eq!(fake.calls.lock().unwrap().len(), calls_before);
    let resp = app
        .clone()
        .oneshot(api(
            "PUT",
            "/api/v1/account/billing/plan",
            Some(json!({ "plan_id": "pro", "interval": "year" })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "the same move, yearly");
    let view = body_json(resp).await;
    assert_eq!(view["pending_plan_id"], "pro");
    assert_eq!(view["pending_interval"], "year");
    assert_eq!(view["interval"], "month", "still paying monthly");
    assert_eq!(
        ledger_kinds(&pool, account)
            .await
            .iter()
            .filter(|k| *k == "downgrade_scheduled")
            .count(),
        2,
        "the switch of cadence is on the ledger"
    );
    assert_eq!(
        fake.calls.lock().unwrap().last().unwrap(),
        &format!("change:{sub}:pri_pro_year:NextPeriod")
    );
    let resp = app
        .clone()
        .oneshot(api(
            "PUT",
            "/api/v1/account/billing/plan",
            Some(json!({ "plan_id": "pro", "interval": "year" })),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "booked at that cadence now"
    );

    let resp = app
        .clone()
        .oneshot(api(
            "PUT",
            "/api/v1/account/billing/plan",
            Some(json!({ "plan_id": "team", "interval": "month" })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let view = body_json(resp).await;
    assert_eq!(view["plan_id"], "team");
    assert_eq!(view["pending_plan_id"], Value::Null, "the downgrade is off");
    assert_eq!(
        fake.calls.lock().unwrap().last().unwrap(),
        &format!("change:{sub}:pri_team_month:NextPeriod"),
        "no proration on a period already paid at that price; the booked yearly price is what the provider bills on"
    );

    let resp = app
        .clone()
        .oneshot(api(
            "PUT",
            "/api/v1/account/billing/plan",
            Some(json!({ "plan_id": "pro", "interval": "month" })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    for attempt in ["booked", "a second click"] {
        let resp = app
            .clone()
            .oneshot(api("POST", "/api/v1/account/billing/cancel", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{attempt}");
        let view = body_json(resp).await;
        assert!(!view["cancel_at"].is_null(), "{attempt}");
        assert_eq!(
            view["pending_plan_id"],
            Value::Null,
            "a cancel outranks the booked move"
        );
        assert_eq!(view["status"], "active");
    }
    let calls_before = fake.calls.lock().unwrap().len();
    let resp = app
        .clone()
        .oneshot(api(
            "PUT",
            "/api/v1/account/billing/plan",
            Some(json!({ "plan_id": "pro", "interval": "month" })),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "withdraw the cancel first"
    );
    assert_eq!(body_json(resp).await["error"]["code"], "SUBSCRIPTION_STATE");
    assert_eq!(
        fake.calls.lock().unwrap().len(),
        calls_before,
        "the provider is not asked to move a subscription that is ending"
    );

    let resp = app
        .clone()
        .oneshot(api("DELETE", "/api/v1/account/billing/cancel", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let view = body_json(resp).await;
    assert_eq!(
        view["pending_plan_id"], "pro",
        "the booked downgrade is back"
    );

    let resp = app
        .clone()
        .oneshot(api("POST", "/api/v1/account/billing/portal", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        body_json(resp).await["url"]
            .as_str()
            .unwrap()
            .contains("ctm_1")
    );

    let resp = app
        .oneshot(api("DELETE", "/api/v1/account/billing/cancel", None))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "nothing left to withdraw"
    );
}

/// An owner whose renewal failed, as the receiver saw it: bound by an active
/// view, then a failed payment, with the fake now holding the subscription
/// unpaid. Answers the subscription ref with the provider's call log empty.
async fn unpaid_owner(
    app: &axum::Router,
    fake: &FakeProvider,
    pool: &PgPool,
    account: AccountId,
) -> String {
    let sub = sub_ref();
    let snap = snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, Utc::now());
    fake.subscriptions
        .lock()
        .unwrap()
        .insert(sub.clone(), snap.clone());
    for kind in [EventKind::Subscription(snap), EventKind::PaymentFailed] {
        let resp = app
            .clone()
            .oneshot(webhook(&event(account, &sub, kind), true))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    fake.subscriptions
        .lock()
        .unwrap()
        .get_mut(&sub)
        .unwrap()
        .status = SubscriptionStatus::PastDue;
    let before = row(pool, account).await;
    assert_eq!(before.status, BillingStatus::PastDue);
    assert!(before.grace_until.is_some());
    fake.calls.lock().unwrap().clear();
    sub
}

#[tokio::test]
#[ignore]
async fn an_unpaid_owner_leaves_at_once_and_nothing_is_booked() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    seed_prices(&pool).await;
    let fake = Arc::new(FakeProvider::default());
    let (app, org) = app_with_fake(&pool, fake.clone()).await;
    let account = owner_account(&pool, org).await;
    let sub = unpaid_owner(&app, &fake, &pool, account).await;

    let resp = app
        .clone()
        .oneshot(api("POST", "/api/v1/account/billing/cancel", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let view = body_json(resp).await;
    assert_eq!(
        (&view["status"], &view["plan_id"]),
        (&json!("canceled"), &view["fallback_plan_id"]),
        "{view}"
    );
    assert_eq!(
        *fake.calls.lock().unwrap(),
        vec![format!("cancel:{sub}:Now")],
        "unpaid ends now, nothing is booked"
    );
    let after = row(&pool, account).await;
    assert_eq!(
        (after.status, after.grace_until, after.cancel_at),
        (BillingStatus::Canceled, None, None)
    );
    assert!(
        ledger_kinds(&pool, account)
            .await
            .contains(&"subscription_ended".to_string())
    );

    let resp = app
        .oneshot(api("POST", "/api/v1/account/billing/cancel", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT, "nothing left to end");
    assert_eq!(body_json(resp).await["error"]["code"], "SUBSCRIPTION_STATE");
}

#[tokio::test]
#[ignore]
async fn an_unpaid_owner_whose_end_is_booked_is_told_the_provider_keeps_it() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    seed_prices(&pool).await;
    let fake = Arc::new(FakeProvider::default());
    let (app, org) = app_with_fake(&pool, fake.clone()).await;
    let account = owner_account(&pool, org).await;
    let sub = unpaid_owner(&app, &fake, &pool, account).await;
    // Booked through the provider's portal, so this side never saw it.
    {
        let mut subs = fake.subscriptions.lock().unwrap();
        let held = subs.get_mut(&sub).unwrap();
        held.cancel_at = held.period_end;
    }

    let resp = app
        .oneshot(api("POST", "/api/v1/account/billing/cancel", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        body_json(resp).await["error"]["code"],
        "BILLING_PROVIDER_REFUSED"
    );
    assert_eq!(
        *fake.calls.lock().unwrap(),
        vec![format!("cancel:{sub}:Now"), format!("fetch:{sub}")],
        "asked to end now, shown the booked end instead"
    );
    let s = row(&pool, account).await;
    assert_eq!(s.status, BillingStatus::PastDue);
    assert!(
        s.cancel_at.is_some(),
        "the booked end the answer showed landed"
    );
}

#[tokio::test]
#[ignore]
async fn only_the_payer_sees_or_moves_the_subscription() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let fake = Arc::new(FakeProvider::default());
    let (app, org) = build_test_app_with_pg_store_anon_tweaked(
        pool.clone(),
        |_| {},
        move |state| state.with_billing_provider(fake),
    )
    .await;
    let owner = make_user(&pool, "payer").await;
    sqlx::query(
        "UPDATE organizations SET account_id = a.id FROM accounts a \
         WHERE organizations.id = $1 AND a.owner_user_id = $2",
    )
    .bind(org.0)
    .bind(owner.0)
    .execute(&pool)
    .await
    .expect("point the org at the payer's account");
    let member = make_user(&pool, "member").await;
    for (user, role) in [(owner, "owner"), (member, "owner")] {
        sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, $3)")
            .bind(user.0)
            .bind(org.0)
            .bind(role)
            .execute(&pool)
            .await
            .expect("membership");
    }

    let as_member = with_session(app.clone(), member, Some(org), None);
    for req in [
        api("GET", "/api/v1/account/billing", None),
        api(
            "POST",
            "/api/v1/account/billing/checkout",
            Some(json!({ "plan_id": "pro", "interval": "month" })),
        ),
        api("POST", "/api/v1/account/billing/cancel", None),
    ] {
        let resp = as_member.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            body_json(resp).await["error"]["code"],
            "ACCOUNT_OWNER_REQUIRED"
        );
    }

    let as_owner = with_session(app, owner, Some(org), None);
    let resp = as_owner
        .oneshot(api("GET", "/api/v1/account/billing", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
#[ignore]
async fn leaving_ends_the_subscription_first() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    seed_prices(&pool).await;
    let fake = Arc::new(FakeProvider::default());
    let (app, org) = app_with_fake(&pool, fake.clone()).await;
    let account = owner_account(&pool, org).await;
    let sub = sub_ref();
    let snap = snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, Utc::now());
    let resp = app
        .clone()
        .oneshot(webhook(
            &event(account, &sub, EventKind::Subscription(snap.clone())),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(api("DELETE", "/api/v1/me", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(resp).await["error"]["code"],
        "SUBSCRIPTION_NOT_FOUND"
    );
    let s = row(&pool, account).await;
    assert_eq!(s.status, BillingStatus::Active);
    assert!(s.owner.is_some());
    let (deleted,): (bool,) =
        sqlx::query_as("SELECT deleted_at IS NOT NULL FROM users WHERE id = $1")
            .bind(s.owner.unwrap().0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!deleted);

    fake.subscriptions.lock().unwrap().insert(sub.clone(), snap);
    let resp = app
        .oneshot(api("DELETE", "/api/v1/me", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        fake.calls
            .lock()
            .unwrap()
            .contains(&format!("cancel:{sub}:NextPeriod")),
        "paid up, so the period runs out rather than being cut: {:?}",
        fake.calls.lock().unwrap()
    );
    let s = row(&pool, account).await;
    assert!(s.cancel_at.is_some());
    assert!(
        ledger_kinds(&pool, account)
            .await
            .contains(&"cancel_scheduled".to_string())
    );
}

#[tokio::test]
#[ignore]
async fn leaving_after_a_booked_cancel_checks_it_still_stands() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    seed_prices(&pool).await;
    let fake = Arc::new(FakeProvider::default());
    let (app, org) = app_with_fake(&pool, fake.clone()).await;
    let account = owner_account(&pool, org).await;
    let sub = sub_ref();
    let snap = snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, Utc::now());
    fake.subscriptions
        .lock()
        .unwrap()
        .insert(sub.clone(), snap.clone());
    let resp = app
        .clone()
        .oneshot(webhook(
            &event(account, &sub, EventKind::Subscription(snap)),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app
        .clone()
        .oneshot(api("POST", "/api/v1/account/billing/cancel", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(row(&pool, account).await.cancel_at.is_some());
    // Removed at the provider's dashboard; the event saying so was lost.
    fake.subscriptions
        .lock()
        .unwrap()
        .get_mut(&sub)
        .unwrap()
        .cancel_at = None;
    fake.calls.lock().unwrap().clear();

    let resp = app
        .oneshot(api("DELETE", "/api/v1/me", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        *fake.calls.lock().unwrap(),
        vec![format!("cancel:{sub}:NextPeriod")],
        "a booking this side remembers is not taken on trust"
    );
    assert!(fake.subscriptions.lock().unwrap()[&sub].cancel_at.is_some());
    let (deleted,): (bool,) =
        sqlx::query_as("SELECT deleted_at IS NOT NULL FROM users WHERE id = $1")
            .bind(row(&pool, account).await.owner.unwrap().0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(deleted);
}

#[tokio::test]
#[ignore]
async fn leaving_takes_a_refused_cancel_as_done_when_the_provider_shows_it_ending() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    seed_prices(&pool).await;
    let fake = Arc::new(FakeProvider::default());
    let (app, org) = app_with_fake(&pool, fake.clone()).await;
    let account = owner_account(&pool, org).await;
    let sub = sub_ref();
    let snap = snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, Utc::now());
    let resp = app
        .clone()
        .oneshot(webhook(
            &event(account, &sub, EventKind::Subscription(snap.clone())),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // The provider already holds the cancel an earlier attempt never recorded.
    fake.subscriptions.lock().unwrap().insert(
        sub.clone(),
        SubscriptionSnapshot {
            cancel_at: snap.period_end,
            ..snap
        },
    );
    fake.cancel_refused.store(true, Ordering::Relaxed);

    let resp = app
        .oneshot(api("DELETE", "/api/v1/me", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        *fake.calls.lock().unwrap(),
        vec![format!("cancel:{sub}:NextPeriod"), format!("fetch:{sub}")]
    );
    let s = row(&pool, account).await;
    assert!(s.cancel_at.is_some());
    assert!(
        ledger_kinds(&pool, account)
            .await
            .contains(&"cancel_scheduled".to_string())
    );
}

#[tokio::test]
#[ignore]
async fn leaving_while_unpaid_takes_a_booked_end_as_the_most_the_provider_allows() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    seed_prices(&pool).await;
    let fake = Arc::new(FakeProvider::default());
    let (app, org) = app_with_fake(&pool, fake.clone()).await;
    let account = owner_account(&pool, org).await;
    let sub = unpaid_owner(&app, &fake, &pool, account).await;
    // Booked through the provider's portal, so this side never saw it.
    {
        let mut subs = fake.subscriptions.lock().unwrap();
        let held = subs.get_mut(&sub).unwrap();
        held.cancel_at = held.period_end;
    }

    let resp = app
        .oneshot(api("DELETE", "/api/v1/me", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        *fake.calls.lock().unwrap(),
        vec![format!("cancel:{sub}:Now"), format!("fetch:{sub}")]
    );
}

#[tokio::test]
#[ignore]
async fn leaving_ends_a_subscription_the_provider_still_holds_live() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    seed_prices(&pool).await;
    let fake = Arc::new(FakeProvider::default());
    let (app, org) = app_with_fake(&pool, fake.clone()).await;
    let account = owner_account(&pool, org).await;
    let sub = sub_ref();
    let started = Utc::now() - chrono::Duration::seconds(2);
    let ended = snapshot(&sub, SubscriptionStatus::Canceled, TEAM_MONTH, started);
    for snap in [
        snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, started),
        ended,
    ] {
        let resp = app
            .clone()
            .oneshot(webhook(
                &event(account, &sub, EventKind::Subscription(snap)),
                true,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    assert_eq!(row(&pool, account).await.status, BillingStatus::Canceled);
    // The customer took the cancel back through the portal; that event was
    // lost, so the provider's view is newer and live.
    fake.subscriptions.lock().unwrap().insert(
        sub.clone(),
        snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, Utc::now()),
    );

    let resp = app
        .oneshot(api("DELETE", "/api/v1/me", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        *fake.calls.lock().unwrap(),
        vec![format!("fetch:{sub}"), format!("cancel:{sub}:NextPeriod")],
        "a row that says ended is checked before it is believed"
    );
    let s = row(&pool, account).await;
    assert_eq!(s.status, BillingStatus::Active);
    assert!(s.cancel_at.is_some());
}

#[tokio::test]
#[ignore]
async fn leaving_with_an_ended_subscription_the_provider_has_lost_goes_through() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    seed_prices(&pool).await;
    let fake = Arc::new(FakeProvider::default());
    let (app, org) = app_with_fake(&pool, fake.clone()).await;
    let account = owner_account(&pool, org).await;
    let sub = sub_ref();
    for status in [SubscriptionStatus::Active, SubscriptionStatus::Canceled] {
        let snap = snapshot(&sub, status, TEAM_MONTH, Utc::now());
        let resp = app
            .clone()
            .oneshot(webhook(
                &event(account, &sub, EventKind::Subscription(snap)),
                true,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    assert_eq!(row(&pool, account).await.status, BillingStatus::Canceled);

    let resp = app
        .oneshot(api("DELETE", "/api/v1/me", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(*fake.calls.lock().unwrap(), vec![format!("fetch:{sub}")]);
}

#[tokio::test]
#[ignore]
async fn leaving_with_an_ended_row_waits_for_the_providers_answer() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    seed_prices(&pool).await;
    let fake = Arc::new(FakeProvider::default());
    let (app, org) = app_with_fake(&pool, fake.clone()).await;
    let account = owner_account(&pool, org).await;
    let sub = sub_ref();
    let started = Utc::now() - chrono::Duration::seconds(2);
    for status in [SubscriptionStatus::Active, SubscriptionStatus::Canceled] {
        let snap = snapshot(&sub, status, TEAM_MONTH, started);
        let resp = app
            .clone()
            .oneshot(webhook(
                &event(account, &sub, EventKind::Subscription(snap)),
                true,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    assert_eq!(row(&pool, account).await.status, BillingStatus::Canceled);
    // The grace-expiry cancel never landed: the provider still bills.
    fake.subscriptions.lock().unwrap().insert(
        sub.clone(),
        snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, Utc::now()),
    );
    fake.fetch_fails.store(true, Ordering::Relaxed);

    let resp = app
        .clone()
        .oneshot(api("DELETE", "/api/v1/me", None))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "no answer, no erasure"
    );
    assert_eq!(
        body_json(resp).await["error"]["code"],
        "BILLING_PROVIDER_UNREACHABLE"
    );
    assert_eq!(*fake.calls.lock().unwrap(), vec![format!("fetch:{sub}")]);
    let owner = row(&pool, account).await.owner.unwrap();
    let (deleted,): (bool,) =
        sqlx::query_as("SELECT deleted_at IS NOT NULL FROM users WHERE id = $1")
            .bind(owner.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!deleted);

    fake.fetch_fails.store(false, Ordering::Relaxed);
    fake.calls.lock().unwrap().clear();
    let resp = app
        .oneshot(api("DELETE", "/api/v1/me", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        *fake.calls.lock().unwrap(),
        vec![format!("fetch:{sub}"), format!("cancel:{sub}:NextPeriod")]
    );
}

#[tokio::test]
#[ignore]
async fn leaving_ends_what_the_provider_still_holds_unpaid_or_paused() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    seed_prices(&pool).await;
    for status in [SubscriptionStatus::PastDue, SubscriptionStatus::Paused] {
        let fake = Arc::new(FakeProvider::default());
        let (app, org) = app_with_fake(&pool, fake.clone()).await;
        let account = owner_account(&pool, org).await;
        let sub = sub_ref();
        let started = Utc::now() - chrono::Duration::seconds(2);
        for seen in [SubscriptionStatus::Active, SubscriptionStatus::Canceled] {
            let snap = snapshot(&sub, seen, TEAM_MONTH, started);
            let resp = app
                .clone()
                .oneshot(webhook(
                    &event(account, &sub, EventKind::Subscription(snap)),
                    true,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
        fake.subscriptions
            .lock()
            .unwrap()
            .insert(sub.clone(), snapshot(&sub, status, TEAM_MONTH, Utc::now()));

        let resp = app
            .oneshot(api("DELETE", "/api/v1/me", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{status:?}");
        assert_eq!(
            *fake.calls.lock().unwrap(),
            vec![format!("fetch:{sub}"), format!("cancel:{sub}:Now")],
            "{status:?}: the row said ended, the provider said otherwise"
        );
        assert_eq!(
            fake.subscriptions.lock().unwrap()[&sub].status,
            SubscriptionStatus::Canceled
        );
        assert_eq!(row(&pool, account).await.status, BillingStatus::Canceled);
    }
}

#[tokio::test]
#[ignore]
async fn leaving_is_refused_while_the_subscription_cannot_be_ended() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    seed_prices(&pool).await;
    let fake = Arc::new(FakeProvider::default());
    let (app, org) = app_with_fake(&pool, fake.clone()).await;
    let account = owner_account(&pool, org).await;
    let sub = sub_ref();
    let snap = snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, Utc::now());
    fake.subscriptions
        .lock()
        .unwrap()
        .insert(sub.clone(), snap.clone());
    let resp = app
        .clone()
        .oneshot(webhook(
            &event(account, &sub, EventKind::Subscription(snap)),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    fake.cancel_refused.store(true, Ordering::Relaxed);

    let resp = app
        .oneshot(api("DELETE", "/api/v1/me", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        body_json(resp).await["error"]["code"],
        "BILLING_PROVIDER_REFUSED"
    );
    assert_eq!(
        *fake.calls.lock().unwrap(),
        vec![format!("cancel:{sub}:NextPeriod"), format!("fetch:{sub}")],
        "a refusal is checked against the provider's view before it counts"
    );
    let s = row(&pool, account).await;
    assert_eq!((s.status, s.pending_plan_id), (BillingStatus::Active, None));
    let (deleted,): (bool,) =
        sqlx::query_as("SELECT deleted_at IS NOT NULL FROM users WHERE id = $1")
            .bind(s.owner.unwrap().0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!deleted, "a subscription still charging keeps its account");
}

#[tokio::test]
#[ignore]
async fn a_blocked_deletion_leaves_the_subscription_alone() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    seed_prices(&pool).await;
    let fake = Arc::new(FakeProvider::default());
    let (app, org) = app_with_fake(&pool, fake.clone()).await;
    let account = owner_account(&pool, org).await;
    let sub = sub_ref();
    let snap = snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, Utc::now());
    fake.subscriptions
        .lock()
        .unwrap()
        .insert(sub.clone(), snap.clone());
    let resp = app
        .clone()
        .oneshot(webhook(
            &event(account, &sub, EventKind::Subscription(snap)),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let member = make_user(&pool, "member").await;
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'member')")
        .bind(member.0)
        .bind(org.0)
        .execute(&pool)
        .await
        .expect("membership");
    let before = row(&pool, account).await;

    let resp = app
        .oneshot(api("DELETE", "/api/v1/me", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(resp).await["error"]["code"], "OWNS_SHARED_ORGS");
    assert!(
        fake.calls.lock().unwrap().is_empty(),
        "the provider was asked to do something for a deletion that was refused: {:?}",
        fake.calls.lock().unwrap()
    );
    let after = row(&pool, account).await;
    assert_eq!(after, before, "the subscription must be exactly as it was");
    assert!(
        !ledger_kinds(&pool, account)
            .await
            .contains(&"cancel_scheduled".to_string())
    );
}

#[tokio::test]
#[ignore]
async fn the_billing_page_shows_the_plans_and_only_the_payers_controls() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    seed_prices(&pool).await;
    let fake = Arc::new(FakeProvider::default());
    let (app, org) = app_with_fake(&pool, fake.clone()).await;
    let account = owner_account(&pool, org).await;

    let resp = app
        .clone()
        .oneshot(
            Request::get("/settings/billing?plan=pro&interval=year")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("plan:free"), "current plan named");
    assert!(
        html.contains(r#"value="pro" checked"#),
        "?plan preselects the card"
    );
    assert!(
        html.contains(r#"value="year" checked"#),
        "?interval picks the cadence"
    );
    assert!(
        html.contains("$90 / year") && html.contains("save $18 a year"),
        "{html}"
    );
    assert!(
        html.contains("$19 / month"),
        "priced from plan_prices: {html}"
    );
    assert!(html.contains(r#"data-act="checkout""#), "the owner can buy");
    assert!(!html.contains(r#"data-act="change""#) || html.contains(r#"data-act="change" hidden"#));

    let sub = sub_ref();
    let snap = snapshot(&sub, SubscriptionStatus::Active, PRO_MONTH, Utc::now());
    fake.subscriptions
        .lock()
        .unwrap()
        .insert(sub.clone(), snap.clone());
    app.clone()
        .oneshot(webhook(
            &event(account, &sub, EventKind::Subscription(snap)),
            true,
        ))
        .await
        .unwrap();
    let resp = app
        .clone()
        .oneshot(
            Request::get("/settings/billing")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let html = body_text(resp).await;
    assert!(html.contains("plan:pro"), "{html}");
    assert!(html.contains("billing-status--active"));
    assert!(
        html.contains(r#"data-portal="1""#),
        "portal once a customer exists"
    );
    assert!(html.contains(r#"data-interval-current="month""#));
    assert!(html.contains("renews"));

    // A member of the org sees where it stands, not the buttons.
    let member = make_user(&pool, "member").await;
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'member')")
        .bind(member.0)
        .bind(org.0)
        .execute(&pool)
        .await
        .expect("membership");
    let (anon, _) = build_test_app_with_pg_store_anon_tweaked(
        pool.clone(),
        |_| {},
        move |state| state.with_billing_provider(fake),
    )
    .await;
    let resp = with_session(anon.clone(), member, Some(org), None)
        .oneshot(
            Request::get("/settings/billing")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("only the account owner"), "{html}");
    assert!(!html.contains("data-act="));

    // The card form is what the payment-failed mail points at; signing in
    // from that mail has to land there.
    let resp = anon
        .oneshot(
            Request::get("/settings/billing/payment-method")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        resp.headers()[header::LOCATION],
        "/login?redirect_after=%2Fsettings%2Fbilling%2Fpayment-method"
    );
}

#[tokio::test]
#[ignore]
async fn the_billing_page_tells_a_booked_move_from_a_booked_cancel() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    seed_prices(&pool).await;
    let fake = Arc::new(FakeProvider::default());
    let (app, org) = app_with_fake(&pool, fake.clone()).await;
    let account = owner_account(&pool, org).await;
    let sub = sub_ref();
    let snap = snapshot(&sub, SubscriptionStatus::Active, TEAM_MONTH, Utc::now());
    fake.subscriptions
        .lock()
        .unwrap()
        .insert(sub.clone(), snap.clone());
    app.clone()
        .oneshot(webhook(
            &event(account, &sub, EventKind::Subscription(snap)),
            true,
        ))
        .await
        .unwrap();
    let page = || {
        Request::get("/settings/billing")
            .body(Body::empty())
            .unwrap()
    };
    let nav = || {
        Request::get("/web/partials/nav")
            .body(Body::empty())
            .unwrap()
    };

    let resp = app
        .clone()
        .oneshot(api(
            "PUT",
            "/api/v1/account/billing/plan",
            Some(json!({ "plan_id": "pro", "interval": "month" })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(app.clone().oneshot(page()).await.unwrap()).await;
    assert!(
        html.contains(r#"data-pending-plan="pro" data-pending-interval="month""#),
        "{html}"
    );
    assert!(!html.contains("data-cancel-booked"));
    let resp = app
        .clone()
        .oneshot(api(
            "PUT",
            "/api/v1/account/billing/plan",
            Some(json!({ "plan_id": "pro", "interval": "year" })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(app.clone().oneshot(page()).await.unwrap()).await;
    assert!(
        html.contains(r#"data-pending-plan="pro" data-pending-interval="year""#),
        "{html}"
    );
    assert!(html.contains(r#"data-interval-current="month""#), "{html}");
    assert!(html.contains("pro · yearly ·"), "{html}");
    assert!(html.contains("moves-to") && html.contains("renews"));
    let banner = body_text(app.clone().oneshot(nav()).await.unwrap()).await;
    assert!(
        banner.contains(r#"plan moves to <span class="font-mono text-body">Pro</span>"#),
        "{banner}"
    );

    let resp = app
        .clone()
        .oneshot(api("POST", "/api/v1/account/billing/cancel", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(app.clone().oneshot(page()).await.unwrap()).await;
    assert!(html.contains(r#"data-cancel-booked="1""#), "{html}");
    assert!(!html.contains("data-pending-plan"));
    assert!(html.contains("paid-until") && html.contains("then"));
    let banner = body_text(app.oneshot(nav()).await.unwrap()).await;
    assert!(
        banner.contains(r#"plan moves to <span class="font-mono text-body">Free</span>"#),
        "a cancel lands on the fallback, not on the move it outranked: {banner}"
    );
}

#[tokio::test]
#[ignore]
async fn billing_is_absent_without_a_provider() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _) = common::build_test_app_with_pg_store(pool.clone(), |_| {}).await;
    let resp = app
        .clone()
        .oneshot(api("GET", "/api/v1/account/billing", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(resp).await["error"]["code"],
        "BILLING_UNAVAILABLE"
    );

    let resp = app
        .clone()
        .oneshot(
            Request::get("/settings/billing")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = app
        .oneshot(
            Request::post("/hooks/billing/fake")
                .header(SIGNATURE_HEADER, SIGNATURE)
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore]
async fn every_payment_is_logged_with_its_transaction() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;
    let payment = paid();
    let EventKind::Paid(Payment {
        transaction_ref, ..
    }) = &payment
    else {
        unreachable!()
    };
    let transaction_ref = transaction_ref.clone();
    h.billing
        .apply_event(&h.pool, &h.quotas, event(account, &sub, payment))
        .await
        .expect("paid");

    let (payload,): (serde_json::Value,) = sqlx::query_as(
        "SELECT payload FROM account_billing_events WHERE account_id = $1 AND kind = 'payment_received'",
    )
    .bind(account.0)
    .fetch_one(&h.pool)
    .await
    .expect("payment logged");
    assert_eq!(payload["transaction"], transaction_ref.as_str());
    assert_eq!(payload["subscription"], sub.as_str());
    assert_eq!(payload["total"]["amount_minor"], 1900);
    assert_eq!(payload["total"]["currency"], "USD");
}

#[tokio::test]
#[ignore]
async fn refunds_are_logged_and_never_touch_the_plan() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;
    h.billing
        .apply_event(&h.pool, &h.quotas, event(account, &sub, paid_as("txn_1")))
        .await
        .expect("paid");
    h.provider.calls.lock().unwrap().clear();

    for kind in [refund("txn_1", false, true), refund("txn_1", true, true)] {
        h.billing
            .apply_event(&h.pool, &h.quotas, event(account, &sub, kind))
            .await
            .expect("refund");
    }
    assert!(
        h.provider.calls.lock().unwrap().is_empty(),
        "ending the subscription stays a support decision"
    );
    assert_eq!(row(&h.pool, account).await.status, BillingStatus::Active);
    let kinds = ledger_kinds(&h.pool, account).await;
    assert_eq!(
        kinds.iter().filter(|k| *k == "refund_recorded").count(),
        2,
        "{kinds:?}"
    );
}

#[tokio::test]
#[ignore]
async fn a_refund_labelled_partial_for_the_whole_payment_counts_as_full() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let sub = sub_ref();
    activate(&h, account, &sub, TEAM_MONTH).await;
    let txn = format!("txn_{}", Uuid::now_v7().simple());
    h.billing
        .apply_event(&h.pool, &h.quotas, event(account, &sub, paid_as(&txn)))
        .await
        .expect("paid");

    let mut part = refund(&txn, true, false);
    if let EventKind::Refunded(r) = &mut part {
        r.total = Some(Money {
            amount_minor: 500,
            currency: "USD".into(),
        });
    }
    for kind in [refund(&txn, true, false), part] {
        h.billing
            .apply_event(&h.pool, &h.quotas, event(account, &sub, kind))
            .await
            .expect("refund");
    }
    let full: Vec<bool> = sqlx::query_scalar(
        "SELECT (payload->>'full')::bool FROM account_billing_events
          WHERE account_id = $1 AND kind = 'refund_recorded' ORDER BY id",
    )
    .bind(account.0)
    .fetch_all(&h.pool)
    .await
    .expect("refunds");
    assert_eq!(full, vec![true, false]);
}

#[tokio::test]
#[ignore]
async fn a_refund_on_a_replaced_subscription_finds_its_account_through_the_payment() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let old = sub_ref();
    activate(&h, account, &old, TEAM_MONTH).await;
    let txn = format!("txn_{}", Uuid::now_v7().simple());
    h.billing
        .apply_event(&h.pool, &h.quotas, event(account, &old, paid_as(&txn)))
        .await
        .expect("paid");
    let ended = snapshot(&old, SubscriptionStatus::Canceled, TEAM_MONTH, Utc::now());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &old, EventKind::Subscription(ended)),
        )
        .await
        .expect("ended");
    let new = sub_ref();
    activate(&h, account, &new, TEAM_MONTH).await;
    assert_eq!(
        row(&h.pool, account).await.subscription_ref.as_deref(),
        Some(new.as_str())
    );

    let mut late = event(account, &old, refund(&txn, true, true));
    late.account = None;
    h.billing
        .apply_event(&h.pool, &h.quotas, late)
        .await
        .expect("refund");
    assert!(
        ledger_kinds(&h.pool, account)
            .await
            .contains(&"refund_recorded".to_string())
    );
}

#[tokio::test]
#[ignore]
async fn a_late_payment_on_a_replaced_subscription_is_logged_without_touching_the_plan() {
    let Some(h) = harness().await else { return };
    let (account, _, _) = account(&h.pool, "founding", 0).await;
    let old = sub_ref();
    activate(&h, account, &old, TEAM_MONTH).await;
    let ended = snapshot(&old, SubscriptionStatus::Canceled, TEAM_MONTH, Utc::now());
    h.billing
        .apply_event(
            &h.pool,
            &h.quotas,
            event(account, &old, EventKind::Subscription(ended)),
        )
        .await
        .expect("ended");
    let new = sub_ref();
    activate(&h, account, &new, TEAM_MONTH).await;
    let before = row(&h.pool, account).await;

    let txn = format!("txn_{}", Uuid::now_v7().simple());
    h.billing
        .apply_event(&h.pool, &h.quotas, event(account, &old, paid_as(&txn)))
        .await
        .expect("late payment");
    let after = row(&h.pool, account).await;
    assert_eq!(after.subscription_ref, before.subscription_ref);
    assert_eq!(after.plan_id, before.plan_id);
    assert_eq!(after.status, before.status);

    let mut late_refund = event(account, &old, refund(&txn, true, true));
    late_refund.account = None;
    h.billing
        .apply_event(&h.pool, &h.quotas, late_refund)
        .await
        .expect("refund");
    let kinds = ledger_kinds(&h.pool, account).await;
    assert!(kinds.contains(&"payment_received".to_string()), "{kinds:?}");
    assert!(kinds.contains(&"refund_recorded".to_string()), "{kinds:?}");
}
