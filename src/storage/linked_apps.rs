//! App accounts people proved are theirs, so an acknowledgement pressed in the
//! app can name them, and the single-use codes they prove it with. Held per
//! user rather than per org: [`LinkedAppStore::resolve`] is the only way a
//! press turns into a person, and it names only a member of the org the press
//! landed in.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::{ExternalId, LinkedApp, LinkedAppAccount, OrgId, UserId};
use crate::error::{AppError, Result};
use crate::security::app_link::{PUSHOVER_LINK_TTL, PUSHOVER_OFFER_COOLDOWN};
use crate::storage::locks::{advisory_xact_lock, app_account_lock_key, app_link_user_lock_key};

/// Who an app account is to one org.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Linked {
    /// A live member of the org linked it.
    Member(UserId),
    /// Linked by someone the org does not count as a member.
    Outsider,
    /// Nobody linked it.
    Unlinked,
    /// The lookup failed, so nothing is known about it.
    Unknown,
}

impl Linked {
    pub fn member(self) -> Option<UserId> {
        match self {
            Self::Member(user) => Some(user),
            Self::Outsider | Self::Unlinked | Self::Unknown => None,
        }
    }

    /// Whoever holds the account is worth telling how to link it only when
    /// it is known that nobody has.
    pub fn invites_link(self) -> bool {
        self == Self::Unlinked
    }
}

/// Who pressed, as far as `org` is concerned. A failed lookup names nobody:
/// taking the page matters more than naming who took it.
pub async fn identify(
    store: &dyn LinkedAppStore,
    org: OrgId,
    app: LinkedApp,
    sender: ExternalId,
) -> Linked {
    match store.resolve(org, app, sender).await {
        Ok(linked) => linked,
        Err(err) => {
            tracing::warn!(org_id = %org.0, app = app.as_db_str(), error = %err, "linked app lookup failed");
            Linked::Unknown
        }
    }
}

