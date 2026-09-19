//! Status page write bodies: create a page, retitle or publish it, curate the
//! monitors on it.
//!
//! No audit here — the wrapper in [`super::tools_write`] records the outcome.

use rmcp::RoleServer;
use rmcp::handler::server::wrapper::Json;
use rmcp::service::RequestContext;

use crate::api::handlers::status_page::{
    clean_curation, clean_curation_patch, ensure_detail_share, validate_name,
};
use crate::auth::scope::Scope;
use crate::domain::{
    NewStatusPage, NewStatusPageComponent, OrgId, StatusPage, StatusPageComponentUpdate,
    StatusPageUpdate, WriteSource, validate_slug,
};
use crate::quotas::ratelimit::RateLimitCategory;
use crate::storage::status_pages::AddComponentOutcome;

use crate::mcp::auth::McpAuth;
use crate::mcp::confirm::require_confirmation;
use crate::mcp::error::{McpToolError, codes, config_error};
use crate::mcp::schema::{
    AddComponentsArgs, ComponentOutcome, ComponentUpdated, ComponentsAdded, CreateStatusPageArgs,
    NewComponentArg, StatusPageWritten, UpdateComponentArgs, UpdateStatusPageArgs,
};

use super::McpServer;
use super::args::parse_uuid;
use super::text::{sanitize_data, sanitize_prompt};

/// Curation field bounds, matching the DB CHECKs the REST path validates on.
const MAX_PUBLIC_NAME: usize = 80;
const MAX_PUBLIC_DESCRIPTION: usize = 200;
const MAX_PUBLIC_GROUP: usize = 50;

