//! Status page write bodies: create a page, retitle or publish it, curate the
//! monitors on it.
//!
//! No audit here — the wrapper in [`super::tools_write`] records the outcome.

use rmcp::RoleServer;
use rmcp::handler::server::wrapper::Json;
use rmcp::service::RequestContext;

use crate::auth::scope::Scope;
use crate::domain::{
    AccessError, AccessRule, AccessRuleKind, AccessSpec, AccessWrite, EmailAccessSpec,
    NewStatusPage, NewStatusPageComponent, OrgId, PageAccess, PageRef, PasswordAccessSpec,
    StatusPage, StatusPageComponentUpdate, StatusPageId, StatusPageUpdate, WriteSource,
    validate_slug,
};
use crate::public_status::curation::{clean_curation, clean_curation_patch, validate_name};
use crate::quotas::ratelimit::RateLimitCategory;
use crate::storage::status_pages::AddComponentOutcome;

use crate::error::AppError;
use crate::mcp::auth::McpAuth;
use crate::mcp::confirm::require_confirmation;
use crate::mcp::error::{McpToolError, codes, config_error};
use crate::mcp::schema::{
    AddComponentsArgs, ComponentOutcome, ComponentUpdated, ComponentsAdded, CreateStatusPageArgs,
    NewComponentArg, NewSectionArg, PageAccessArg, PageAccessRuleArg, PageEmailAccessArg,
    PagePasswordAccessArg, SectionOutcome, StatusPageWritten, UpdateComponentArgs,
    UpdateStatusPageArgs,
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

    async fn written(&self, page: &StatusPage) -> Result<Json<StatusPageWritten>, McpToolError> {
        Ok(Json(StatusPageWritten {
            slug: page.slug.clone(),
            name: sanitize_data(&page.name),
            public_url: self.page_public_url(&page.slug),
            enabled: page.enabled,
            access: self.page_access(page.org_id, page.id).await?,
        }))
    }

    /// Who may open a page, as the tools return it.
    pub(super) async fn page_access(
        &self,
        org: OrgId,
        page: StatusPageId,
    ) -> Result<Option<PageAccessArg>, McpToolError> {
        Ok(self
            .stored_access(org, page)
            .await?
            .as_ref()
            .map(access_arg))
    }

    async fn stored_access(
        &self,
        org: OrgId,
        page: StatusPageId,
    ) -> Result<Option<PageAccess>, McpToolError> {
        let pool = self.require_pool()?;
        crate::storage::page_access::settings::load(pool, PageRef { page, org })
            .await
            .map_err(|e| McpToolError::internal(format!("page access: {e}")))
    }

    /// The access write an argument asks for, validated against the plan.
    /// `stored` is the page's access now: a page already private stays
    /// editable while held for a plan that lost private pages, and its stored
    /// password is what turning the password on uses, since none is ever
    /// passed here.
    async fn access_write(
        &self,
        auth: &McpAuth,
        arg: Option<&PageAccessArg>,
        stored: Option<&PageAccess>,
    ) -> Result<Option<AccessWrite>, McpToolError> {
        let Some(arg) = arg else { return Ok(None) };
        let plan = self
            .state
            .quotas
            .limit_for_org(auth.org)
            .await
            .map_err(|e| McpToolError::internal(format!("plan: {e}")))?;
        let valid = access_spec(arg)?
            .validate(plan.private_pages_enabled, stored)
            .map_err(|e| match e {
                AccessError::PasswordRequired => McpToolError::invalid_argument(
                    "this page has no password yet: set the password in the app, the API or \
                     Terraform, then turn it on here",
                ),
                other => McpToolError::invalid_argument(other.to_string()),
            })?;
        Ok(Some(AccessWrite {
            access: valid.access,
            password_hash: None,
            actor: Some(auth.user_id),
        }))
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

        let access = self.access_write(auth, args.access.as_ref(), None).await?;
        let private = access.as_ref().is_some_and(|w| w.access.is_private());
        let visibility = match (enabled, private) {
            (_, true) => format!(
                "It will be private: {}.",
                who_can_open(&access.as_ref().expect("private").access)
            ),
            (true, false) => "It will be publicly readable as soon as it exists.".to_owned(),
            (false, false) => {
                "It starts unpublished, so nothing is public until you enable it.".to_owned()
            }
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
                    access,
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

        self.written(&page).await
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
        if name.is_none() && new_slug.is_none() && args.enabled.is_none() && args.access.is_none() {
            return Err(McpToolError::invalid_argument(
                "nothing to change; pass name, new_slug, enabled or access",
            ));
        }
        let stored = self.stored_access(auth.org, page.id).await?;
        let was_private = stored.as_ref().is_some_and(PageAccess::is_private);
        let password_was_on = stored
            .as_ref()
            .is_some_and(|a| a.is_private() && a.settings.password_enabled);
        let email_was_on = stored
            .as_ref()
            .is_some_and(|a| a.is_private() && a.settings.email_enabled);
        let access = self
            .access_write(auth, args.access.as_ref(), stored.as_ref())
            .await?;

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
        if let Some(w) = &access {
            effects.push(match (was_private, w.access.is_private()) {
                (_, true) => format!("make it private: {}", who_can_open(&w.access)),
                (true, false) => "make it public: anyone with the URL can read it".to_string(),
                (false, false) => "keep it public and store its access list".to_string(),
            });
            if password_was_on && w.access.is_private() && !w.access.settings.password_enabled {
                effects.push(
                    "turn the page password off, signing out everyone who came in with it"
                        .to_string(),
                );
            }
            if email_was_on && w.access.is_private() && !w.access.settings.email_enabled {
                effects.push(
                    "turn the email code off, signing out everyone who signed in by email and \
                     ending their subscriptions"
                        .to_string(),
                );
            }
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
                    public_locale: None,
                    access,
                },
                WriteSource::Api,
            )
            .await
            .map_err(config_error)?
            .ok_or_else(|| McpToolError::not_found("status page not found"))?;

        self.state.public_source.invalidate(page.id).await;
        self.state.page_gate.invalidate(page.id).await;
        if was_private && args.access.as_ref().is_some_and(|a| !a.protected) {
            crate::quotas::holds::release_after_delete(
                self.require_pool()?,
                &self.state.quotas,
                auth.org,
            )
            .await;
        }
        self.written(&updated).await
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

        if args.components.is_empty() && args.groups.is_empty() {
            return Err(McpToolError::invalid_argument(
                "pass at least one monitor or group to add",
            ));
        }
        let page = self.page_by_slug(auth.org, &args.slug).await?;
        let sections = self.prepare_sections(auth, &args.groups).await?;
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
        let mut lines = Vec::with_capacity(prepared.len() + sections.len());
        for c in &prepared {
            let name = self.monitor_label(auth.org, c.target_id).await;
            lines.push(match &c.public_name {
                Some(public) => format!("{name} — shown as \"{}\"", sanitize_prompt(public)),
                None => name,
            });
        }
        for (_, name, heading) in &sections {
            let shown = heading.as_deref().unwrap_or(name);
            lines.push(format!(
                "group \"{}\" as a section headed \"{}\": every member, now and as it changes",
                sanitize_prompt(name),
                sanitize_prompt(shown)
            ));
        }
        require_confirmation(
            ctx,
            auth,
            format!(
                "Add {} monitor(s) and {} group(s) to status page \"{}\"? {published}\n\n{}",
                prepared.len(),
                sections.len(),
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
                        && let Err(e) = self
                            .state
                            .publishing()
                            .ensure_detail_share(auth.org, page.id, target_id, auth.user_id)
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

        let pool = self.require_pool()?;
        let mut section_results = Vec::with_capacity(sections.len());
        for (group, _, heading) in sections {
            let outcome = crate::storage::page_sections::add(
                pool,
                auth.org,
                page.id.0,
                group,
                heading,
                max,
                Some(auth.user_id),
            )
            .await;
            section_results.push(match outcome {
                Ok((section, change)) => {
                    added += 1;
                    SectionOutcome {
                        group_id: group.to_string(),
                        outcome: "added".into(),
                        section_id: Some(section.to_string()),
                        left_off: u32::try_from(change.left_off).unwrap_or(u32::MAX),
                        error: None,
                    }
                }
                Err(e) => SectionOutcome {
                    group_id: group.to_string(),
                    outcome: match e {
                        AppError::Conflict {
                            code: crate::error::codes::SECTION_EXISTS,
                            ..
                        } => "already_on_page",
                        _ => "failed",
                    }
                    .into(),
                    section_id: None,
                    left_off: 0,
                    error: Some(e.to_string()),
                },
            });
        }

        if added > 0 {
            self.state.public_source.invalidate(page.id).await;
        }
        Ok(Json(ComponentsAdded {
            slug: page.slug,
            added,
            results,
            sections: section_results,
        }))
    }

    /// Each requested group, checked before the confirmation names it.
    async fn prepare_sections(
        &self,
        auth: &McpAuth,
        requested: &[NewSectionArg],
    ) -> Result<Vec<(uuid::Uuid, String, Option<String>)>, McpToolError> {
        let mut out = Vec::with_capacity(requested.len());
        for r in requested {
            let id = parse_uuid(&r.group_id, "group id")?;
            let heading = clean_curation(
                r.heading.clone(),
                "heading",
                crate::storage::page_sections::MAX_HEADING_LEN,
            )
            .map_err(config_error)?;
            let group = self
                .state
                .monitor_group_store
                .get(auth.org, id)
                .await
                .map_err(|e| McpToolError::internal(format!("monitor group: {e}")))?
                .ok_or_else(|| McpToolError::not_found(format!("group {id} not found")))?;
            out.push((id, group.name, heading));
        }
        Ok(out)
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
        if let Some(section) = args.section_id.as_deref() {
            return self
                .update_section_heading(ctx, auth, &page, section, args)
                .await;
        }
        let Some(monitor_id) = args.monitor_id.as_deref() else {
            return Err(McpToolError::invalid_argument(
                "pass monitor_id, or section_id with a heading",
            ));
        };
        let target_id = parse_uuid(monitor_id, "monitor id")?;
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
            monitor_id: Some(target_id.to_string()),
            section_id: None,
        }))
    }

    async fn update_section_heading(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        page: &crate::domain::StatusPage,
        section: &str,
        args: &UpdateComponentArgs,
    ) -> Result<Json<ComponentUpdated>, McpToolError> {
        let section = parse_uuid(section, "section id")?;
        let Some(heading) = args.heading.clone() else {
            return Err(McpToolError::invalid_argument(
                "nothing to change; pass heading",
            ));
        };
        let heading = clean_curation(
            Some(heading),
            "heading",
            crate::storage::page_sections::MAX_HEADING_LEN,
        )
        .map_err(config_error)?;
        require_confirmation(
            ctx,
            auth,
            format!(
                "Change a section heading on status page \"{}\" to \"{}\"?",
                sanitize_prompt(&page.name),
                heading
                    .as_deref()
                    .map(sanitize_prompt)
                    .unwrap_or_else(|| "the group's name".into())
            ),
        )
        .await?;
        let pool = self.require_pool()?;
        crate::storage::page_sections::update(
            pool,
            auth.org,
            page.id.0,
            section,
            heading,
            Some(auth.user_id),
        )
        .await
        .map_err(|e| match e {
            AppError::NotFound {
                code: crate::error::codes::SECTION_NOT_FOUND,
                ..
            } => McpToolError::not_found("section is not on this page"),
            other => config_error(other),
        })?;
        self.state.public_source.invalidate(page.id).await;
        Ok(Json(ComponentUpdated {
            slug: page.slug.clone(),
            monitor_id: None,
            section_id: Some(section.to_string()),
        }))
    }
}

