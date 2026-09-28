//! The Acknowledge button on a central-bot page. A Telegram client may send any
//! callback data it likes, so the data is signed. It names the incident and the
//! episode; the MAC also binds the org and channel, which the receiver recovers
//! from the chat the press came from rather than from the data.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::domain::OrgId;
use crate::security::mac::hmac_sha256;

const PREFIX: &str = "a";
const MAC_LEN: usize = 12;
const RAW_LEN: usize = 16 + 4 + MAC_LEN;

fn mac(secret: &str, org: OrgId, incident_id: Uuid, channel_id: Uuid, generation: u32) -> [u8; 32] {
    hmac_sha256(
        secret.as_bytes(),
        &[
            b"telegram-ack",
            org.0.as_bytes(),
            incident_id.as_bytes(),
            channel_id.as_bytes(),
            &generation.to_be_bytes(),
        ],
    )
}

/// Callback data for the button on a page about `incident_id`'s episode
/// `generation`, sent to `channel_id`. Within Telegram's 64 bytes. `None` for
/// an episode past what the data can carry, which pages without the button.
pub fn callback_data(
    secret: &str,
    org: OrgId,
    incident_id: Uuid,
    channel_id: Uuid,
    generation: i64,
) -> Option<String> {
    let generation = u32::try_from(generation).ok()?;
    let mut raw = Vec::with_capacity(RAW_LEN);
    raw.extend_from_slice(incident_id.as_bytes());
    raw.extend_from_slice(&generation.to_be_bytes());
    raw.extend_from_slice(&mac(secret, org, incident_id, channel_id, generation)[..MAC_LEN]);
    Some(format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(raw)))
}

/// A pressed button as its data describes it, not yet tied to any org.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    pub incident_id: Uuid,
    pub generation: i64,
    mac: [u8; MAC_LEN],
}

impl Button {
    pub fn parse(data: &str) -> Option<Self> {
        let raw = URL_SAFE_NO_PAD.decode(data.strip_prefix(PREFIX)?).ok()?;
        if raw.len() != RAW_LEN {
            return None;
        }
        Some(Self {
            incident_id: Uuid::from_slice(&raw[..16]).ok()?,
            generation: i64::from(u32::from_be_bytes(raw[16..20].try_into().ok()?)),
            mac: raw[20..].try_into().ok()?,
        })
    }

    /// Whether this button was minted for a page to `channel_id` in `org`.
    pub fn minted_for(&self, secret: &str, org: OrgId, channel_id: Uuid) -> bool {
        let Ok(generation) = u32::try_from(self.generation) else {
            return false;
        };
        let expected = mac(secret, org, self.incident_id, channel_id, generation);
        expected[..MAC_LEN].ct_eq(&self.mac).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_press_verifies_only_for_the_page_it_was_minted_for() {
        let (org, incident, channel) = (OrgId(Uuid::now_v7()), Uuid::now_v7(), Uuid::now_v7());
        let data = callback_data("s3cret", org, incident, channel, 3).unwrap();
        assert!(data.len() <= 64, "Telegram caps callback data at 64 bytes");

        let press = Button::parse(&data).unwrap();
        assert_eq!((press.incident_id, press.generation), (incident, 3));
        assert!(press.minted_for("s3cret", org, channel));
        assert!(!press.minted_for("s3cret", OrgId(Uuid::now_v7()), channel));
        assert!(!press.minted_for("s3cret", org, Uuid::now_v7()));
        assert!(!press.minted_for("other", org, channel));
    }

    #[test]
    fn a_press_moved_to_another_episode_or_incident_fails() {
        let (org, incident, channel) = (OrgId(Uuid::now_v7()), Uuid::now_v7(), Uuid::now_v7());
        let data = callback_data("s3cret", org, incident, channel, 0).unwrap();
        let mut raw = URL_SAFE_NO_PAD.decode(&data[1..]).unwrap();
        raw[19] = 1;
        let later = Button::parse(&format!("a{}", URL_SAFE_NO_PAD.encode(&raw))).unwrap();
        assert!(!later.minted_for("s3cret", org, channel));

        let mut raw = URL_SAFE_NO_PAD.decode(&data[1..]).unwrap();
        raw[..16].copy_from_slice(Uuid::now_v7().as_bytes());
        let other = Button::parse(&format!("a{}", URL_SAFE_NO_PAD.encode(&raw))).unwrap();
        assert!(!other.minted_for("s3cret", org, channel));
    }

    #[test]
    fn junk_data_is_no_press() {
        assert_eq!(Button::parse(""), None);
        assert_eq!(Button::parse("a"), None);
        assert_eq!(Button::parse("ack"), None);
        assert_eq!(Button::parse("b".repeat(44).as_str()), None);
    }

    #[test]
    fn an_episode_past_the_data_gets_no_button() {
        let id = Uuid::now_v7();
        assert!(callback_data("s", OrgId(id), id, id, i64::from(u32::MAX) + 1).is_none());
        assert!(callback_data("s", OrgId(id), id, id, -1).is_none());
    }
}
