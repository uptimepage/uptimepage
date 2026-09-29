//! The acknowledge page link, for transports whose buttons can only open a URL.

use serde::Deserialize;
use uuid::Uuid;

use crate::domain::OrgId;

/// The org rides along so a member of several lands in the one the alert is
/// about, the channel so switching its button off withdraws the alerts
/// already sent, and the episode so an alert kept through a reopen takes
/// nothing after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlertLink {
    pub org: OrgId,
    pub channel: Uuid,
    pub episode: i64,
}

impl AlertLink {
    pub fn path(&self, incident_id: Uuid) -> String {
        format!(
            "/incidents/{incident_id}/acknowledge?org={}&channel={}&episode={}",
            self.org.0, self.channel, self.episode
        )
    }
}

#[derive(Debug, Deserialize)]
pub struct AlertLinkQuery {
    #[serde(default)]
    org: String,
    #[serde(default)]
    channel: String,
    #[serde(default)]
    episode: String,
}

impl AlertLinkQuery {
    pub fn link(&self) -> Option<AlertLink> {
        Some(AlertLink {
            org: OrgId(Uuid::parse_str(self.org.trim()).ok()?),
            channel: Uuid::parse_str(self.channel.trim()).ok()?,
            episode: self.episode.trim().parse().ok()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use axum::extract::Query;

    use super::*;

    #[test]
    fn a_minted_path_reads_back_as_the_same_link() {
        let link = AlertLink {
            org: OrgId(Uuid::now_v7()),
            channel: Uuid::now_v7(),
            episode: 3,
        };
        let uri: axum::http::Uri = link.path(Uuid::now_v7()).parse().unwrap();
        let Query(query) = Query::<AlertLinkQuery>::try_from_uri(&uri).unwrap();
        assert_eq!(query.link(), Some(link));
    }

    #[test]
    fn a_link_missing_its_org_channel_or_episode_is_no_link() {
        let q = |org: &str, channel: &str, episode: &str| AlertLinkQuery {
            org: org.into(),
            channel: channel.into(),
            episode: episode.into(),
        };
        let id = Uuid::now_v7().to_string();
        assert!(q(&id, &id, "3").link().is_some());
        assert!(q(&id, &id, "").link().is_none());
        assert!(q(&id, "", "3").link().is_none());
        assert!(q("acme", &id, "3").link().is_none());
    }
}
