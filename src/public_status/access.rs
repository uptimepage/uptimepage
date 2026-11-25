//! The one decision every surface of a status page asks before it serves:
//! may this request read the page? A public page answers yes without a
//! query; a private one needs a session through a way in that is still open:
//! a viewer whose address the list still covers, or the page password, still
//! on. A later way in (an office network) is a new branch in
//! [`PageGate::check`], not a change to any surface.

use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderMap;
use axum::http::header::{COOKIE, HOST, ORIGIN};
use moka::future::Cache;
use moka::sync::Cache as SyncCache;
use sqlx::PgPool;
use subtle::ConstantTimeEq;
use tower_cookies::cookie::time::Duration as CookieDuration;
use tower_cookies::cookie::{Cookie, SameSite};
use url::Url;
use uuid::Uuid;

use crate::domain::{PageAccess, PageRef, PageSessionMethod, StatusPageId};
use crate::error::public::PublicAppError;
use crate::storage::page_access::{password, sessions, settings, viewers};

/// Host-only on the page's own host, so one page's session never reaches
/// another page, even a sibling under the same base domain. Pages sharing the
/// operator host on a path-based deploy scope theirs by path instead.
pub const SESSION_COOKIE: &str = "_up_page";

/// Browsers cap a cookie's life at 400 days, so a session the owner left
/// unlimited is re-proved by email after that, or swept sooner when idle.
const UNLIMITED_COOKIE_DAYS: i64 = 400;

const SETTINGS_TTL: Duration = Duration::from_secs(10);
const TOUCH_DEBOUNCE: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct Viewer {
    pub id: Uuid,
    pub email: String,
}

#[derive(Debug, Clone)]
pub enum Access {
    /// Anyone with the URL.
    Public,
    /// A private page, read by a signed-in viewer.
    Viewer(Viewer),
    /// A private page, read by someone who knew the page password. Nobody is
    /// named. `session` is the session row's hash, for grants that read it
    /// again under a lock and for the feed link a page shows, which only a
    /// live session gets; `None` for the page-password feed link itself.
    Password { session: Option<String> },
    /// A private page, and nothing that may read it.
    SignInRequired(Arc<PageAccess>),
}

impl Access {
    pub fn is_private(&self) -> bool {
        !matches!(self, Self::Public)
    }

    pub fn viewer(&self) -> Option<&Viewer> {
        match self {
            Self::Viewer(v) => Some(v),
            _ => None,
        }
    }
}

/// Settings cached briefly per page; an owner's save drops the entry so the
/// instance that took it applies a removed rule on the next request.
pub struct PageGate {
    settings: Cache<StatusPageId, Arc<PageAccess>>,
    touched: SyncCache<String, ()>,
}

impl Default for PageGate {
    fn default() -> Self {
        Self {
            settings: Cache::builder()
                .time_to_live(SETTINGS_TTL)
                .max_capacity(10_000)
                .build(),
            touched: SyncCache::builder()
                .time_to_live(TOUCH_DEBOUNCE)
                .max_capacity(100_000)
                .build(),
        }
    }
}

impl PageGate {
    /// The page's settings and list. A read error fails closed: a private page
    /// must not open because its settings could not be read.
    pub async fn settings(
        &self,
        pool: Option<&PgPool>,
        page: PageRef,
    ) -> Result<Arc<PageAccess>, PublicAppError> {
        let Some(pool) = pool else {
            return Ok(Arc::new(PageAccess::default()));
        };
        let pool = pool.clone();
        self.settings
            .try_get_with(page.page, async move {
                settings::load(&pool, page)
                    .await
                    .map(|a| Arc::new(a.unwrap_or_default()))
            })
            .await
            .map_err(|e| PublicAppError::Internal(anyhow::anyhow!("page access settings: {e}")))
    }

    pub async fn invalidate(&self, page: StatusPageId) {
        self.settings.invalidate(&page).await;
    }

    pub async fn check(
        &self,
        pool: Option<&PgPool>,
        page: PageRef,
        headers: &HeaderMap,
    ) -> Result<Access, PublicAppError> {
        let access = self.settings(pool, page).await?;
        if !access.is_private() {
            return Ok(Access::Public);
        }
        let Some(pool) = pool else {
            return Ok(Access::SignInRequired(access));
        };
        // A stale session beside a live one for the same page must not shut
        // the reader out, so each is judged in turn.
        for cookie in session_cookies(headers) {
            let Some(session) = sessions::lookup(pool, page, &cookie).await? else {
                continue;
            };
            if let Some(reader) = reader_of(&access, &session) {
                self.touch(pool, page, &session);
                return Ok(reader);
            }
        }
        Ok(Access::SignInRequired(access))
    }

