use std::str::FromStr;

use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// IANA zone a status page states its times in where no reader's clock can
/// help: its subscriber emails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(try_from = "String", into = "String")]
#[schema(value_type = String, example = "Europe/Berlin")]
pub struct PageTimezone(Tz);

impl PageTimezone {
    pub fn tz(self) -> Tz {
        self.0
    }

    pub fn as_str(self) -> &'static str {
        self.0.name()
    }

    pub fn from_db(s: &str) -> Self {
        Tz::from_str(s).map(Self).unwrap_or_else(|_| {
            tracing::warn!(
                value = s,
                "unknown public_timezone in DB, falling back to UTC"
            );
            Self::default()
        })
    }
}

impl Default for PageTimezone {
    fn default() -> Self {
        Self(Tz::UTC)
    }
}

impl TryFrom<String> for PageTimezone {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        Tz::from_str(&name)
            .map(Self)
            .map_err(|_| format!("`{name}` is not a known IANA time zone"))
    }
}

impl From<PageTimezone> for String {
    fn from(zone: PageTimezone) -> Self {
        zone.as_str().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_name_round_trips() {
        let zone = PageTimezone::from_db("Europe/Berlin");
        assert_eq!(zone.as_str(), "Europe/Berlin");
        assert_eq!(zone.tz(), chrono_tz::Europe::Berlin);
    }

    #[test]
    fn a_name_no_longer_known_reads_as_utc() {
        assert_eq!(
            PageTimezone::from_db("Mars/Olympus"),
            PageTimezone::default()
        );
        assert_eq!(PageTimezone::default().as_str(), "UTC");
    }

    #[test]
    fn the_wire_form_is_the_iana_name() {
        let zone: PageTimezone = serde_json::from_str("\"America/Sao_Paulo\"").unwrap();
        assert_eq!(serde_json::to_value(zone).unwrap(), "America/Sao_Paulo");
    }

    #[test]
    fn an_unknown_or_miscased_name_is_refused() {
        for name in ["\"Mars/Olympus\"", "\"europe/berlin\"", "\"\""] {
            assert!(
                serde_json::from_str::<PageTimezone>(name).is_err(),
                "{name}"
            );
        }
    }
}
