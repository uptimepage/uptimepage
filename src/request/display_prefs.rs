//! Issues the per-user display-preference cookies — theme (`sm_theme`) and time
//! format (`sm_time_format`) — that the page-shell boot script and
//! `localtime.js` read on first paint. One read, both cookies: every
//! sign-in goes through this so a new auth path can't ship one preference
//! cookie and silently forget the other. The DB columns remain the
//! source of truth; the per-preference PATCH handlers set their own cookie.

use sqlx::PgPool;
use tower_cookies::Cookies;

use crate::domain::UserId;
use crate::error::Result;
use crate::request::{theme, time_format};
use crate::storage::users as users_store;

pub async fn issue_cookies(
    pool: &PgPool,
    secure: bool,
    cookies: &Cookies,
    user: UserId,
) -> Result<()> {
    let prefs = users_store::get_display_prefs(pool, user).await?;
    cookies.add(theme::build_cookie(prefs.theme, secure));
    cookies.add(time_format::build_cookie(prefs.time_format, secure));
    Ok(())
}