    /// A viewer's personal feed link stands in for the cookie a feed reader
    /// does not have, under the same conditions as a session.
    pub async fn check_feed(
        &self,
        pool: Option<&PgPool>,
        page: PageRef,
        secret: &str,
        viewer: Uuid,
        token: &str,
    ) -> Result<Option<Viewer>, PublicAppError> {
        let Some(pool) = pool else {
            return Ok(None);
        };
        if secret.is_empty() || !verify_feed_token(secret, page.page, viewer, token) {
            return Ok(None);
        }
        let access = self.settings(Some(pool), page).await?;
        let Some(v) = viewers::get(pool, page, viewer).await? else {
            return Ok(None);
        };
        if v.blocked_at.is_some() || !access.admits(&v.email) {
            return Ok(None);
        }
        Ok(Some(Viewer {
            id: v.id,
            email: v.email,
        }))
    }

    /// The page-password feed link, for a reader with no cookie. It is cut
    /// from the stored hash, so a new password kills every copy, and while
    /// the password is off it is refused like no link at all.
    pub async fn check_password_feed(
        &self,
        pool: Option<&PgPool>,
        page: PageRef,
        secret: &str,
        token: &str,
    ) -> Result<bool, PublicAppError> {
        let Some(pool) = pool else {
            return Ok(false);
        };
        if secret.is_empty() {
            return Ok(false);
        }
        let Some(hash) = password::live_hash(pool, page).await? else {
            return Ok(false);
        };
        Ok(bool::from(
            password_feed_token(secret, page.page, &hash)
                .as_bytes()
                .ct_eq(token.as_bytes()),
        ))
    }

    /// Visit stamps, at most once a minute per session and off the request
    /// path: a lost stamp costs a history row, never a page view.
    fn touch(&self, pool: &PgPool, page: PageRef, session: &sessions::ActiveSession) {
        if self.touched.contains_key(&session.id_hash) {
            return;
        }
        self.touched.insert(session.id_hash.clone(), ());
        let pool = pool.clone();
        let id_hash = session.id_hash.clone();
        let viewer = session.viewer.as_ref().map(|v| v.id);
        tokio::spawn(async move {
            if let Err(e) = sessions::touch(&pool, page, &id_hash).await {
                tracing::warn!(error = %e, "page session touch failed");
            }
            if let Some(viewer) = viewer
                && let Err(e) = viewers::touch(&pool, page, viewer).await
            {
                tracing::warn!(error = %e, "page viewer touch failed");
            }
        });
    }
}

/// Browsers send `Origin` on every POST, so a cross-site form or fetch shows
/// its own host here. A request with none did not come from a browser page.
pub fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let host = headers.get(HOST).and_then(|v| v.to_str().ok());
    Url::parse(origin).ok().is_some_and(|o| {
        let authority = match (o.host_str(), o.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_owned(),
            _ => return false,
        };
        host.is_some_and(|h| h.eq_ignore_ascii_case(&authority))
    })
}

/// Who a session reads the page as, while its way in is still open.
fn reader_of(access: &PageAccess, session: &sessions::ActiveSession) -> Option<Access> {
    // The owner's current length, so shortening it ends older sessions too,
    // not only the ones opened after the change.
    if let Some(days) = access.settings.session_days
        && session.created_at < chrono::Utc::now() - chrono::Duration::days(i64::from(days))
    {
        return None;
    }
    match (session.method, &session.viewer) {
        (PageSessionMethod::Email, Some(v)) if !v.blocked && access.admits(&v.email) => {
            Some(Access::Viewer(Viewer {
                id: v.id,
                email: v.email.clone(),
            }))
        }
        (PageSessionMethod::Password, None) if access.settings.password_enabled => {
            Some(Access::Password {
                session: Some(session.id_hash.clone()),
            })
        }
        _ => None,
    }
}

/// Session cookies one request may hold for a page: the page's own path, the
/// host root, and one more set from a sibling host. Each costs a lookup, so a
/// request stuffed with them must not buy more.
const MAX_SESSION_COOKIES: usize = 3;

/// The session cookies the browser sent. A path-based deploy's pages each
/// scope their own, so one request can carry several, and a browser need not
/// send the nearest first.
pub fn session_cookies(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(Cookie::split_parse)
        .filter_map(Result::ok)
        .filter(|c| c.name() == SESSION_COOKIE && !c.value().is_empty())
        .map(|c| c.value().to_owned())
        .take(MAX_SESSION_COOKIES)
        .collect()
}

pub fn session_set_cookie(
    value: &str,
    session_days: Option<i32>,
    secure: bool,
    path: &str,
) -> Cookie<'static> {
    let mut c = Cookie::new(SESSION_COOKIE, value.to_owned());
    c.set_http_only(true);
    c.set_secure(secure);
    c.set_same_site(SameSite::Lax);
    c.set_path(path.to_owned());
    c.set_max_age(CookieDuration::days(
        session_days.map_or(UNLIMITED_COOKIE_DAYS, i64::from),
    ));
    c
}

pub fn session_clear_cookie(secure: bool, path: &str) -> Cookie<'static> {
    let mut c = session_set_cookie("", Some(0), secure, path);
    c.set_max_age(CookieDuration::ZERO);
    c
}