/// The side of a link that whoever spends a code proves.
#[derive(Debug, Clone, Copy)]
pub enum Claimant<'a> {
    /// The app account that pressed Start on a code minted for a person.
    Account {
        id: ExternalId,
        label: Option<&'a str>,
    },
    /// The signed-in person opening a code pushed to an app account.
    Person(UserId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkOutcome {
    Linked(UserId),
    /// It was this person's already.
    AlreadyYours(UserId),
    /// Someone else holds it and has to unlink it first. The code stays live.
    Taken,
    /// Unknown, spent, expired, or minted for the other kind of proof.
    Invalid,
}

/// Cap on what an app calls someone, mirrored by the column CHECK.
const MAX_LABEL_CHARS: usize = 128;

#[async_trait]
pub trait LinkedAppStore: Send + Sync {
    /// The user's linked accounts, oldest first.
    async fn for_user(&self, user: UserId) -> Result<Vec<LinkedAppAccount>>;
    /// `false` when no such account is linked to `user`.
    async fn unlink(&self, user: UserId, id: Uuid) -> Result<bool>;
    /// Free an app account from whoever holds it, at the account's own
    /// request. `false` when nobody did.
    async fn release(&self, app: LinkedApp, account: ExternalId) -> Result<bool>;
    /// Who this app account is to `org`. Never [`Linked::Unknown`]: a failed
    /// lookup is an error here.
    async fn resolve(&self, org: OrgId, app: LinkedApp, account: ExternalId) -> Result<Linked>;
    /// Store a Telegram code that links whoever presses Start with it to
    /// `user`, voiding the ones they asked for before.
    async fn mint_telegram(
        &self,
        user: UserId,
        code_hash: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<()>;
    /// Store a code offering `account` to whoever signs in to open it. `false`
    /// when the account was offered one within the cooldown, so none is.
    async fn offer(
        &self,
        app: LinkedApp,
        account: ExternalId,
        label: Option<&str>,
        code_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<bool>;
    /// Take back an offer that never reached its account, so the cooldown does
    /// not hold back the next one.
    async fn withdraw_offer(&self, app: LinkedApp, code_hash: &str) -> Result<()>;
    /// What a live offer's account is called, for the page that opens it.
    /// `None` when the code is unknown, spent or expired.
    async fn offered(&self, app: LinkedApp, code_hash: &str) -> Result<Option<Option<String>>>;
    /// Spend a live code and write the link it completes, in one transaction.
    async fn claim(
        &self,
        app: LinkedApp,
        code_hash: &str,
        claimant: Claimant<'_>,
    ) -> Result<LinkOutcome>;
}

fn clean_label(label: Option<&str>) -> Option<String> {
    label
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.chars().take(MAX_LABEL_CHARS).collect())
}

fn db_err(what: &'static str) -> impl FnOnce(sqlx::Error) -> AppError {
    move |e| AppError::Other(anyhow::anyhow!("{what}: {e}"))
}

/// Delete the codes nothing reads any more: each one once it expires, except a
/// Pushover offer, which the offer cooldown keeps counting until it passes.
pub async fn purge_dead_codes(pool: &PgPool) -> Result<u64> {
    let done = sqlx::query(
        "DELETE FROM app_link_challenges \
         WHERE expires_at < now() AND (external_hash IS NULL OR created_at < $1)",
    )
    .bind(Utc::now() - PUSHOVER_OFFER_COOLDOWN)
    .execute(pool)
    .await
    .map_err(db_err("purge app link codes"))?;
    Ok(done.rows_affected())
}

pub struct PgLinkedAppStore {
    pool: PgPool,
}

impl PgLinkedAppStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl LinkedAppStore for PgLinkedAppStore {
    async fn for_user(&self, user: UserId) -> Result<Vec<LinkedAppAccount>> {
        let rows: Vec<(Uuid, String, Option<String>, DateTime<Utc>)> = sqlx::query_as(
            "SELECT id, app, label, linked_at FROM linked_app_accounts \
             WHERE user_id = $1 ORDER BY linked_at, id",
        )
        .bind(user.0)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err("linked app accounts"))?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, app, label, linked_at)| {
                Some(LinkedAppAccount {
                    id,
                    user_id: user,
                    app: LinkedApp::from_db_str(&app)?,
                    label,
                    linked_at,
                })
            })
            .collect())
    }

    async fn unlink(&self, user: UserId, id: Uuid) -> Result<bool> {
        let done = sqlx::query("DELETE FROM linked_app_accounts WHERE id = $1 AND user_id = $2")
            .bind(id)
            .bind(user.0)
            .execute(&self.pool)
            .await
            .map_err(db_err("unlink app account"))?;
        Ok(done.rows_affected() > 0)
    }

    async fn release(&self, app: LinkedApp, account: ExternalId) -> Result<bool> {
        let done =
            sqlx::query("DELETE FROM linked_app_accounts WHERE app = $1 AND external_hash = $2")
                .bind(app.as_db_str())
                .bind(account.hex())
                .execute(&self.pool)
                .await
                .map_err(db_err("release app account"))?;
        Ok(done.rows_affected() > 0)
    }

    async fn resolve(&self, org: OrgId, app: LinkedApp, account: ExternalId) -> Result<Linked> {
        let row: Option<(Uuid, bool)> = sqlx::query_as(
            "SELECT a.user_id, EXISTS ( \
                 SELECT 1 FROM memberships m JOIN users u ON u.id = m.user_id \
                 WHERE m.user_id = a.user_id AND m.org_id = $1 AND u.deleted_at IS NULL) \
             FROM linked_app_accounts a WHERE a.app = $2 AND a.external_hash = $3",
        )
        .bind(org.0)
        .bind(app.as_db_str())
        .bind(account.hex())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err("resolve linked app"))?;
        Ok(match row {
            Some((user, true)) => Linked::Member(UserId(user)),
            Some((_, false)) => Linked::Outsider,
            None => Linked::Unlinked,
        })
    }

    async fn mint_telegram(
        &self,
        user: UserId,
        code_hash: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(db_err("begin"))?;
        advisory_xact_lock(&mut *tx, &app_link_user_lock_key(user))
            .await
            .map_err(db_err("app link user lock"))?;
        sqlx::query(
            "DELETE FROM app_link_challenges \
             WHERE app = 'telegram' AND user_id = $1 AND consumed_at IS NULL",
        )
        .bind(user.0)
        .execute(&mut *tx)
        .await
        .map_err(db_err("void telegram link codes"))?;
        sqlx::query(
            "INSERT INTO app_link_challenges (code_hash, app, user_id, expires_at) \
             VALUES ($1, 'telegram', $2, $3)",
        )
        .bind(code_hash)
        .bind(user.0)
        .bind(expires_at)
        .execute(&mut *tx)
        .await
        .map_err(db_err("mint telegram link code"))?;
        tx.commit().await.map_err(db_err("commit"))?;
        Ok(())
    }

    async fn offer(
        &self,
        app: LinkedApp,
        account: ExternalId,
        label: Option<&str>,
        code_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let account = account.hex();
        let mut tx = self.pool.begin().await.map_err(db_err("begin"))?;
        advisory_xact_lock(&mut *tx, &app_account_lock_key(app.as_db_str(), &account))
            .await
            .map_err(db_err("app account lock"))?;
        let recent: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM app_link_challenges \
                            WHERE app = $1 AND external_hash = $2 AND created_at > $3)",
        )
        .bind(app.as_db_str())
        .bind(&account)
        .bind(now - PUSHOVER_OFFER_COOLDOWN)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err("recent link offer"))?;
        if recent {
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO app_link_challenges \
                 (code_hash, app, external_hash, label, created_at, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(code_hash)
        .bind(app.as_db_str())
        .bind(&account)
        .bind(clean_label(label))
        .bind(now)
        .bind(now + PUSHOVER_LINK_TTL)
        .execute(&mut *tx)
        .await
        .map_err(db_err("mint link offer"))?;
        tx.commit().await.map_err(db_err("commit"))?;
        Ok(true)
    }

    async fn withdraw_offer(&self, app: LinkedApp, code_hash: &str) -> Result<()> {
        sqlx::query(
            "DELETE FROM app_link_challenges \
             WHERE code_hash = $1 AND app = $2 AND external_hash IS NOT NULL \
               AND consumed_at IS NULL",
        )
        .bind(code_hash)
        .bind(app.as_db_str())
        .execute(&self.pool)
        .await
        .map_err(db_err("withdraw link offer"))?;
        Ok(())
    }

    async fn offered(&self, app: LinkedApp, code_hash: &str) -> Result<Option<Option<String>>> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT label FROM app_link_challenges \
             WHERE code_hash = $1 AND app = $2 AND external_hash IS NOT NULL \
               AND consumed_at IS NULL AND expires_at > now()",
        )
        .bind(code_hash)
        .bind(app.as_db_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err("link offer"))?;
        Ok(row.map(|(label,)| label))
    }

    async fn claim(
        &self,
        app: LinkedApp,
        code_hash: &str,
        claimant: Claimant<'_>,
    ) -> Result<LinkOutcome> {
        let mut tx = self.pool.begin().await.map_err(db_err("begin"))?;
        let code: Option<(Option<Uuid>, Option<String>, Option<String>)> = sqlx::query_as(
            "UPDATE app_link_challenges SET consumed_at = now() \
             WHERE code_hash = $1 AND app = $2 AND consumed_at IS NULL AND expires_at > now() \
             RETURNING user_id, external_hash, label",
        )
        .bind(code_hash)
        .bind(app.as_db_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err("spend app link code"))?;
        // Returning drops the transaction, so a refusal leaves the code live.
        let (user, account, label) = match (claimant, code) {
            (Claimant::Account { id, label }, Some((Some(user), None, _))) => {
                (UserId(user), id.hex(), clean_label(label))
            }
            (Claimant::Person(user), Some((None, Some(account), label))) => (user, account, label),
            _ => return Ok(LinkOutcome::Invalid),
        };
        let inserted: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO linked_app_accounts (user_id, app, external_hash, label) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (app, external_hash) DO NOTHING RETURNING id",
        )
        .bind(user.0)
        .bind(app.as_db_str())
        .bind(&account)
        .bind(label)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err("link app account"))?;
        let outcome = if inserted.is_some() {
            LinkOutcome::Linked(user)
        } else {
            let holder: Option<Uuid> = sqlx::query_scalar(
                "SELECT user_id FROM linked_app_accounts WHERE app = $1 AND external_hash = $2",
            )
            .bind(app.as_db_str())
            .bind(&account)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err("app account holder"))?;
            if holder != Some(user.0) {
                return Ok(LinkOutcome::Taken);
            }
            LinkOutcome::AlreadyYours(user)
        };
        tx.commit().await.map_err(db_err("commit"))?;
        Ok(outcome)
    }
}