impl McpServer {
    /// The public brand surface is an owner-level asset, the same bar the REST
    /// handlers hold it to; a scope alone would let a member re-slug the org's
    /// public URL over a door that refuses them elsewhere.
    async fn require_page_owner(&self, auth: &McpAuth) -> Result<(), McpToolError> {
        auth.require(Scope::StatusPageWrite)?;
        let pool = self.require_pool()?;
        match crate::storage::orgs::is_owner(pool, auth.user_id, auth.org).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(McpToolError::new(
                codes::INSUFFICIENT_SCOPE,
                "status pages are owner-managed; ask an owner of this organization to make \
                 this change",
                false,
            )),
            Err(e) => Err(McpToolError::internal(format!("owner check: {e}"))),
        }
    }

    /// Pages are addressed by slug everywhere on this surface, since that is
    /// what `list_status_pages` hands back and what the public URL shows.
    /// Stored slugs are always lowercase, so the lookup matches on that.
    async fn page_by_slug(&self, org: OrgId, slug: &str) -> Result<StatusPage, McpToolError> {
        let slug = slug.trim().to_ascii_lowercase();
        self.state
            .status_page_store
            .list(org)
            .await
            .map_err(|e| McpToolError::internal(format!("status page lookup: {e}")))?
            .into_iter()
            .find(|p| p.slug == slug)
            .ok_or_else(|| McpToolError::not_found("status page not found"))
    }

    /// A deployment with no public surface mounted has no address to show, and
    /// an empty one reads as a broken link rather than an absent feature.
    fn page_address(&self, slug: &str) -> String {
        let url = self.page_public_url(slug);
        if url.is_empty() {
            format!("slug `{slug}` (this deployment publishes no public status URL)")
        } else {
            url
        }
    }

    /// How a confirmation names a monitor: its operator-side name, falling back
    /// to the id when the monitor cannot be read, so a prompt is never unnamed.
    async fn monitor_label(&self, org: OrgId, target_id: uuid::Uuid) -> String {
        match self.state.target_store.get(org, target_id).await {
            Ok(Some(t)) => sanitize_prompt(&t.name),
            _ => target_id.to_string(),
        }
    }

    fn written(&self, page: &StatusPage) -> Json<StatusPageWritten> {
        Json(StatusPageWritten {
            slug: page.slug.clone(),
            name: sanitize_data(&page.name),
            public_url: self.page_public_url(&page.slug),
            enabled: page.enabled,
        })
    }

    /// `create_status_page` body.
    pub(super) async fn create_status_page_inner(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        args: &CreateStatusPageArgs,
    ) -> Result<Json<StatusPageWritten>, McpToolError> {
        self.require_page_owner(auth).await?;
        self.enforce_rate_limit(auth.org, RateLimitCategory::ApiWrites)
            .await?;

        let slug = args.slug.trim().to_ascii_lowercase();
        validate_slug(&slug).map_err(|e| McpToolError::invalid_argument(e.to_string()))?;
        let name = validate_name(&args.name).map_err(config_error)?;
        let enabled = args.enabled.unwrap_or(false);

        self.state
            .quotas
            .check_can_create_status_page(auth.org, None)
            .await
            .map_err(config_error)?;
        let plan = self
            .state
            .quotas
            .limit_for_org(auth.org)
            .await
            .map_err(|e| McpToolError::internal(format!("plan: {e}")))?;
        let max = i64::from(plan.max_status_pages);

        let visibility = if enabled {
            "It will be publicly readable as soon as it exists."
        } else {
            "It starts unpublished, so nothing is public until you enable it."
        };
        require_confirmation(
            ctx,
            auth,
            format!(
                "Create status page \"{}\" at {}? {visibility}",
                sanitize_prompt(&name),
                self.page_address(&slug),
            ),
        )
        .await?;

        let page = self
            .state
            .status_page_store
            .create(
                auth.org,
                NewStatusPage {
                    slug,
                    name,
                    enabled,
                },
                WriteSource::Api,
                max,
                Some(auth.user_id),
            )
            .await
            .map_err(config_error)?
            .ok_or_else(|| {
                McpToolError::invalid_argument(format!(
                    "this plan allows {max} status pages and they are all in use"
                ))
            })?;

        Ok(self.written(&page))
    }

    /// `update_status_page` body.
    pub(super) async fn update_status_page_inner(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        args: &UpdateStatusPageArgs,
    ) -> Result<Json<StatusPageWritten>, McpToolError> {
        self.require_page_owner(auth).await?;
        self.enforce_rate_limit(auth.org, RateLimitCategory::ApiWrites)
            .await?;

        let page = self.page_by_slug(auth.org, &args.slug).await?;
        let name = args
            .name
            .as_deref()
            .map(|n| validate_name(n).map_err(config_error))
            .transpose()?;
        let new_slug = match args.new_slug.as_deref() {
            Some(s) => {
                let s = s.trim().to_ascii_lowercase();
                validate_slug(&s).map_err(|e| McpToolError::invalid_argument(e.to_string()))?;
                Some(s)
            }
            None => None,
        };
        if name.is_none() && new_slug.is_none() && args.enabled.is_none() {
            return Err(McpToolError::invalid_argument(
                "nothing to change; pass name, new_slug or enabled",
            ));
        }

        let mut effects = Vec::new();
        if let Some(n) = &name {
            effects.push(format!("rename it to \"{}\"", sanitize_prompt(n)));
        }
        // The old URL stops resolving and every existing link to it breaks.
        if let Some(s) = &new_slug {
            effects.push(format!(
                "move it to {}, breaking existing links",
                self.page_public_url(s)
            ));
        }
        if let Some(e) = args.enabled {
            effects.push(if e {
                "publish it".to_string()
            } else {
                "unpublish it, hiding it from the public".to_string()
            });
        }
        require_confirmation(
            ctx,
            auth,
            format!(
                "Update status page \"{}\": {}?",
                sanitize_prompt(&page.name),
                effects.join(", ")
            ),
        )
        .await?;

        let updated = self
            .state
            .status_page_store
            .update(
                auth.org,
                page.id,
                StatusPageUpdate {
                    name,
                    slug: new_slug,
                    enabled: args.enabled,
                    branding: None,
                },
                WriteSource::Api,
            )
            .await
            .map_err(config_error)?
            .ok_or_else(|| McpToolError::not_found("status page not found"))?;

        self.state.public_source.invalidate(page.id).await;
        Ok(self.written(&updated))
    }

    /// `add_status_page_components` body. One confirmation covers the batch,
    /// then each monitor is applied on its own so one rejection does not discard
    /// the rest.
    pub(super) async fn add_status_page_components_inner(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        args: &AddComponentsArgs,
    ) -> Result<Json<ComponentsAdded>, McpToolError> {
        self.require_page_owner(auth).await?;
        self.enforce_rate_limit(auth.org, RateLimitCategory::ApiWrites)
            .await?;

        if args.components.is_empty() {
            return Err(McpToolError::invalid_argument(
                "pass at least one monitor to add",
            ));
        }
        let page = self.page_by_slug(auth.org, &args.slug).await?;
        // Positions continue from what the page already shows; restarting at 0
        // would interleave this batch with the components already on it.
        let existing = self
            .state
            .status_page_store
            .list_components(auth.org, page.id)
            .await
            .map_err(|e| McpToolError::internal(format!("page components: {e}")))?;
        let next_sort = existing
            .iter()
            .map(|c| c.sort_order)
            .max()
            .map_or(0, |m| m + 1);
        let prepared = self.prepare_components(&args.components, next_sort)?;

        let published = if page.enabled {
            "The page is published, so they appear publicly at once."
        } else {
            "The page is unpublished, so nothing is public yet."
        };
        let mut lines = Vec::with_capacity(prepared.len());
        for c in &prepared {
            let name = self.monitor_label(auth.org, c.target_id).await;
            lines.push(match &c.public_name {
                Some(public) => format!("{name} — shown as \"{}\"", sanitize_prompt(public)),
                None => name,
            });
        }
        require_confirmation(
            ctx,
            auth,
            format!(
                "Add {} monitor(s) to status page \"{}\"? {published}\n\n{}",
                prepared.len(),
                sanitize_prompt(&page.name),
                lines.join("\n"),
            ),
        )
        .await?;

        let plan = self
            .state
            .quotas
            .limit_for_org(auth.org)
            .await
            .map_err(|e| McpToolError::internal(format!("plan: {e}")))?;
        let max = i64::from(plan.max_public_components);

        let mut results = Vec::with_capacity(prepared.len());
        let mut added = 0usize;
        for new in prepared {
            let target_id = new.target_id;
            let wants_detail_link = new.detail_link_enabled;
            let outcome = self
                .state
                .status_page_store
                .add_component(auth.org, page.id, new, max, Some(auth.user_id))
                .await;
            let (label, error) = match outcome {
                Ok(AddComponentOutcome::Added) => {
                    added += 1;
                    if wants_detail_link
                        && let Err(e) = ensure_detail_share(
                            &self.state,
                            auth.org,
                            page.id,
                            target_id,
                            auth.user_id,
                        )
                        .await
                    {
                        ("added", Some(format!("detail link not published: {e}")))
                    } else {
                        ("added", None)
                    }
                }
                Ok(AddComponentOutcome::AlreadyOnPage) => ("already_on_page", None),
                Ok(AddComponentOutcome::OverCap { used }) => (
                    "failed",
                    Some(format!(
                        "this plan publishes {max} components and {used} are in use"
                    )),
                ),
                Err(e) => ("failed", Some(e.to_string())),
            };
            results.push(ComponentOutcome {
                monitor_id: target_id.to_string(),
                outcome: label.to_string(),
                error,
            });
        }

        if added > 0 {
            self.state.public_source.invalidate(page.id).await;
        }
        Ok(Json(ComponentsAdded {
            slug: page.slug,
            added,
            results,
        }))
    }

    /// All validated up front, so a bad field in the tenth entry does not leave
    /// nine already applied.
    fn prepare_components(
        &self,
        requested: &[NewComponentArg],
        next_sort: i32,
    ) -> Result<Vec<NewStatusPageComponent>, McpToolError> {
        requested
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let target_id = parse_uuid(&c.monitor_id, "monitor id")?;
                Ok(NewStatusPageComponent {
                    target_id,
                    public_name: clean_curation(
                        c.public_name.clone(),
                        "public_name",
                        MAX_PUBLIC_NAME,
                    )
                    .map_err(config_error)?,
                    public_description: clean_curation(
                        c.public_description.clone(),
                        "public_description",
                        MAX_PUBLIC_DESCRIPTION,
                    )
                    .map_err(config_error)?,
                    public_group: clean_curation(
                        c.public_group.clone(),
                        "public_group",
                        MAX_PUBLIC_GROUP,
                    )
                    .map_err(config_error)?,
                    sort_order: c
                        .sort_order
                        .unwrap_or_else(|| next_sort.saturating_add(i32::try_from(i).unwrap_or(0))),
                    detail_link_enabled: c.detail_link_enabled.unwrap_or(false),
                })
            })
            .collect()
    }

    /// `update_status_page_component` body.
    pub(super) async fn update_status_page_component_inner(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        args: &UpdateComponentArgs,
    ) -> Result<Json<ComponentUpdated>, McpToolError> {
        self.require_page_owner(auth).await?;
        self.enforce_rate_limit(auth.org, RateLimitCategory::ApiWrites)
            .await?;

        let page = self.page_by_slug(auth.org, &args.slug).await?;
        let target_id = parse_uuid(&args.monitor_id, "monitor id")?;
        if args.public_name.is_none()
            && args.public_description.is_none()
            && args.public_group.is_none()
            && args.sort_order.is_none()
        {
            return Err(McpToolError::invalid_argument(
                "nothing to change; pass public_name, public_description, public_group or sort_order",
            ));
        }

        // A field the caller sent is a change; one it omitted is not. An
        // explicit blank clears the override, so both layers must survive.
        let patch = |v: &Option<String>| v.as_ref().map(|s| Some(s.clone()));
        let upd = StatusPageComponentUpdate {
            public_name: clean_curation_patch(
                patch(&args.public_name),
                "public_name",
                MAX_PUBLIC_NAME,
            )
            .map_err(config_error)?,
            public_description: clean_curation_patch(
                patch(&args.public_description),
                "public_description",
                MAX_PUBLIC_DESCRIPTION,
            )
            .map_err(config_error)?,
            public_group: clean_curation_patch(
                patch(&args.public_group),
                "public_group",
                MAX_PUBLIC_GROUP,
            )
            .map_err(config_error)?,
            sort_order: args.sort_order,
            ..Default::default()
        };

        require_confirmation(
            ctx,
            auth,
            format!(
                "Change how {} is presented on status page \"{}\"?",
                self.monitor_label(auth.org, target_id).await,
                sanitize_prompt(&page.name)
            ),
        )
        .await?;

        let changed = self
            .state
            .status_page_store
            .update_component(auth.org, page.id, target_id, upd)
            .await
            .map_err(config_error)?;
        if !changed {
            return Err(McpToolError::not_found("monitor is not on this page"));
        }

        self.state.public_source.invalidate(page.id).await;
        Ok(Json(ComponentUpdated {
            slug: page.slug,
            monitor_id: target_id.to_string(),
        }))
    }
}
