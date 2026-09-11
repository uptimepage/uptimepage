use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, Method, header};
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{get, post};
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowOrigin, CorsLayer};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::api::handlers;
use crate::api::{ApiDoc, idempotency, middleware as api_middleware};
use crate::app::AppState;
use crate::config::CorsConfig;
use crate::quotas::rate_limit_middleware;

const SINGLE_BODY_LIMIT: usize = 64 * 1024;
const BULK_BODY_LIMIT: usize = 8 * 1024 * 1024;

pub fn build_router(state: AppState, shutdown: CancellationToken) -> Router {
    // Idle-entry janitor for the per-org/user limiter map. Co-located with
    // the limiter it sweeps and bound to the same shutdown token, so a
    // refactor can't drop the sweep and leak the map.
    let j = &state.cfg.rate_limits.janitor;
    state.rate_limits.clone().spawn_janitor(
        Duration::from_secs(j.cleanup_interval_hours.saturating_mul(3600)),
        Duration::from_secs(j.idle_threshold_hours.saturating_mul(3600)),
        shutdown.clone(),
    );

    let bulk = Router::new()
        .route("/targets/bulk", post(handlers::targets::bulk_create))
        .route("/targets/bulk-action", post(handlers::targets::bulk_action))
        // `.merge`d below *after* v1's auth + rate-limit layers, so — like
        // `logo` — these routes must carry their own copies or they ship
        // unauthenticated and un-rate-limited (a bulk endpoint is the worst
        // place to lose either). Same inner→outer order as the main stack:
        // rate-limit added first (innermost) so auth runs before it and the
        // limiter keys on the resolved org/user. Bulk keeps the large body
        // limit; it must not inherit v1's 64 KiB single-item limit.
        .layer(from_fn_with_state(state.clone(), rate_limit_middleware))
        .layer(from_fn_with_state(
            state.clone(),
            crate::web::auth::api_token::middleware,
        ))
        .layer(from_fn_with_state(
            state.idempotency.clone(),
            idempotency::middleware,
        ))
        .layer(DefaultBodyLimit::max(BULK_BODY_LIMIT));

    // Logo upload needs more than the 64 KiB JSON limit but far less than the
    // bulk ceiling. Generous headroom over `max_logo_size_bytes` (not just
    // multipart framing) so an a-bit-too-large logo still reaches the handler
    // and gets a clean `413 LOGO_TOO_LARGE` instead of the body layer aborting
    // mid-parse and surfacing as an opaque `LOGO_MISSING`. Like `bulk`, `.merge`
    // lands these routes outside the main v1 stack, so they must carry their own
    // auth AND rate-limit copies — the CPU-heavy image re-encode is exactly what
    // the limiter exists to cap. Same inner→outer order: rate-limit innermost so
    // auth resolves the org/user the limiter keys on.
    let logo_body_limit = state.cfg.public_status.max_logo_size_bytes as usize + 1024 * 1024;
    let logo = Router::new()
        .route(
            "/status-pages/{id}/logo",
            post(handlers::status_page::upload_logo).delete(handlers::status_page::delete_logo),
        )
        .layer(from_fn_with_state(state.clone(), rate_limit_middleware))
        .layer(from_fn_with_state(
            state.clone(),
            crate::web::auth::api_token::middleware,
        ))
        .layer(DefaultBodyLimit::max(logo_body_limit));

    let mut v1 = Router::new()
        .route(
            "/targets",
            get(handlers::targets::list).post(handlers::targets::create),
        )
        .route(
            "/targets/{id}",
            get(handlers::targets::get)
                .patch(handlers::targets::update)
                .delete(handlers::targets::delete),
        )
        .route("/targets/test", post(handlers::targets::test_check))
        .route(
            "/targets/{id}/check-now",
            post(handlers::targets::check_now),
        )
        .route(
            "/targets/{id}/results",
            get(handlers::results::list_results),
        )
        .route("/targets/{id}/latency", get(handlers::results::latency))
        .route(
            "/targets/{id}/latency/by-region",
            get(handlers::results::latency_by_region),
        )
        .route(
            "/targets/{id}/flow-steps",
            get(handlers::results::flow_steps),
        )
        .route("/targets/{id}/uptime", get(handlers::results::uptime))
        .route(
            "/targets/{id}/regions",
            get(handlers::targets::get_target_regions).put(handlers::targets::set_target_regions),
        )
        .route(
            "/targets/{id}/heartbeat",
            get(handlers::targets::get_heartbeat),
        )
        .route(
            "/targets/{id}/heartbeat/rotate",
            axum::routing::post(handlers::targets::rotate_heartbeat),
        )
        .route(
            "/targets/{id}/heartbeat/previous",
            axum::routing::delete(handlers::targets::revoke_heartbeat_previous),
        )
        .route("/regions", get(handlers::targets::list_regions))
        .route(
            "/targets/{id}/incidents",
            get(handlers::results::list_incidents),
        )
        .route(
            "/targets/{id}/shares",
            get(handlers::shares::list_shares).post(handlers::shares::create_share),
        )
        .route(
            "/targets/{id}/shares/{share_id}",
            axum::routing::delete(handlers::shares::revoke_share),
        )
        .route("/tags", get(handlers::tags::list_tags))
        .route(
            "/dashboard/summary",
            get(handlers::dashboard::dashboard_summary),
        )
        .route(
            "/maintenance",
            get(handlers::maintenance::list_maintenance)
                .post(handlers::maintenance::create_maintenance),
        )
        .route(
            "/maintenance/{id}",
            get(handlers::maintenance::get_maintenance)
                .patch(handlers::maintenance::update_maintenance)
                .delete(handlers::maintenance::delete_maintenance),
        )
        .route(
            "/variables",
            get(handlers::variables::list).post(handlers::variables::create),
        )
        .route(
            "/variables/{id}",
            get(handlers::variables::get)
                .patch(handlers::variables::update)
                .delete(handlers::variables::delete),
        )
        .route(
            "/notification-channels",
            get(handlers::notification_channels::list)
                .post(handlers::notification_channels::create),
        )
        .route(
            "/notification-channels/{id}",
            get(handlers::notification_channels::get)
                .patch(handlers::notification_channels::update)
                .delete(handlers::notification_channels::delete),
        )
        .route(
            "/notification-channels/test",
            post(handlers::notification_channels::test_config),
        )
        .route(
            "/notification-channels/{id}/test",
            post(handlers::notification_channels::test_send),
        )
        .route(
            "/notification-channels/{id}/resend-verification",
            post(handlers::notification_channels::resend_verification),
        )
        // Mounted unconditionally: conditional mounting would let the path
        // fall into `/notification-channels/{id}` (405/400 noise) on
        // bot-less deployments. The handlers 404 when no bot is configured.
        .route(
            "/notification-channels/telegram-link",
            post(handlers::notification_channels::telegram_link_mint),
        )
        .route(
            "/notification-channels/telegram-link/{id}",
            get(handlers::notification_channels::telegram_link_status),
        )
        .route(
            "/notification-channels/whatsapp-link",
            post(handlers::notification_channels::whatsapp_link_mint),
        )
        .route(
            "/notification-channels/whatsapp-link/{id}",
            get(handlers::notification_channels::whatsapp_link_status),
        )
        .route(
            "/notification-channels/delegate",
            get(handlers::notification_channels::delegate_link_list)
                .post(handlers::notification_channels::delegate_link_mint),
        )
        .route(
            "/notification-channels/delegate/{id}",
            axum::routing::delete(handlers::notification_channels::delegate_link_revoke),
        )
        .route(
            "/incidents",
            get(handlers::incidents::list_incidents).post(handlers::incidents::declare_incident),
        )
        .route(
            "/incidents/{id}",
            axum::routing::patch(handlers::incidents::update_incident_narration)
                .get(handlers::incidents::get_incident),
        )
        .route(
            "/incidents/{id}/notifications",
            get(handlers::incidents::incident_notifications),
        )
        .route(
            "/incidents/{id}/updates",
            post(handlers::incidents::post_incident_update),
        )
        .route(
            "/incidents/{id}/acknowledge",
            post(handlers::incidents::acknowledge_incident),
        )
        .route(
            "/incidents/{id}/resolve",
            post(handlers::incidents::resolve_incident),
        )
        .route(
            "/incidents/{id}/reopen",
            post(handlers::incidents::reopen_incident),
        )
        .route(
            "/incidents/{id}/assign",
            post(handlers::incidents::assign_incident),
        )
        .route(
            "/incidents/{id}/notes",
            post(handlers::incidents::add_incident_note),
        )
        .route(
            "/incidents/{id}/publish",
            post(handlers::incidents::publish_incident),
        )
        .route(
            "/incidents/{id}/unpublish",
            post(handlers::incidents::unpublish_incident),
        )
        .route(
            "/incidents/metrics",
            get(handlers::incidents::incident_metrics),
        )
        .route(
            "/incidents/{id}/postmortem",
            get(handlers::incidents::get_postmortem).put(handlers::incidents::put_postmortem),
        )
        .route(
            "/incidents/{id}/postmortem/publish",
            post(handlers::incidents::publish_postmortem),
        )
        .route(
            "/incidents/{id}/postmortem/unpublish",
            post(handlers::incidents::unpublish_postmortem),
        )
        .route(
            "/escalation-policies",
            get(handlers::escalation_policies::list).post(handlers::escalation_policies::create),
        )
        .route(
            "/escalation-policies/default",
            get(handlers::escalation_policies::get_org_default)
                .put(handlers::escalation_policies::set_org_default),
        )
        .route(
            "/escalation-policies/{id}",
            get(handlers::escalation_policies::get)
                .patch(handlers::escalation_policies::replace)
                .delete(handlers::escalation_policies::delete),
        )
        .route(
            "/targets/{id}/escalation-policy",
            get(handlers::escalation_policies::get_target_policy)
                .put(handlers::escalation_policies::set_target_policy),
        )
        .route(
            "/on-call/schedules",
            get(handlers::on_call::list).post(handlers::on_call::create),
        )
        .route("/on-call/who", get(handlers::on_call::who))
        .route(
            "/on-call/my-contacts",
            get(handlers::on_call::get_my_contacts).put(handlers::on_call::set_my_contacts),
        )
        .route(
            "/on-call/schedules/{id}",
            get(handlers::on_call::get)
                .patch(handlers::on_call::replace)
                .delete(handlers::on_call::delete),
        )
        .route(
            "/on-call/schedules/{id}/overrides",
            post(handlers::on_call::add_override),
        )
        .route(
            "/on-call/schedules/{id}/overrides/{override_id}",
            axum::routing::delete(handlers::on_call::delete_override),
        )
        .route(
            "/orgs",
            get(handlers::orgs::list_my_orgs).post(handlers::orgs::create_org),
        )
        .route("/orgs/check-slug", get(handlers::orgs::check_slug))
        .route(
            "/orgs/{id}",
            get(handlers::orgs::get_org)
                .patch(handlers::orgs::update_org)
                .delete(handlers::orgs::delete_org),
        )
        .route("/orgs/{id}/restore", post(handlers::orgs::restore_org))
        .route(
            "/status-pages",
            get(handlers::status_page::list_pages).post(handlers::status_page::create_page),
        )
        .route(
            "/status-pages/{id}",
            get(handlers::status_page::get_page)
                .patch(handlers::status_page::update_page)
                .delete(handlers::status_page::delete_page),
        )
        .route(
            "/status-pages/{id}/components",
            get(handlers::status_page::list_components).post(handlers::status_page::add_component),
        )
        .route(
            "/status-pages/{id}/components/reorder",
            post(handlers::status_page::reorder_components),
        )
        .route(
            "/status-pages/{id}/components/{target_id}",
            axum::routing::patch(handlers::status_page::update_component)
                .delete(handlers::status_page::remove_component),
        )
        .route(
            "/status-pages/{id}/subscribers/{subscriber_id}",
            axum::routing::delete(handlers::status_page::remove_subscriber),
        )
        .route("/orgs/{id}/usage", get(handlers::usage::get_org_usage))
        .route(
            "/account/holds",
            get(handlers::holds::list_holds).put(handlers::holds::set_holds),
        )
        .route("/orgs/{id}/members", get(handlers::orgs::list_org_members))
        .route(
            "/orgs/{id}/members/{user_id}",
            axum::routing::delete(handlers::orgs::remove_org_member)
                .patch(handlers::orgs::update_org_member_role),
        )
        .route(
            "/me",
            get(handlers::me::me).delete(handlers::account::delete_account),
        )
        // Not `BrowserUser`-gated like its neighbours: the caller's account is
        // soft-deleted, which every other extractor reads as signed out.
        .route("/me/restore", post(handlers::account::restore_account))
        .route("/me/data-export", get(handlers::account::data_export))
        .route(
            "/me/sign-in-methods/{provider}",
            axum::routing::delete(handlers::identities::unlink),
        )
        .route(
            "/me/passkeys/{id}",
            axum::routing::delete(handlers::passkeys::remove),
        )
        .route("/me/usage", get(handlers::usage::get_me_usage))
        .route(
            "/me/theme",
            get(handlers::me::get_theme).patch(handlers::me::update_theme),
        )
        .route(
            "/me/time-format",
            get(handlers::me::get_time_format).patch(handlers::me::update_time_format),
        )
        .route("/me/sessions", get(handlers::me::list_sessions))
        .route(
            "/me/sessions/{id}",
            axum::routing::delete(handlers::me::revoke_session),
        )
        .route("/me/orgs", get(handlers::orgs::list_my_orgs))
        .route(
            "/me/deleted-orgs",
            get(handlers::orgs::list_my_deleted_orgs),
        )
        .route("/me/active-org", post(handlers::orgs::switch_active_org))
        .route(
            "/me/api-tokens",
            get(handlers::api_tokens::list).post(handlers::api_tokens::create),
        )
        .route(
            "/me/api-tokens/{id}",
            axum::routing::patch(handlers::api_tokens::rename).delete(handlers::api_tokens::revoke),
        )
        .route(
            "/orgs/{id}/invitations",
            get(handlers::invitations::list).post(handlers::invitations::create),
        )
        .route(
            "/orgs/{id}/invitations/{invitation_id}",
            axum::routing::delete(handlers::invitations::revoke),
        )
        .route(
            "/orgs/{id}/invitations/{invitation_id}/resend",
            post(handlers::invitations::resend),
        )
        .route("/invitations/accept", post(handlers::invitations::accept))
        .route("/invitations/decline", post(handlers::invitations::decline));

    // Added inside the chain below so it still inherits auth + rate limiting.
    if state.cfg.email.support_enabled() {
        v1 = v1.route("/support", post(handlers::support::create));
    }

    let mut v1 = v1
        // Added before the auth layer so it ends up *inner*: auth runs
        // first and populates `AuthContext`, then the rate-limit middleware
        // keys on the resolved org/user (never the TCP peer).
        .layer(from_fn_with_state(state.clone(), rate_limit_middleware))
        .layer(from_fn_with_state(
            state.clone(),
            crate::web::auth::api_token::middleware,
        ))
        .layer(DefaultBodyLimit::max(SINGLE_BODY_LIMIT))
        .merge(bulk)
        .merge(logo);

    if let Some(layer) = cors_layer(&state.cfg.api.cors) {
        v1 = v1.layer(layer);
    }

    let mut auth_routes = Router::new()
        .route("/auth/github/login", get(handlers::auth::github_login))
        .route(
            "/auth/github/callback",
            get(handlers::auth::github_callback),
        )
        .route("/auth/google/login", get(handlers::auth::google_login))
        .route(
            "/auth/google/callback",
            get(handlers::auth::google_callback),
        )
        .route(
            "/auth/microsoft/login",
            get(handlers::auth::microsoft_login),
        )
        .route(
            "/auth/microsoft/callback",
            get(handlers::auth::microsoft_callback),
        )
        .route("/auth/gitlab/login", get(handlers::auth::gitlab_login))
        .route(
            "/auth/gitlab/callback",
            get(handlers::auth::gitlab_callback),
        )
        // POST, so the CSRF guard covers a request that mints state and starts
        // a credential-adding dance; a GET here is forgeable from any page.
        .route("/auth/github/link", post(handlers::auth::github_link))
        .route("/auth/google/link", post(handlers::auth::google_link))
        .route("/auth/microsoft/link", post(handlers::auth::microsoft_link))
        .route("/auth/gitlab/link", post(handlers::auth::gitlab_link))
        .route("/auth/logout", post(handlers::auth::logout))
        .route("/auth/logout-all", post(handlers::auth::logout_all));

    // Mounted only where they can complete, so a deployment without them
    // answers 404 rather than starting a ceremony that cannot finish.
    if state.cfg.auth.passkey_login_enabled() {
        auth_routes = auth_routes
            .route(
                "/auth/passkey/register/start",
                post(handlers::passkeys::register_start),
            )
            .route(
                "/auth/passkey/register/finish",
                post(handlers::passkeys::register_finish),
            )
            .route(
                "/auth/passkey/login/start",
                post(handlers::passkeys::login_start),
            )
            .route(
                "/auth/passkey/login/finish",
                post(handlers::passkeys::login_finish),
            );
    }

    if state.cfg.auth.magic_link_enabled() {
        auth_routes = auth_routes
            .route(
                "/auth/magic-link/request",
                post(handlers::magic_link::request),
            )
            .route(
                "/auth/magic-link/verify",
                get(handlers::magic_link::verify_landing)
                    .post(handlers::magic_link::verify_confirm),
            )
            .route(
                "/auth/magic-link/code",
                post(handlers::magic_link::submit_code),
            );
    }

    // Region-agent surface. Auth is the `AgentIdentity` extractor in each
    // handler (resolves the agent token against the `agents` table), NOT the
    // tenant `api_token` middleware — leaving that off means no `AuthContext` is
    // ever populated here, so a `sm_live_` tenant token can never reach this
    // nest. Result batches reuse the bulk body limit.
    let agent = Router::new()
        .route("/targets", get(handlers::agents::pull_targets))
        .route("/results", post(handlers::agents::ingest_results))
        .route("/dispatch", get(handlers::agents::claim_dispatch))
        .route(
            "/dispatch/results",
            post(handlers::agents::submit_dispatch_result),
        )
        .layer(DefaultBodyLimit::max(BULK_BODY_LIMIT));

    // Operator (instance-admin) surface. Each handler is gated by the
    // `OperatorAuth` extractor (static bearer secret); when the secret is unset
    // the extractor 404s, so the surface is invisible.
    let operator = Router::new()
        .route(
            "/regions",
            get(handlers::operator::list_regions).post(handlers::operator::create_region),
        )
        .route(
            "/regions/{id}",
            axum::routing::patch(handlers::operator::update_region)
                .delete(handlers::operator::delete_region),
        )
        .route(
            "/agents",
            get(handlers::operator::list_agents).post(handlers::operator::create_agent),
        )
        .route(
            "/agents/{id}",
            axum::routing::patch(handlers::operator::update_agent)
                .delete(handlers::operator::delete_agent),
        )
        .route(
            "/accounts/{id}/plan",
            axum::routing::put(handlers::operator_accounts::set_account_plan),
        )
        .route(
            "/accounts/{id}/overrides",
            axum::routing::put(handlers::operator_accounts::set_account_overrides)
                .delete(handlers::operator_accounts::clear_account_overrides),
        )
        .layer(DefaultBodyLimit::max(SINGLE_BODY_LIMIT));

    let mut root = Router::new()
        .route("/healthz", get(handlers::health::healthz))
        .route("/readyz", get(handlers::health::readyz))
        .merge(auth_routes)
        .nest("/api/v1", v1)
        .nest("/api/agent", agent)
        .nest("/operator", operator);

    if public_routes_active(&state.cfg) {
        root = root.nest("/api/public/v1", build_public_router());
    }

    // CSRF lives one layer up (router::build_app_router) so it wraps
    // the api router AND the merged web::routes() together. Keeping it
    // here would leave any future state-changing route on the web
    // router silently un-protected — the same drift pattern that bit
    // the health-path predicate before centralisation.
    root.merge(SwaggerUi::new("/docs").url("/api/openapi.json", ApiDoc::openapi()))
        .layer(from_fn(api_middleware::cache_control))
        .layer(from_fn(api_middleware::json_charset))
        .layer(from_fn(api_middleware::navigation_login_redirect))
        .layer(tower_cookies::CookieManagerLayer::new())
        .with_state(state)
}