struct MemCode {
    code_hash: String,
    app: LinkedApp,
    user: Option<UserId>,
    account: Option<ExternalId>,
    label: Option<String>,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    spent: bool,
}

#[derive(Default)]
struct MemState {
    accounts: Vec<(LinkedAppAccount, ExternalId)>,
    codes: Vec<MemCode>,
}

/// Single-tenant test double: every linked user counts as a member.
#[derive(Default)]
pub struct InMemoryLinkedAppStore {
    inner: Mutex<MemState>,
}

impl InMemoryLinkedAppStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl LinkedAppStore for InMemoryLinkedAppStore {
    async fn for_user(&self, user: UserId) -> Result<Vec<LinkedAppAccount>> {
        Ok(self
            .inner
            .lock()
            .accounts
            .iter()
            .filter(|(a, _)| a.user_id == user)
            .map(|(a, _)| a.clone())
            .collect())
    }

    async fn unlink(&self, user: UserId, id: Uuid) -> Result<bool> {
        let mut g = self.inner.lock();
        let before = g.accounts.len();
        g.accounts
            .retain(|(a, _)| !(a.id == id && a.user_id == user));
        Ok(g.accounts.len() < before)
    }

    async fn release(&self, app: LinkedApp, account: ExternalId) -> Result<bool> {
        let mut g = self.inner.lock();
        let before = g.accounts.len();
        g.accounts
            .retain(|(a, id)| !(a.app == app && *id == account));
        Ok(g.accounts.len() < before)
    }

