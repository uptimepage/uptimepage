//! Authentication primitives: OAuth flow, sessions, login audit, fingerprint
//! hashing. Endpoints sit in `api::handlers::auth` / `api::handlers::me` and
//! use these helpers.
//!
//! API tokens, invitations and magic-link verification are scaffolded by the
//! schema in `007_auth.up.sql` and the `EmailSender` trait in `crate::email`
//! but their flows arrive in later phases.

pub mod account;
pub mod agent_token;
pub mod api_tokens;
pub mod consent;
pub mod discord;
pub mod email_norm;
pub mod fingerprint;
pub mod github;
pub mod gitlab;
pub mod google;
pub mod invitations;
pub mod login_audit;
pub mod mac;
pub mod magic_link;
pub mod microsoft;
pub mod oauth_login;
pub mod oauth_state;
pub mod passkey;
pub mod scope;
pub mod session;
pub mod slack;
pub mod token_hash;
pub mod url;

pub use fingerprint::{ensure_fingerprint_salt, hash_fingerprint};

use sha2::{Digest, Sha256};

/// SHA-256 hex of any string. Used wherever the schema stores a hashed
/// lookup key for a high-entropy random token (session cookies, OAuth state)
/// so a table or query-log leak yields hashes instead of replayable tokens.
/// The inputs are already 256 bits of unguessable entropy — argon2 would
/// burn CPU per lookup for no extra protection.
pub fn sha256_hex(raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    hex::encode(digest)
}