/// Path-based public surface (`/status/<slug>` HTML + `/api/public/v1/*` JSON
/// on the operator host, org resolved from the slug). Set to `false` in
/// SaaS-strict deployments that route every public surface through the
/// per-org subdomain.
pub fn path_based_public_routes_enabled(cfg: &crate::config::AppConfig) -> bool {
    cfg.tenancy.path_based_public_routes
}

/// Per-org subdomain surface (`*.{public_status.base_domain}`, org resolved
/// from the `Host` header — apex-wildcard shape). A startup assertion
/// refuses to boot if this is set without a well-formed base domain.
pub fn subdomain_public_routes_enabled(cfg: &crate::config::AppConfig) -> bool {
    cfg.tenancy.subdomain_public_routes
}

/// Whether *any* public surface is mounted. The two surfaces are mutually
/// exclusive in practice (the startup assertions forbid path-based + SaaS,
/// and subdomain needs SaaS), so the shared handlers resolve the org via the
/// host-aware [`crate::web::host::StatusPageOrg`] extractor and only one
/// surface is ever live per deployment.
pub fn public_routes_active(cfg: &crate::config::AppConfig) -> bool {
    path_based_public_routes_enabled(cfg) || subdomain_public_routes_enabled(cfg)
}

/// Builds the public, unauthenticated `/api/public/v1/*` router. Lives in its
/// own function and is composed via `nest` so future operator-side
/// middlewares (auth, idempotency, etc.) added to the operator router cannot
/// accidentally cover the public surface.
fn build_public_router() -> Router<AppState> {
    Router::new()
        .route("/status", get(handlers::public::public_status))
        .route(
            "/components/{id}/history",
            get(handlers::public::component_history),
        )
        .route("/incidents", get(handlers::public::public_incidents))
        .route("/incidents/{id}", get(handlers::public::public_incident))
        .route(
            "/incidents.rss",
            get(handlers::public::public_incidents_rss),
        )
        .route("/maintenance", get(handlers::public::public_maintenance))
        .route("/badge.svg", get(handlers::public::public_badge))
        .layer(from_fn(api_middleware::public_cache_control))
}

