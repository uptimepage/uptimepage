//! Agent bearer tokens. Distinct from the `sm_live_` tenant tokens: operator-
//! tier, tied to no user, stored hashed on the `agents` row. The `sm_agent_`
//! prefix lets an operator (and secret scanners) tell the two apart at a glance.
//! Verification reuses [`crate::security::token_hash`] (argon2id).

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::TryRng;
use rand::rngs::SysRng;

/// Public prefix marking an agent token. A bearer value starting with this
/// routes to the agent-auth path; `sm_live_` stays the tenant path.
pub const PREFIX: &str = "sm_agent_";

const RANDOM_BYTES: usize = 32;

/// Visible prefix persisted for the lookup narrow: `sm_agent_` + 8 base64url
/// chars (~48 bits). The argon2 verify disambiguates a prefix collision.
pub const PREFIX_VISIBLE: usize = PREFIX.len() + 8;

/// Fresh raw token: `sm_agent_` + 43 base64url chars (32 random bytes).
pub fn generate_raw() -> String {
    let mut bytes = [0u8; RANDOM_BYTES];
    SysRng
        .try_fill_bytes(&mut bytes)
        .expect("SysRng must succeed for agent-token generation");
    format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// The visible prefix of a raw token, for the indexed lookup.
pub fn visible_prefix(raw: &str) -> &str {
    let n = PREFIX_VISIBLE.min(raw.len());
    &raw[..n]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_is_prefixed_and_long() {
        let raw = generate_raw();
        assert!(raw.starts_with(PREFIX));
        assert_eq!(raw.len(), PREFIX.len() + 43);
        assert_eq!(visible_prefix(&raw).len(), PREFIX_VISIBLE);
    }
}
