//! The shared send gate against a real Postgres: two instances booking one
//! row, the provider's 429 reaching both, and what an instance does when the
//! row cannot be reached in time.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tokio::time::Instant;
use uptimepage::email::gate::BULK_DEADLINE;
use uptimepage::email::{
    BatchResult, EmailAddress, EmailError, EmailResult, EmailSender, EmailTemplate,
    GatedEmailSender, InMemoryEmailSender, MessageId, PreparedBatch, SendClass, SendGate,
    TransactionalEmail,
};

use crate::common;

async fn db(tag: &str) -> Option<(String, String)> {
    common::fresh_test_db(tag).await
}

fn soon() -> Instant {
    Instant::now() + BULK_DEADLINE
}

fn gate(url: &str, limit: u32) -> SendGate {
    SendGate::connect_lazy(url, limit).expect("gate pool")
}

fn sample() -> TransactionalEmail {
    TransactionalEmail {
        to: EmailAddress::new("alice@example.com", "Alice"),
        from: EmailAddress::new("hello@example.test", "Uptimepage"),
        template: EmailTemplate::AccountDeletion {
            scheduled_purge_at: Utc::now(),
        },
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn two_instances_never_book_closer_than_the_shared_pace() {
    let Some((url, name)) = db("gate_pace").await else {
        return;
    };
    // 40 per second: the shared row hands out 32, one every 31.25 ms.
    let (blue, green) = (Arc::new(gate(&url, 40)), Arc::new(gate(&url, 40)));
    for g in [&blue, &green] {
        g.bulk(soon()).await.expect("warm the gate connection");
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = Instant::now();
    let mut sends = Vec::new();
    for i in 0..16 {
        let g = if i % 2 == 0 {
            blue.clone()
        } else {
            green.clone()
        };
        sends.push(tokio::spawn(async move {
            g.immediate(Instant::now() + Duration::from_secs(10))
                .await
                .expect("a slot inside the deadline");
            Instant::now()
        }));
    }
    let mut last = started;
    for s in sends {
        last = last.max(s.await.unwrap());
    }
    // The first slot is free at once; the sixteenth is fifteen intervals on.
    let span = last - started;
    assert!(
        span >= Duration::from_micros(31_250 * 15 - 5_000),
        "16 slots from two instances spread over {span:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn bulk_waits_behind_immediate_slots_booked_ahead() {
    let Some((url, name)) = db("gate_priority").await else {
        return;
    };
    let g = Arc::new(gate(&url, 10));
    // Four alerts arrive together and book the next 500 ms.
    for _ in 0..4 {
        let g = g.clone();
        tokio::spawn(async move {
            g.immediate(Instant::now() + Duration::from_secs(10))
                .await
                .unwrap()
        });
    }
    tokio::time::sleep(Duration::from_millis(60)).await;
    let asked = Instant::now();
    g.bulk(soon()).await.expect("bulk slot");
    assert!(
        asked.elapsed() >= Duration::from_millis(400),
        "bulk took a slot after {:?}, inside the booked ones",
        asked.elapsed()
    );
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_429_on_one_instance_holds_the_other() {
    let Some((url, name)) = db("gate_block").await else {
        return;
    };
    let (blue, green) = (gate(&url, 10), gate(&url, 10));
    blue.block(Duration::from_secs(1)).await;

    let asked = Instant::now();
    green
        .immediate(Instant::now() + Duration::from_secs(10))
        .await
        .unwrap();
    assert!(asked.elapsed() >= Duration::from_millis(950));

    blue.block(Duration::from_secs(1)).await;
    let asked = Instant::now();
    green.bulk(soon()).await.unwrap();
    assert!(asked.elapsed() >= Duration::from_millis(950));
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_429_during_a_booked_sleep_books_again_behind_the_block() {
    let Some((url, name)) = db("gate_rebook").await else {
        return;
    };
    let (blue, green) = (Arc::new(gate(&url, 10)), gate(&url, 10));
    let deadline = || Instant::now() + Duration::from_secs(10);
    // Three slots ahead, so the next one sleeps about 375 ms.
    for _ in 0..3 {
        let b = blue.clone();
        tokio::spawn(async move { b.immediate(deadline()).await.unwrap() });
    }
    tokio::time::sleep(Duration::from_millis(30)).await;
    let asked = Instant::now();
    let sleeper = {
        let b = blue.clone();
        tokio::spawn(async move { b.immediate(deadline()).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    green.block(Duration::from_secs(2)).await;
    sleeper.await.unwrap().unwrap();
    assert!(
        asked.elapsed() >= Duration::from_millis(1_900),
        "the slot booked before the 429 went out after {:?}",
        asked.elapsed()
    );
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn bulk_behind_a_block_longer_than_its_wait_is_told_when_to_come_back() {
    let Some((url, name)) = db("gate_bulk_deadline").await else {
        return;
    };
    let g = gate(&url, 10);
    g.block(Duration::from_secs(1200)).await;
    let asked = Instant::now();
    let err = g
        .bulk(soon())
        .await
        .expect_err("the block outlasts the wait");
    assert!(asked.elapsed() < Duration::from_millis(500));
    assert!(
        matches!(err, EmailError::Deferred { retry_after } if retry_after >= Duration::from_secs(1190)),
        "{err:?}"
    );
    assert!(err.is_pacing());
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn no_slot_before_the_deadline_is_said_at_once() {
    let Some((url, name)) = db("gate_deadline").await else {
        return;
    };
    let g = gate(&url, 10);
    g.block(Duration::from_secs(20)).await;
    let asked = Instant::now();
    let free_in = g
        .immediate(Instant::now() + Duration::from_secs(1))
        .await
        .expect_err("the block outlasts the deadline");
    assert!(asked.elapsed() < Duration::from_millis(500));
    assert!(free_in >= Duration::from_secs(19), "{free_in:?}");
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_locked_gate_row_moves_the_instance_to_its_own_pacer() {
    let Some((url, name)) = db("gate_locked").await else {
        return;
    };
    let pool = common::open_test_pool(&url).await;
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM email_send_gate FOR UPDATE")
        .execute(&mut *holder)
        .await
        .unwrap();

    let g = gate(&url, 10);
    let asked = Instant::now();
    g.immediate(Instant::now() + Duration::from_secs(10))
        .await
        .expect("the local pacer still sends");
    assert!(
        asked.elapsed() < Duration::from_secs(1),
        "{:?}",
        asked.elapsed()
    );
    // Held on the pacer: no second wait on the lock, and bulk stands down.
    let asked = Instant::now();
    assert!(matches!(
        g.bulk(soon()).await,
        Err(EmailError::GateUnavailable)
    ));
    assert!(asked.elapsed() < Duration::from_millis(50));

    holder.rollback().await.unwrap();
    pool.close().await;
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn running_into_the_callers_deadline_is_not_a_database_failure() {
    let Some((url, name)) = db("gate_own_deadline").await else {
        return;
    };
    let pool = common::open_test_pool(&url).await;
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM email_send_gate FOR UPDATE")
        .execute(&mut *holder)
        .await
        .unwrap();
    let g = gate(&url, 10);
    // Shorter than the lock timeout: the caller's deadline ends the wait.
    assert!(
        g.immediate(Instant::now() + Duration::from_millis(100))
            .await
            .is_err()
    );
    holder.rollback().await.unwrap();
    g.bulk(soon()).await.expect("still on the shared row");
    pool.close().await;
    common::drop_test_db(&name).await;
}

#[tokio::test]
async fn an_unreachable_database_still_paces_alerts_and_holds_bulk() {
    // Nothing listens on port 1.
    let g = gate("postgres://monitor:monitor@127.0.0.1:1/none", 10);
    let asked = Instant::now();
    g.immediate(Instant::now() + Duration::from_secs(10))
        .await
        .expect("first slot on the pacer");
    g.immediate(Instant::now() + Duration::from_secs(10))
        .await
        .expect("second slot on the pacer");
    // The pacer runs at a fifth of the limit: 2 per second.
    assert!(asked.elapsed() >= Duration::from_millis(450));
    assert!(asked.elapsed() < Duration::from_secs(2));
    assert!(matches!(
        g.bulk(soon()).await,
        Err(EmailError::GateUnavailable)
    ));
}

#[tokio::test]
async fn a_database_that_never_answers_reaches_the_pacer_within_a_second() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    // Accepts and then says nothing, like a host behind a dead link.
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let g = gate(
        &format!("postgres://monitor:monitor@127.0.0.1:{port}/none"),
        10,
    );
    let asked = Instant::now();
    g.immediate(Instant::now() + Duration::from_secs(10))
        .await
        .expect("the pacer sends");
    assert!(
        asked.elapsed() < Duration::from_millis(1_100),
        "{:?}",
        asked.elapsed()
    );
}

/// Answers 429 a set number of times, then takes the mail.
struct Throttled {
    refusals: AtomicUsize,
    retry_after: Duration,
    calls: AtomicUsize,
    inner: InMemoryEmailSender,
}

#[async_trait]
impl EmailSender for Throttled {
    async fn send(&self, email: TransactionalEmail, class: SendClass) -> EmailResult<MessageId> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.refusals.load(Ordering::SeqCst) > 0 {
            self.refusals.fetch_sub(1, Ordering::SeqCst);
            return Err(EmailError::RateLimited {
                retry_after: self.retry_after,
                detail: "429 Too Many Requests".into(),
            });
        }
        self.inner.send(email, class).await
    }

    fn prepare_batch(
        &self,
        emails: Vec<TransactionalEmail>,
        key: String,
    ) -> EmailResult<PreparedBatch> {
        self.inner.prepare_batch(emails, key)
    }

    async fn send_prepared(&self, batch: PreparedBatch) -> BatchResult {
        self.inner.send_prepared(batch).await
    }
}

fn throttled(refusals: usize, retry_after: Duration) -> Arc<Throttled> {
    Arc::new(Throttled {
        refusals: AtomicUsize::new(refusals),
        retry_after,
        calls: AtomicUsize::new(0),
        inner: InMemoryEmailSender::new(),
    })
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_immediate_mail_retries_a_429_once_when_it_fits_the_deadline() {
    let Some((url, name)) = db("gate_retry").await else {
        return;
    };
    let provider = throttled(1, Duration::from_secs(1));
    let sender = GatedEmailSender::new(provider.clone(), gate(&url, 10));
    let asked = Instant::now();
    sender
        .send(sample(), SendClass::Immediate)
        .await
        .expect("sent on the second try");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert!(asked.elapsed() >= Duration::from_millis(950));
    assert_eq!(provider.inner.len(), 1);
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_429_retry_past_the_deadline_is_not_sent() {
    let Some((url, name)) = db("gate_retry_late").await else {
        return;
    };
    let provider = throttled(1, Duration::from_secs(20));
    let sender = GatedEmailSender::new(provider.clone(), gate(&url, 10));
    let asked = Instant::now();
    let err = sender
        .send(sample(), SendClass::Immediate)
        .await
        .expect_err("the provider's wait outlasts the deadline");
    // The provider's own answer, not the budget that ran out behind it.
    assert!(
        matches!(&err, EmailError::RateLimited { detail, .. } if detail.contains("429")),
        "{err:?}"
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert!(asked.elapsed() < Duration::from_secs(1));
    // Bulk sees the same block and waits it out rather than sending.
    let other = gate(&url, 10);
    assert!(
        tokio::time::timeout(Duration::from_millis(500), other.bulk(soon()))
            .await
            .is_err()
    );
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_healthy_and_a_cut_off_instance_together_stay_at_the_limit() {
    let Some((url, name)) = db("gate_mixed").await else {
        return;
    };
    let healthy = Arc::new(gate(&url, 10));
    let cut_off = Arc::new(gate("postgres://monitor:monitor@127.0.0.1:1/none", 10));
    let window = Duration::from_secs(2);
    let until = Instant::now() + window;
    let sent = Arc::new(AtomicUsize::new(0));
    let mut senders = Vec::new();
    for g in [healthy, cut_off] {
        for _ in 0..4 {
            let (g, sent) = (g.clone(), sent.clone());
            senders.push(tokio::spawn(async move {
                while g.immediate(until).await.is_ok() && Instant::now() < until {
                    sent.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
    }
    for s in senders {
        s.await.unwrap();
    }
    // 8 a second on the row plus 2 on the pacer, one first slot each.
    let sent = sent.load(Ordering::SeqCst);
    assert!(sent <= 22, "{sent} sends in {window:?}");
    assert!(sent >= 15, "{sent} sends in {window:?}");
    common::drop_test_db(&name).await;
}
