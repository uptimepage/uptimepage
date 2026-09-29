//! Proof that an acknowledge control on a page is ours: a signed link for a
//! transport that opens a URL, a signed button for a chat app that reports the
//! press back. Both bind the org, the incident, the channel the page went to
//! and the episode, so a control minted before a reopen takes nothing after it.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::domain::OrgId;
use crate::security::mac::{hmac_sha256, hmac_sha256_hex};

/// Bounds a leaked link: unlike a mailed one this rides in a push payload that
/// may sit on someone else's server.
pub const LINK_TTL_SECS: i64 = 7 * 24 * 60 * 60;

const BUTTON_PREFIX: &str = "a";
const BUTTON_MAC_LEN: usize = 12;
const BUTTON_RAW_LEN: usize = 16 + 4 + BUTTON_MAC_LEN;
/// Telegram's callback data takes 64 bytes, the tightest of the apps: a
/// Discord `custom_id` takes 100 characters and a Slack button value 2000.
const BUTTON_MAX: usize = 64;
const _: () = assert!(BUTTON_PREFIX.len() + (BUTTON_RAW_LEN * 4).div_ceil(3) <= BUTTON_MAX);

/// Proof for the public acknowledge link. Reproduced at verify time, nothing
/// persisted.
pub fn link_token(
    secret: &str,
    org: OrgId,
    incident_id: Uuid,
    channel_id: Uuid,
    generation: i64,
    expires_at: i64,
) -> String {
    let gen_exp = format!("{generation}:{expires_at}");
    hmac_sha256_hex(
        secret.as_bytes(),
        &[
            org.0.as_bytes(),
            incident_id.as_bytes(),
            channel_id.as_bytes(),
            gen_exp.as_bytes(),
        ],
    )
}

pub fn verify_link(
    secret: &str,
    org: OrgId,
    incident_id: Uuid,
    channel_id: Uuid,
    generation: i64,
    expires_at: i64,
    presented: &str,
) -> bool {
    link_token(secret, org, incident_id, channel_id, generation, expires_at)
        .as_bytes()
        .ct_eq(presented.as_bytes())
        .into()
}

/// `None` when the base URL or secret is unset, so no dead link reaches a phone.
pub fn link_url(
    base_url: &str,
    secret: &str,
    org: OrgId,
    incident_id: Uuid,
    channel_id: Uuid,
    generation: i64,
    now: DateTime<Utc>,
) -> Option<String> {
    let base = base_url.trim_end_matches('/');
    if base.is_empty() || secret.is_empty() {
        return None;
    }
    let exp = now.timestamp() + LINK_TTL_SECS;
    let mac = link_token(secret, org, incident_id, channel_id, generation, exp);
    Some(format!(
        "{base}/incident/ack?o={}&i={incident_id}&c={channel_id}&g={generation}&e={exp}&t={mac}",
        org.0
    ))
}

fn button_mac(
    secret: &str,
    org: OrgId,
    incident_id: Uuid,
    channel_id: Uuid,
    generation: u32,
) -> [u8; 32] {
    hmac_sha256(
        secret.as_bytes(),
        &[
            b"ack-button",
            org.0.as_bytes(),
            incident_id.as_bytes(),
            channel_id.as_bytes(),
            &generation.to_be_bytes(),
        ],
    )
}

/// The value of an Acknowledge button our own app receives in Telegram, Slack
/// or Discord, on a page about `incident_id`'s episode `generation` sent to
/// `channel_id`. A client may send any value it likes, so it is signed. It
/// names the incident and the episode; the MAC also binds the org and
/// channel, which the receiver recovers from where the press came from. Fits
/// [`BUTTON_MAX`]. `None` for an episode past what the value can carry, which
/// pages without the button.
pub fn button_data(
    secret: &str,
    org: OrgId,
    incident_id: Uuid,
    channel_id: Uuid,
    generation: i64,
) -> Option<String> {
    let generation = u32::try_from(generation).ok()?;
    let mut raw = Vec::with_capacity(BUTTON_RAW_LEN);
    raw.extend_from_slice(incident_id.as_bytes());
    raw.extend_from_slice(&generation.to_be_bytes());
    raw.extend_from_slice(
        &button_mac(secret, org, incident_id, channel_id, generation)[..BUTTON_MAC_LEN],
    );
    Some(format!("{BUTTON_PREFIX}{}", URL_SAFE_NO_PAD.encode(raw)))
}