/// Builds a CORS layer when `api.cors.enabled = true`. Wildcard origins are
/// only honored via `allow_any_origin`; literal `"*"` inside `allowed_origins`
/// fails the process at startup so misconfiguration cannot silently open the
/// API to any browser.
fn cors_layer(cfg: &CorsConfig) -> Option<CorsLayer> {
    if !cfg.enabled {
        return None;
    }
    let origin = if cfg.allow_any_origin {
        assert!(
            cfg.allowed_origins.is_empty(),
            "api.cors.allow_any_origin = true is mutually exclusive with allowed_origins"
        );
        AllowOrigin::any()
    } else {
        assert!(
            !cfg.allowed_origins.is_empty(),
            "api.cors.enabled = true requires allowed_origins or allow_any_origin"
        );
        let parsed: Vec<HeaderValue> = cfg
            .allowed_origins
            .iter()
            .map(|o| {
                assert!(
                    !o.contains('*'),
                    "api.cors.allowed_origins entry '{o}' contains '*'; set allow_any_origin = true instead"
                );
                HeaderValue::from_str(o).expect("invalid api.cors.allowed_origins entry")
            })
            .collect();
        AllowOrigin::list(parsed)
    };
    let methods: Vec<Method> = cfg
        .allowed_methods
        .iter()
        .map(|m| {
            m.parse::<Method>()
                .expect("invalid api.cors.allowed_methods entry")
        })
        .collect();
    Some(
        CorsLayer::new()
            .allow_origin(origin)
            .allow_methods(methods)
            .allow_headers([header::CONTENT_TYPE]),
    )
}
