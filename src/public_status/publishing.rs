//! Everything that changes what a public page shows and must then bust the
//! cached copy, behind one view so the REST handlers and the MCP tools cannot
//! disagree on which pages an incident or a monitor reaches.

use uuid::Uuid;

use crate::domain::{NewMonitorShare, OpsIncident, OrgId, StatusPageId, UserId};
use crate::error::codes;
use crate::error::{AppError, Result};
use crate::public_status::PublicSource;
use crate::storage::{
    Actor, CreateShareOutcome, IncidentOpsStore, MonitorShareStore, StatusPageStore,
};

/// Borrowed view over the stores a page-visible change touches.
pub struct Publishing<'a> {
    pub pages: &'a dyn StatusPageStore,
    pub shares: &'a dyn MonitorShareStore,
    pub incidents: &'a dyn IncidentOpsStore,
    pub source: &'a dyn PublicSource,
}

impl Publishing<'_> {
    /// Bust the cached pages an incident shows on: those carrying its monitor,
    /// or the ones it was published to when it has none.
    pub async fn invalidate_incident(&self, org: OrgId, incident: &OpsIncident) {
        if incident.target_id.is_some() {
            return self
                .invalidate_targets(org, incident.target_id.as_slice())
                .await;
        }
        match self.incidents.status_pages(org, incident.id).await {
            Ok(pages) => self.invalidate_pages(&pages).await,
            Err(e) => tracing::warn!(error = %e, "could not resolve pages for cache invalidation"),
        }
    }

    /// Bust cached public status pages that surface any of `ids`.
    pub async fn invalidate_targets(&self, org: OrgId, ids: &[Uuid]) {
        if ids.is_empty() {
            return;
        }
        match self.pages.pages_for_targets(org, ids).await {
            Ok(pages) => {
                for page in pages {
                    self.source.invalidate(page).await;
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not resolve pages for cache invalidation"),
        }
    }

    async fn invalidate_pages(&self, pages: &[Uuid]) {
        for page in pages {
            self.source.invalidate(StatusPageId(*page)).await;
        }
    }

    /// Publish, then bust every page the incident shows on, and the pages a new
    /// `status_page_ids` list just took it off.
    pub async fn publish_incident(
        &self,
        org: OrgId,
        id: Uuid,
        public_title: Option<String>,
        public_description: Option<String>,
        status_page_ids: Option<Vec<Uuid>>,
        actor: Actor,
    ) -> Result<Option<OpsIncident>> {
        let before = match status_page_ids {
            Some(_) => self.incidents.status_pages(org, id).await?,
            None => Vec::new(),
        };
        let Some(incident) = self
            .incidents
            .publish(
                org,
                id,
                public_title,
                public_description,
                status_page_ids,
                actor,
            )
            .await?
        else {
            return Ok(None);
        };
        self.invalidate_incident(org, &incident).await;
        self.invalidate_pages(&before).await;
        Ok(Some(incident))
    }

    /// Mints only when the component has no live share, so an untick then re-tick
    /// returns the same URL. Plan share caps do not apply: the page already
    /// publishes this monitor.
    pub async fn ensure_detail_share(
        &self,
        org: OrgId,
        page: StatusPageId,
        target_id: Uuid,
        user: UserId,
    ) -> Result<()> {
        let existing = self
            .pages
            .list_components(org, page)
            .await?
            .into_iter()
            .find(|c| c.target_id == target_id)
            .and_then(|c| c.share_id);
        if let Some(share) = existing {
            let now = chrono::Utc::now();
            let live = self
                .shares
                .list_for_target(org, target_id)
                .await?
                .iter()
                .any(|s| s.id == share && s.expires_at.is_none_or(|e| e > now));
            if live {
                return Ok(());
            }
        }
        let outcome = self
            .shares
            .create(
                org,
                target_id,
                NewMonitorShare::default(),
                Some(user),
                None,
                None,
            )
            .await?;
        let CreateShareOutcome::Created(created) = outcome else {
            return Err(component_not_found());
        };
        if !self
            .pages
            .attach_share(org, page, target_id, existing, created.share.id)
            .await?
        {
            // Lost the swap or the component vanished; either way another mint owns
            // the slot now, so don't strand this one as a live public URL.
            self.shares
                .revoke(org, target_id, created.share.id, Some(user))
                .await?;
            return Err(component_not_found());
        }
        Ok(())
    }
}

fn component_not_found() -> AppError {
    AppError::not_found(codes::STATUS_PAGE_NOT_FOUND, "component not on this page")
}
