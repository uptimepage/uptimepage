//! Authentication: OAuth flow, sessions, passkeys, API tokens, invitations,
//! magic links, login audit and fingerprint hashing. Endpoints sit in
//! `api::handlers::auth` / `api::handlers::me` and use these helpers.

pub mod account;
pub mod api_tokens;
pub mod discord;
pub mod email_norm;
pub mod fingerprint;
pub mod github;
pub mod gitlab;
pub mod google;
pub mod invitations;
pub mod login_audit;
pub mod magic_link;
pub mod microsoft;
pub mod oauth_login;
pub mod oauth_state;
pub mod passkey;
pub mod scope;
pub mod session;
pub mod slack;
pub mod url;

pub use fingerprint::{ensure_fingerprint_salt, hash_fingerprint};

use crate::config::AppConfig;
use crate::domain::WaysIn;

/// A linked provider counts only where this deployment will complete a sign-in
/// for it, and email only where the mail is actually delivered.
pub fn ways_in(cfg: &AppConfig) -> WaysIn {
    WaysIn {
        enabled_providers: cfg.auth.enabled_login_providers(),
        email_is_a_way_back: cfg.auth.magic_link_enabled() && cfg.email.delivers(),
        passkeys_open_the_account: passkey::login_enabled(&cfg.auth),
    }
}
