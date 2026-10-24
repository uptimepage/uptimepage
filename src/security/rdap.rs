//! Pure RDAP field parsing for the scheduled domain-expiry monitor.

use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(crate) struct DomainResponse {
    #[serde(default)]
    events: Vec<Event>,
    #[serde(default)]
    entities: Vec<Entity>,
}

impl DomainResponse {
    pub fn expiration(&self) -> Option<DateTime<Utc>> {
        let event = self
            .events
            .iter()
            .find(|e| e.action.eq_ignore_ascii_case("expiration"))?;
        DateTime::parse_from_rfc3339(&event.date)
            .ok()
            .map(|d| d.with_timezone(&Utc))
    }

    pub fn registrar(&self) -> Option<String> {
        let entity = self
            .entities
            .iter()
            .find(|e| e.roles.iter().any(|r| r.eq_ignore_ascii_case("registrar")))?;
        let entries = entity.vcard.as_ref()?.as_array()?.get(1)?.as_array()?;
        entries.iter().find_map(|entry| {
            let fields = entry.as_array()?;
            (fields.first()?.as_str()? == "fn")
                .then(|| fields.get(3)?.as_str().map(str::to_owned))?
        })
    }
}

#[derive(Debug, Deserialize)]
struct Event {
    #[serde(rename = "eventAction")]
    action: String,
    #[serde(rename = "eventDate")]
    date: String,
}

#[derive(Debug, Deserialize)]
struct Entity {
    #[serde(default)]
    roles: Vec<String>,
    #[serde(rename = "vcardArray")]
    vcard: Option<serde_json::Value>,
}

/// Established registry endpoint used when IANA's bootstrap is incomplete.
pub(crate) fn override_url(tld: &str) -> Option<&'static str> {
    (tld == "io").then_some("https://rdap.identitydigital.services/rdap/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_expiration_not_registration_or_last_update() {
        let response: DomainResponse = serde_json::from_value(json!({
            "events": [
                {"eventAction": "registration", "eventDate": "2020-01-01T00:00:00Z"},
                {"eventAction": "expiration", "eventDate": "2027-01-01T02:00:00+02:00"}
            ],
            "entities": [{"roles": ["registrar"], "vcardArray": ["vcard", [
                ["fn", {}, "text", "Example Registrar"]
            ]]}]
        }))
        .unwrap();
        assert_eq!(
            response.expiration().unwrap().to_rfc3339(),
            "2027-01-01T00:00:00+00:00"
        );
        assert_eq!(response.registrar().as_deref(), Some("Example Registrar"));
    }

    #[test]
    fn missing_or_bad_dates_are_not_guessed() {
        for body in [
            json!({}),
            json!({"events": [{"eventAction": "expiration", "eventDate": "unknown"}]}),
        ] {
            let response: DomainResponse = serde_json::from_value(body).unwrap();
            assert!(response.expiration().is_none());
            assert!(response.registrar().is_none());
        }
    }
}