fn access_arg(a: &PageAccess) -> PageAccessArg {
    let spec = AccessSpec::from(a);
    PageAccessArg {
        protected: spec.protected,
        session_days: spec.session_days,
        email: PageEmailAccessArg {
            enabled: spec.email.enabled,
            passkeys_enabled: spec.email.passkeys_enabled,
            recheck_days: spec.email.recheck_days,
        },
        password: PagePasswordAccessArg {
            enabled: spec.password.enabled,
            value: None,
        },
        rules: spec
            .rules
            .iter()
            .map(|r| PageAccessRuleArg {
                kind: r.kind.as_db_str().to_owned(),
                value: sanitize_data(&r.value),
            })
            .collect(),
    }
}

fn access_spec(arg: &PageAccessArg) -> Result<AccessSpec, McpToolError> {
    if arg.password.value.is_some() {
        return Err(McpToolError::invalid_argument(
            "a page password is never passed as a tool argument: set it in the app, the API or \
             Terraform, then turn `password.enabled` on here",
        ));
    }
    let rules = arg
        .rules
        .iter()
        .map(|r| {
            let kind = AccessRuleKind::from_db_str(r.kind.trim()).ok_or_else(|| {
                McpToolError::invalid_argument(format!(
                    "rule kind {:?} is neither \"domain\" nor \"address\"",
                    r.kind
                ))
            })?;
            Ok(AccessRule {
                kind,
                value: r.value.clone(),
            })
        })
        .collect::<Result<Vec<_>, McpToolError>>()?;
    Ok(AccessSpec {
        protected: arg.protected,
        session_days: arg.session_days,
        email: EmailAccessSpec {
            enabled: arg.email.enabled,
            passkeys_enabled: arg.email.passkeys_enabled,
            recheck_days: arg.email.recheck_days,
        },
        password: PasswordAccessSpec {
            enabled: arg.password.enabled,
            value: None,
        },
        rules,
    })
}

/// The ways in as the confirmation reads them, the list capped so a long one
/// stays a prompt.
fn who_can_open(access: &PageAccess) -> String {
    const SHOWN: usize = 10;
    let mut ways = Vec::new();
    if access.settings.email_enabled {
        let mut names: Vec<String> = access
            .rules
            .iter()
            .take(SHOWN)
            .map(|r| match r.kind {
                AccessRuleKind::Domain => format!("anyone at {}", sanitize_prompt(&r.value)),
                AccessRuleKind::Address => sanitize_prompt(&r.value),
            })
            .collect();
        if access.rules.len() > SHOWN {
            names.push(format!("{} more", access.rules.len() - SHOWN));
        }
        ways.push(format!("{}, after signing in by email", names.join(", ")));
    }
    if access.settings.password_enabled {
        ways.push("anyone with the page password".to_owned());
    }
    format!("only {} can open it", ways.join("; or "))
}