    async fn resolve(&self, _org: OrgId, app: LinkedApp, account: ExternalId) -> Result<Linked> {
        Ok(self
            .inner
            .lock()
            .accounts
            .iter()
            .find(|(a, id)| a.app == app && *id == account)
            .map_or(Linked::Unlinked, |(a, _)| Linked::Member(a.user_id)))
    }

    async fn mint_telegram(
        &self,
        user: UserId,
        code_hash: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut g = self.inner.lock();
        g.codes
            .retain(|c| !(c.app == LinkedApp::Telegram && c.user == Some(user) && !c.spent));
        g.codes.push(MemCode {
            code_hash: code_hash.to_string(),
            app: LinkedApp::Telegram,
            user: Some(user),
            account: None,
            label: None,
            created_at: Utc::now(),
            expires_at,
            spent: false,
        });
        Ok(())
    }

    async fn offer(
        &self,
        app: LinkedApp,
        account: ExternalId,
        label: Option<&str>,
        code_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let mut g = self.inner.lock();
        let cutoff = now - PUSHOVER_OFFER_COOLDOWN;
        if g.codes
            .iter()
            .any(|c| c.app == app && c.account == Some(account) && c.created_at > cutoff)
        {
            return Ok(false);
        }
        g.codes.push(MemCode {
            code_hash: code_hash.to_string(),
            app,
            user: None,
            account: Some(account),
            label: clean_label(label),
            created_at: now,
            expires_at: now + PUSHOVER_LINK_TTL,
            spent: false,
        });
        Ok(true)
    }

    async fn withdraw_offer(&self, app: LinkedApp, code_hash: &str) -> Result<()> {
        self.inner.lock().codes.retain(|c| {
            !(c.code_hash == code_hash && c.app == app && c.account.is_some() && !c.spent)
        });
        Ok(())
    }

