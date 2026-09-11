//! Reusable org-scoped variables. A monitor stores `{{key}}` references in its
//! request fields and the worker resolves them at probe time, so editing a
//! variable once propagates to every monitor that references it. A secret
//! variable's value is sealed at rest and never serialized to any read surface;
//! the operator view carries `value: None` for secrets.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Type;
use utoipa::ToSchema;
use uuid::Uuid;

use super::org::OrgId;
use super::user::UserId;

/// Longest accepted key, matching the `^[a-z][a-z0-9_]{0,62}$` storage CHECK.
pub const MAX_VAR_KEY_LEN: usize = 63;

/// Strongly-typed variable id, mirroring [`MonitorShareId`](super::MonitorShareId).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type, ToSchema)]
#[serde(transparent)]
#[sqlx(transparent)]
#[schema(value_type = String, format = "uuid")]
pub struct VariableId(pub Uuid);

impl std::fmt::Display for VariableId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// One variable as the operator sees it. A plain variable carries its text; a
/// secret carries `value: None` (redacted by construction — the store never
/// decrypts for this view). The sealed bytes never reach this type.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Variable {
    pub id: VariableId,
    #[serde(skip)]
    pub org_id: OrgId,
    pub key: String,
    pub is_secret: bool,
    /// Plain value, or `None` for a secret variable.
    #[schema(nullable = true)]
    pub value: Option<String>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip)]
    pub updated_by: Option<UserId>,
}

/// Create body. `is_secret` is fixed at create — to switch, delete and recreate.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewVariable {
    pub key: String,
    #[serde(default)]
    pub is_secret: bool,
    pub value: String,
}

/// A decrypted variable ready for interpolation. Built only by the worker-side
/// resolve path; `is_secret` drives the per-field policy in the resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedVar {
    pub value: String,
    pub is_secret: bool,
}

/// `key -> resolved value` for one org, consumed by the interpolation resolver.
pub type VarMap = std::collections::HashMap<String, ResolvedVar>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKeyError {
    Empty,
    TooLong,
    LeadingNonAlpha,
    InvalidChar,
}

impl std::fmt::Display for VarKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Empty => "variable key must not be empty",
            Self::TooLong => "variable key must be at most 63 characters",
            Self::LeadingNonAlpha => "variable key must start with a lowercase letter",
            Self::InvalidChar => {
                "variable key may only contain lowercase letters, digits, and underscores"
            }
        };
        f.write_str(s)
    }
}

impl std::error::Error for VarKeyError {}

/// Validate a key against `^[a-z][a-z0-9_]{0,62}$`. The same shape is enforced
/// by the storage CHECK; this gives a clean error before the round trip.
pub fn validate_var_key(key: &str) -> Result<(), VarKeyError> {
    let bytes = key.as_bytes();
    if bytes.is_empty() {
        return Err(VarKeyError::Empty);
    }
    if bytes.len() > MAX_VAR_KEY_LEN {
        return Err(VarKeyError::TooLong);
    }
    if !bytes[0].is_ascii_lowercase() {
        return Err(VarKeyError::LeadingNonAlpha);
    }
    for &b in bytes {
        if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
            return Err(VarKeyError::InvalidChar);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_keys() {
        for k in ["a", "api_key", "x9", "z_0_9", &"a".repeat(MAX_VAR_KEY_LEN)] {
            assert!(validate_var_key(k).is_ok(), "{k} should be valid");
        }
    }

    #[test]
    fn rejects_bad_keys() {
        assert_eq!(validate_var_key(""), Err(VarKeyError::Empty));
        assert_eq!(
            validate_var_key(&"a".repeat(MAX_VAR_KEY_LEN + 1)),
            Err(VarKeyError::TooLong)
        );
        assert_eq!(validate_var_key("1abc"), Err(VarKeyError::LeadingNonAlpha));
        assert_eq!(validate_var_key("_abc"), Err(VarKeyError::LeadingNonAlpha));
        assert_eq!(validate_var_key("Abc"), Err(VarKeyError::LeadingNonAlpha));
        assert_eq!(validate_var_key("aB"), Err(VarKeyError::InvalidChar));
        assert_eq!(validate_var_key("a-b"), Err(VarKeyError::InvalidChar));
        assert_eq!(validate_var_key("a b"), Err(VarKeyError::InvalidChar));
    }

    #[test]
    fn secret_value_skipped_in_serialization() {
        let v = Variable {
            id: VariableId(Uuid::nil()),
            org_id: OrgId(Uuid::nil()),
            key: "api_key".into(),
            is_secret: true,
            value: None,
            updated_at: DateTime::<Utc>::MIN_UTC,
            updated_by: None,
        };
        let json = serde_json::to_value(&v).unwrap();
        assert!(json.get("value").unwrap().is_null());
        assert!(json.get("org_id").is_none());
    }
}
