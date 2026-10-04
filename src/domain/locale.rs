use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Language of a status page's public surface and its subscriber emails.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Locale {
    #[default]
    En,
    De,
}

impl Locale {
    pub const ALL: [Self; 2] = [Self::En, Self::De];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::De => "de",
        }
    }

    pub fn native_name(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::De => "Deutsch",
        }
    }

    pub fn from_db(s: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|l| l.as_str() == s)
            .unwrap_or_else(|| {
                tracing::warn!(value = s, "unknown public_locale in DB, falling back to en");
                Self::default()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_round_trip_covers_every_variant() {
        for l in Locale::ALL {
            assert_eq!(Locale::from_db(l.as_str()), l);
        }
        assert_eq!(Locale::from_db("fr"), Locale::En);
    }

    #[test]
    fn wire_form_matches_db_form() {
        for l in Locale::ALL {
            assert_eq!(serde_json::to_value(l).unwrap(), l.as_str());
        }
        assert!(serde_json::from_str::<Locale>("\"fr\"").is_err());
    }
}