    async fn offered(&self, app: LinkedApp, code_hash: &str) -> Result<Option<Option<String>>> {
        let now = Utc::now();
        Ok(self
            .inner
            .lock()
            .codes
            .iter()
            .find(|c| {
                c.code_hash == code_hash
                    && c.app == app
                    && c.account.is_some()
                    && !c.spent
                    && c.expires_at > now
            })
            .map(|c| c.label.clone()))
    }

    async fn claim(
        &self,
        app: LinkedApp,
        code_hash: &str,
        claimant: Claimant<'_>,
    ) -> Result<LinkOutcome> {
        let now = Utc::now();
        let mut g = self.inner.lock();
        let Some(idx) = g.codes.iter().position(|c| {
            c.code_hash == code_hash && c.app == app && !c.spent && c.expires_at > now
        }) else {
            return Ok(LinkOutcome::Invalid);
        };
        let code = &g.codes[idx];
        let (user, account, label) = match (claimant, code.user, code.account) {
            (Claimant::Account { id, label }, Some(user), None) => (user, id, clean_label(label)),
            (Claimant::Person(user), None, Some(account)) => (user, account, code.label.clone()),
            _ => return Ok(LinkOutcome::Invalid),
        };
        let outcome = match g
            .accounts
            .iter()
            .find(|(a, id)| a.app == app && *id == account)
        {
            Some((held, _)) if held.user_id == user => LinkOutcome::AlreadyYours(user),
            Some(_) => return Ok(LinkOutcome::Taken),
            None => {
                g.accounts.push((
                    LinkedAppAccount {
                        id: Uuid::now_v7(),
                        user_id: user,
                        app,
                        label,
                        linked_at: now,
                    },
                    account,
                ));
                LinkOutcome::Linked(user)
            }
        };
        g.codes[idx].spent = true;
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::app_link::external_id;

    fn olena_telegram() -> ExternalId {
        external_id("s3cret", "4242")
    }

    fn press(id: ExternalId) -> Claimant<'static> {
        Claimant::Account {
            id,
            label: Some("@olena"),
        }
    }

    #[tokio::test]
    async fn a_telegram_code_links_once_and_never_takes_an_account_from_its_holder() {
        let store = InMemoryLinkedAppStore::new();
        let (olena, taras) = (UserId(Uuid::now_v7()), UserId(Uuid::now_v7()));
        let later = Utc::now() + chrono::Duration::hours(1);
        store.mint_telegram(olena, "c1", later).await.unwrap();
        assert_eq!(
            store
                .claim(LinkedApp::Telegram, "c1", press(olena_telegram()))
                .await
                .unwrap(),
            LinkOutcome::Linked(olena)
        );
        assert_eq!(
            store
                .claim(
                    LinkedApp::Telegram,
                    "c1",
                    press(external_id("s3cret", "99"))
                )
                .await
                .unwrap(),
            LinkOutcome::Invalid,
            "a code links one Telegram account"
        );

        store.mint_telegram(taras, "c2", later).await.unwrap();
        assert_eq!(
            store
                .claim(LinkedApp::Telegram, "c2", press(olena_telegram()))
                .await
                .unwrap(),
            LinkOutcome::Taken,
            "olena's Telegram stays hers"
        );
        let org = OrgId(Uuid::now_v7());
        assert_eq!(
            store
                .resolve(org, LinkedApp::Telegram, olena_telegram())
                .await
                .unwrap(),
            Linked::Member(olena)
        );
        assert_eq!(
            store
                .resolve(org, LinkedApp::Pushover, olena_telegram())
                .await
                .unwrap(),
            Linked::Unlinked,
            "the same id in another app is someone else"
        );
    }

    #[tokio::test]
    async fn asking_again_voids_the_earlier_telegram_code() {
        let store = InMemoryLinkedAppStore::new();
        let olena = UserId(Uuid::now_v7());
        let later = Utc::now() + chrono::Duration::hours(1);
        store.mint_telegram(olena, "old", later).await.unwrap();
        store.mint_telegram(olena, "new", later).await.unwrap();
        assert_eq!(
            store
                .claim(LinkedApp::Telegram, "old", press(olena_telegram()))
                .await
                .unwrap(),
            LinkOutcome::Invalid
        );
        assert_eq!(
            store
                .claim(LinkedApp::Telegram, "new", press(olena_telegram()))
                .await
                .unwrap(),
            LinkOutcome::Linked(olena)
        );
    }

