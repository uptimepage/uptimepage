use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::error::AppError;

#[derive(Debug, Clone)]
pub struct RegistrationAnswer {
    pub expiration: DateTime<Utc>,
    pub registrar: Option<String>,
}

/// Permanent verdicts about the TLD, not about the attempt.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RegistrationError {
    #[error("no registration lookup available for TLD '.{tld}'")]
    TldUnsupported { tld: String },

    #[error("the .{tld} registry does not publish domain expiry dates")]
    NoPublicExpiry { tld: String },
}

impl From<RegistrationError> for AppError {
    fn from(err: RegistrationError) -> Self {
        AppError::Other(err.into())
    }
}

/// Saves callers from matching on message strings.
pub fn tld_verdict(err: &anyhow::Error) -> Option<&RegistrationError> {
    err.downcast_ref::<RegistrationError>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tld_verdict_survives_anyhow_wrapping() {
        let err: anyhow::Error = RegistrationError::NoPublicExpiry { tld: "de".into() }.into();
        let wrapped = err.context("probing registration");
        assert!(matches!(
            tld_verdict(&wrapped),
            Some(RegistrationError::NoPublicExpiry { .. })
        ));
    }

    #[test]
    fn unrelated_errors_have_no_verdict() {
        let err = anyhow::anyhow!("connection reset");
        assert!(tld_verdict(&err).is_none());
    }
}
