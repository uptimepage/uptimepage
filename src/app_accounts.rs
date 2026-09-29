//! Naming who pressed an app's Acknowledge button, and offering an unnamed
//! presser a link to their account.

use chrono::{DateTime, Utc};

use crate::domain::{ExternalId, Linked, LinkedApp, OrgId};
use crate::security::sha256_hex;
use crate::security::token_hash::generate_raw_token;
use crate::storage::LinkedAppStore;

/// Who pressed, as far as `org` is concerned. A failed lookup names nobody:
/// taking the page matters more than naming who took it.
pub async fn identify(
    store: &dyn LinkedAppStore,
    org: OrgId,
    app: LinkedApp,
    sender: ExternalId,
) -> Linked {
    match store.resolve(org, app, sender).await {
        Ok(linked) => linked,
        Err(err) => {
            tracing::warn!(org_id = %org.0, app = app.as_db_str(), error = %err, "linked app lookup failed");
            Linked::Unknown
        }
    }
}

/// A link offer stored for an app account and ready to send.
#[derive(Debug, Clone)]
pub struct LinkOffer {
    /// Opens the offer; whoever signs in there gets the account.
    pub url: String,
    /// What the store holds, for taking back an offer that never arrived.
    pub code_hash: String,
}

/// Offer `account` to whoever opens the link, on the app's
/// [`OfferTerms`](crate::security::app_link::OfferTerms). `None` when there is
/// no public address to link to, while the account's last offer is still
/// cooling down, or when storing the offer failed.
pub async fn offer_link(
    store: &dyn LinkedAppStore,
    base_url: &str,
    app: LinkedApp,
    account: ExternalId,
    label: Option<&str>,
    now: DateTime<Utc>,
) -> Option<LinkOffer> {
    let base = base_url.trim_end_matches('/');
    if base.is_empty() {
        tracing::info!(
            app = app.as_db_str(),
            "link offer skipped: no public base URL to link to"
        );
        return None;
    }
    let code = generate_raw_token();
    let code_hash = sha256_hex(&code);
    match store.offer(app, account, label, &code_hash, now).await {
        Ok(true) => Some(LinkOffer {
            url: format!("{base}/link/{}?c={code}", app.as_db_str()),
            code_hash,
        }),
        Ok(false) => None,
        Err(err) => {
            tracing::warn!(error = %err, app = app.as_db_str(), "link offer failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::app_link::external_id;
    use crate::storage::InMemoryLinkedAppStore;

    #[tokio::test]
    async fn an_offer_link_opens_the_apps_own_page_and_needs_a_public_address() {
        let store = InMemoryLinkedAppStore::new();
        let presser = external_id("s3cret", "T0AB12CD3:U0AB12CD3");
        let now = Utc::now();
        let offer = |base: &'static str, app| offer_link(&store, base, app, presser, None, now);

        assert!(offer("", LinkedApp::Slack).await.is_none());
        let slack = offer("https://app.example.test/", LinkedApp::Slack)
            .await
            .expect("a Slack offer");
        let code = slack
            .url
            .strip_prefix("https://app.example.test/link/slack?c=")
            .expect("the Slack offer page");
        assert_eq!(slack.code_hash, sha256_hex(code));
        assert!(
            store
                .offered(LinkedApp::Slack, &slack.code_hash)
                .await
                .unwrap()
                .is_some()
        );

        assert!(
            offer("https://app.example.test", LinkedApp::Pushover)
                .await
                .is_some()
        );
        assert!(
            offer("https://app.example.test", LinkedApp::Pushover)
                .await
                .is_none(),
            "a Pushover account waits out its cooldown"
        );
    }
}
