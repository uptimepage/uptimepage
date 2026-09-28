//! What ties an app account to a person. A link offer is a single-use random
//! code carrying one side of the link; the other side proves itself where the
//! code is spent. Telegram vouches for who pressed Start on a code minted for a
//! signed-in person, and a signed-in session vouches for who opened a Pushover
//! offer pushed to one account.

use chrono::Duration;

use crate::domain::ExternalId;
use crate::security::mac::hmac_sha256;

/// Sets an account link apart from a channel link code, which is 43 random
/// base64url characters with no prefix.
const TELEGRAM_PREFIX: &str = "me-";
/// Long enough to find the phone, short enough that a screenshot of it goes
/// stale.
pub const TELEGRAM_LINK_TTL: Duration = Duration::hours(1);
/// The offer waits in the Pushover app until someone opens it.
pub const PUSHOVER_LINK_TTL: Duration = Duration::hours(24);
/// Someone who acknowledges from Pushover without linking hears about it at
/// most this often, however many incidents they take.
pub const PUSHOVER_OFFER_COOLDOWN: Duration = Duration::days(7);

/// Keyed, so a leaked row cannot be walked back to a Telegram id by hashing
/// every number there is.
pub fn external_id(secret: &str, raw_id: &str) -> ExternalId {
    ExternalId(hmac_sha256(
        secret.as_bytes(),
        &[b"linked-app-account\0", raw_id.as_bytes()],
    ))
}

/// The `/start` payload carrying a Telegram link code: within Telegram's 64
/// characters of `[A-Za-z0-9_-]`.
pub fn telegram_start_payload(code: &str) -> String {
    format!("{TELEGRAM_PREFIX}{code}")
}

/// The link code in a `/start` payload shaped like an account link rather
/// than a channel link code. Says nothing about whether it is live.
pub fn telegram_start_code(payload: &str) -> Option<&str> {
    payload
        .strip_prefix(TELEGRAM_PREFIX)
        .filter(|code| code.len() == 43)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::token_hash::generate_raw_token;

    #[test]
    fn a_start_payload_fits_telegram_and_gives_its_code_back() {
        let code = generate_raw_token();
        let payload = telegram_start_payload(&code);
        assert!(payload.len() <= 64, "Telegram caps a start payload at 64");
        assert!(
            payload
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{payload}"
        );
        assert_eq!(telegram_start_code(&payload), Some(code.as_str()));
    }

    #[test]
    fn a_channel_link_code_is_never_taken_for_an_account_link() {
        let code = generate_raw_token();
        assert_eq!(telegram_start_code(&code), None);
        assert_eq!(telegram_start_code("me-short"), None);
    }

    #[test]
    fn an_app_account_id_needs_the_secret() {
        assert_eq!(external_id("s3cret", "77"), external_id("s3cret", "77"));
        assert_ne!(external_id("s3cret", "77"), external_id("other", "77"));
        assert_ne!(external_id("s3cret", "77"), external_id("s3cret", "78"));
    }
}