    #[tokio::test]
    async fn a_pushover_account_is_offered_a_link_once_per_cooldown() {
        let store = InMemoryLinkedAppStore::new();
        let key = external_id("s3cret", "ukey");
        let now = Utc::now();
        assert!(
            store
                .offer(LinkedApp::Pushover, key, Some("iphone"), "o1", now)
                .await
                .unwrap()
        );
        assert!(
            !store
                .offer(LinkedApp::Pushover, key, None, "o2", now)
                .await
                .unwrap()
        );
        let after = now + PUSHOVER_OFFER_COOLDOWN + chrono::Duration::seconds(1);
        assert!(
            store
                .offer(LinkedApp::Pushover, key, None, "o3", after)
                .await
                .unwrap()
        );
        assert_eq!(
            store.offered(LinkedApp::Pushover, "o1").await.unwrap(),
            Some(Some("iphone".to_string()))
        );

        let (olena, taras) = (UserId(Uuid::now_v7()), UserId(Uuid::now_v7()));
        assert_eq!(
            store
                .claim(LinkedApp::Pushover, "o1", Claimant::Person(olena))
                .await
                .unwrap(),
            LinkOutcome::Linked(olena)
        );
        assert_eq!(
            store
                .claim(LinkedApp::Pushover, "o1", Claimant::Person(taras))
                .await
                .unwrap(),
            LinkOutcome::Invalid,
            "an offer links once"
        );
        assert_eq!(
            store.offered(LinkedApp::Pushover, "o1").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn an_offer_that_never_arrived_does_not_hold_back_the_next() {
        let store = InMemoryLinkedAppStore::new();
        let key = external_id("s3cret", "ukey");
        let now = Utc::now();
        assert!(
            store
                .offer(LinkedApp::Pushover, key, None, "lost", now)
                .await
                .unwrap()
        );
        store
            .withdraw_offer(LinkedApp::Pushover, "lost")
            .await
            .unwrap();
        assert_eq!(
            store.offered(LinkedApp::Pushover, "lost").await.unwrap(),
            None
        );
        assert!(
            store
                .offer(LinkedApp::Pushover, key, None, "retry", now)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_code_spends_only_with_the_proof_it_asks_for() {
        let store = InMemoryLinkedAppStore::new();
        let olena = UserId(Uuid::now_v7());
        store
            .mint_telegram(olena, "c", Utc::now() + chrono::Duration::hours(1))
            .await
            .unwrap();
        assert_eq!(
            store
                .claim(LinkedApp::Telegram, "c", Claimant::Person(olena))
                .await
                .unwrap(),
            LinkOutcome::Invalid
        );
        assert_eq!(
            store
                .claim(LinkedApp::Pushover, "c", press(olena_telegram()))
                .await
                .unwrap(),
            LinkOutcome::Invalid
        );
    }

    /// Only a known-unlinked account is told how to link: one linked to
    /// someone outside the org is linked already, and a failed lookup knows
    /// nothing.
    #[test]
    fn only_an_account_nobody_linked_is_invited_to_link() {
        let user = UserId(Uuid::now_v7());
        assert_eq!(Linked::Member(user).member(), Some(user));
        assert!(!Linked::Member(user).invites_link());
        assert!(Linked::Unlinked.invites_link());
        for nobody in [Linked::Outsider, Linked::Unlinked, Linked::Unknown] {
            assert_eq!(nobody.member(), None);
        }
        assert!(!Linked::Outsider.invites_link());
        assert!(!Linked::Unknown.invites_link());
    }

    #[test]
    fn a_label_is_trimmed_and_capped() {
        assert_eq!(clean_label(Some("   ")), None);
        assert_eq!(clean_label(Some(" Olena ")).as_deref(), Some("Olena"));
        let long = "ї".repeat(200);
        assert_eq!(
            clean_label(Some(&long)).unwrap().chars().count(),
            MAX_LABEL_CHARS
        );
    }
}