/// Stateless, like the unsubscribe link: nothing to store, and it stops
/// working the moment the viewer is blocked or leaves the list.
pub fn feed_token(secret: &str, page: StatusPageId, viewer: Uuid) -> String {
    crate::security::mac::hmac_sha256_hex(
        secret.as_bytes(),
        &[b"page-feed:", page.0.as_bytes(), viewer.as_bytes()],
    )
}

fn verify_feed_token(secret: &str, page: StatusPageId, viewer: Uuid, presented: &str) -> bool {
    feed_token(secret, page, viewer)
        .as_bytes()
        .ct_eq(presented.as_bytes())
        .into()
}

/// The feed link a password visitor is shown: one per page and password,
/// since the password names nobody.
pub fn password_feed_token(secret: &str, page: StatusPageId, password_hash: &str) -> String {
    crate::security::mac::hmac_sha256_hex(
        secret.as_bytes(),
        &[
            b"page-password-feed:",
            page.0.as_bytes(),
            password_hash.as_bytes(),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_cookie_is_found_among_others() {
        let mut h = HeaderMap::new();
        h.insert(COOKIE, "a=1; _up_page=abc; b=2".parse().unwrap());
        assert_eq!(session_cookies(&h), ["abc"]);
        h.insert(COOKIE, "_up_page_x=abc".parse().unwrap());
        assert!(session_cookies(&h).is_empty());
    }

    #[test]
    fn every_page_session_on_a_shared_host_is_a_candidate() {
        let mut h = HeaderMap::new();
        h.insert(
            COOKIE,
            "_up_page=one; _up_page=; _up_page=two".parse().unwrap(),
        );
        assert_eq!(session_cookies(&h), ["one", "two"]);
    }

    #[test]
    fn a_request_stuffed_with_session_cookies_buys_no_more_lookups() {
        let stuffed = (0..50)
            .map(|i| format!("_up_page=s{i}"))
            .collect::<Vec<_>>()
            .join("; ");
        let mut h = HeaderMap::new();
        h.insert(COOKIE, stuffed.parse().unwrap());
        assert_eq!(session_cookies(&h).len(), MAX_SESSION_COOKIES);
    }

    #[test]
    fn a_scoped_page_keeps_its_session_cookie_under_its_path() {
        let c = session_set_cookie("v", None, true, "/status/two");
        assert_eq!(c.path(), Some("/status/two"));
        assert_eq!(
            session_clear_cookie(true, "/status/two").path(),
            Some("/status/two")
        );
    }

    #[test]
    fn the_session_cookie_is_host_only() {
        let c = session_set_cookie("v", None, true, "/");
        assert_eq!(c.domain(), None);
        assert_eq!(c.path(), Some("/"));
        assert_eq!(c.http_only(), Some(true));
        assert_eq!(
            c.max_age(),
            Some(CookieDuration::days(UNLIMITED_COOKIE_DAYS))
        );
    }

    #[test]
    fn a_feed_token_names_one_viewer_on_one_page() {
        let page = StatusPageId(Uuid::from_u128(1));
        let viewer = Uuid::from_u128(2);
        let t = feed_token("s", page, viewer);
        assert!(verify_feed_token("s", page, viewer, &t));
        assert!(!verify_feed_token(
            "s",
            StatusPageId(Uuid::from_u128(3)),
            viewer,
            &t
        ));
        assert!(!verify_feed_token("s", page, Uuid::from_u128(4), &t));
        assert!(!verify_feed_token("other", page, viewer, &t));
    }

    #[test]
    fn a_password_feed_token_names_one_page_and_one_password() {
        let page = StatusPageId(Uuid::from_u128(1));
        let t = password_feed_token("s", page, "$argon2id$a");
        assert_eq!(t, password_feed_token("s", page, "$argon2id$a"));
        assert_ne!(t, password_feed_token("s", page, "$argon2id$b"));
        assert_ne!(
            t,
            password_feed_token("s", StatusPageId(Uuid::from_u128(2)), "$argon2id$a")
        );
        // Not a viewer link under another name.
        assert_ne!(t, feed_token("s", page, Uuid::from_u128(0)));
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn a_post_from_another_host_is_not_same_origin() {
        assert!(same_origin(&headers(&[("host", "acme.example.com")])));
        assert!(same_origin(&headers(&[
            ("host", "acme.example.com"),
            ("origin", "https://acme.example.com"),
        ])));
        assert!(same_origin(&headers(&[
            ("host", "acme.localhost:8080"),
            ("origin", "http://acme.localhost:8080"),
        ])));
        assert!(!same_origin(&headers(&[
            ("host", "acme.example.com"),
            ("origin", "https://other.example.com"),
        ])));
        assert!(!same_origin(&headers(&[
            ("host", "acme.example.com"),
            ("origin", "null"),
        ])));
    }
}