/// A pressed button as its data describes it, not yet tied to any org.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    pub incident_id: Uuid,
    pub generation: i64,
    mac: [u8; BUTTON_MAC_LEN],
}

impl Button {
    pub fn parse(data: &str) -> Option<Self> {
        let raw = URL_SAFE_NO_PAD
            .decode(data.strip_prefix(BUTTON_PREFIX)?)
            .ok()?;
        if raw.len() != BUTTON_RAW_LEN {
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
        let expected = button_mac(secret, org, self.incident_id, channel_id, generation);
        expected[..BUTTON_MAC_LEN].ct_eq(&self.mac).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_is_bound_to_incident_episode_channel_and_expiry() {
        let org = OrgId(Uuid::from_u128(1));
        let incident = Uuid::from_u128(2);
        let channel = Uuid::from_u128(3);
        let episode = 0i64;
        let exp = 1_800_000_000i64;
        let mac = link_token("s3cret", org, incident, channel, episode, exp);
        let ok = |secret, org, incident, channel, episode, exp, mac| {
            verify_link(secret, org, incident, channel, episode, exp, mac)
        };

        assert!(ok("s3cret", org, incident, channel, episode, exp, &mac));
        assert!(!ok("other", org, incident, channel, episode, exp, &mac));
        assert!(!ok(
            "s3cret",
            OrgId(Uuid::from_u128(9)),
            incident,
            channel,
            episode,
            exp,
            &mac
        ));
        assert!(!ok(
            "s3cret",
            org,
            Uuid::from_u128(9),
            channel,
            episode,
            exp,
            &mac
        ));
        assert!(!ok(
            "s3cret",
            org,
            incident,
            Uuid::from_u128(9),
            episode,
            exp,
            &mac
        ));
        // The episode: a link minted before a reopen must not verify after one.
        assert!(!ok(
            "s3cret",
            org,
            incident,
            channel,
            episode + 1,
            exp,
            &mac
        ));
        assert!(!ok(
            "s3cret",
            org,
            incident,
            channel,
            episode,
            exp + 1,
            &mac
        ));
        assert!(!ok("s3cret", org, incident, channel, episode, exp, ""));

        // Separated in the signed input, so moving a digit from one to the other
        // cannot forge a match.
        assert_ne!(
            link_token("s3cret", org, incident, channel, 1, 23),
            link_token("s3cret", org, incident, channel, 12, 3)
        );

        let now = Utc::now();
        let url = link_url(
            "https://app.example.com/",
            "s3cret",
            org,
            incident,
            channel,
            7,
            now,
        )
        .expect("link with a base url and a secret");
        assert!(url.starts_with("https://app.example.com/incident/ack?"));
        assert!(url.contains("&g=7&"));
        assert!(url.contains(&format!("e={}", now.timestamp() + LINK_TTL_SECS)));

        // No base URL and no secret each mean no link at all.
        assert!(link_url("", "s3cret", org, incident, channel, 0, now).is_none());
        assert!(
            link_url(
                "https://app.example.com",
                "",
                org,
                incident,
                channel,
                0,
                now
            )
            .is_none()
        );
    }

    #[test]
    fn a_press_verifies_only_for_the_page_it_was_minted_for() {
        let (org, incident, channel) = (OrgId(Uuid::now_v7()), Uuid::now_v7(), Uuid::now_v7());
        let data = button_data("s3cret", org, incident, channel, 3).unwrap();
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
        let data = button_data("s3cret", org, incident, channel, 0).unwrap();
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
        assert!(button_data("s", OrgId(id), id, id, i64::from(u32::MAX) + 1).is_none());
        assert!(button_data("s", OrgId(id), id, id, -1).is_none());
    }
}
