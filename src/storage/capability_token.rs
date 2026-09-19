//! Capability-token discipline shared by share links and heartbeat pings: a
//! 256-bit URL-safe secret whose SHA-256 hex is the lookup key and whose sealed
//! copy is the only reversible form stored. Holding the raw token is the proof.

use crate::error::{AppError, Result};
use crate::security::sha256_hex;
use crate::security::token_hash::generate_raw_token;
use crate::security::{Cipher, open_str, seal_str};

/// A freshly minted token: the raw secret to hand out once, its hash for the
/// lookup column, and the sealed copy for the reversible column.
pub struct Minted {
    pub raw: String,
    pub hash: String,
    pub sealed: String,
}

/// Mint a token. Seal falls back to plaintext without a KEK, the same fallback
/// as target credentials.
pub fn mint(cipher: Option<&Cipher>) -> Result<Minted> {
    let raw = generate_raw_token();
    let hash = sha256_hex(&raw);
    let sealed = seal_str(&raw, cipher)
        .map_err(|e| AppError::Other(anyhow::anyhow!("capability token encryption: {e}")))?;
    Ok(Minted { raw, hash, sealed })
}

/// Lookup key for a presented raw token.
pub fn hash(raw: &str) -> String {
    sha256_hex(raw)
}

/// Recover the raw token from its sealed copy. `None` when it is an envelope
/// but no KEK is available (e.g. the key was rotated out).
pub fn open(sealed: &str, cipher: Option<&Cipher>) -> Option<String> {
    open_str(sealed, cipher)
}
