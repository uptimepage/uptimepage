use axum::Router;
use axum::response::Redirect;
use axum::routing::{get, post};
use tower_cookies::CookieManagerLayer;

use crate::api::public_routes_active;
use crate::app::AppState;
use crate::web::{assets, error, views};

/// Builds the UI router with state applied. The public-status pages
/// (`/status`, `/status/incidents/{id}`) are mounted only when
/// [`public_routes_active`] is true; the org they render is resolved
/// per-request by the host-aware `StatusPageOrg` extractor (subdomain →
/// that tenant; self-host → the default org). `/` runs a host-aware
/// dispatcher ([`views::dashboard::root`]) so on the SaaS subdomain
/// surface each org's page lives at the apex of its host (industry
/// parity); the operator dashboard keeps `/` on its own host.
pub fn routes(state: AppState) -> Router {
    let cfg = &state.cfg;
    crate::web::filters::set_escalation_ui(cfg.escalation.enabled);
    crate::web::filters::set_support_ui(cfg.email.support_enabled());
    crate::web::filters::set_billing_ui(state.billing.is_some());
    let mut r = Router::new()
        .route("/", get(views::dashboard::root))
        .route("/targets", get(views::targets_list::index))
        .route("/targets/new", get(views::targets_form::new_form))
        .route("/targets/{id}", get(views::targets_detail::index))
        .route(
            "/targets/{id}/incidents",
            get(views::targets_detail::incidents),
        )
        .route("/targets/{id}/edit", get(views::targets_form::edit_form))
        .route("/incidents", get(views::incidents::list))
        .route(
            "/web/partials/incidents",
            get(views::incidents::list_partial),
        )
        .route("/incidents/declare", get(views::incidents::declare_form))
        .route("/incidents/reports", get(views::incidents::reports))
        .route("/incidents/{id}", get(views::incidents::detail))
        .route("/incidents/{id}/edit", get(views::incidents::edit_form))
        .route(
            "/incidents/{id}/postmortem",
            get(views::incidents::postmortem_form),
        )
        .route("/web/targets/list", get(views::targets_list::list_partial))
        .route(
            "/web/partials/targets/{id}/live",
            get(views::targets_detail::live_partial),
        )
        .route(
            "/web/partials/targets/{id}/checks",
            get(views::targets_detail::check_rows),
        )
        // Public capability links: a token grants read-only access to one
        // monitor's detail view. Unauthenticated by design; the token resolves
        // to its org. Always mounted (not gated on public_routes_active) — share
        // links live on the operator app host, not the per-tenant status hosts.
        .route("/m/{token}", get(views::share::detail))
        .route("/m/{token}/incidents", get(views::share::incidents))
        .route("/m/{token}/live", get(views::share::live_partial))
        .route("/m/{token}/latency", get(views::share::latency))
        .route("/m/{token}/results", get(views::share::results))
        // Inbound heartbeat pings, token-authenticated like the share links.
        // GET for curl/cron one-liners, POST for HTTP clients that default to it.
        .route(
            "/ping/{token}",
            get(views::heartbeat::ping).post(views::heartbeat::ping),
        )
        // `/start`, `/fail`, and `curl $URL/$?` — the job's own account of a run,
        // beside the bare URL that only says "still alive".
        .route(
            "/ping/{token}/{signal}",
            get(views::heartbeat::ping_signal).post(views::heartbeat::ping_signal),
        )
        // Public email-channel verification: possession of the mailed token
        // is the proof; same always-mounted reasoning as the share links.
        .route("/verify-channel", get(views::verify_channel::verify))
        // GET confirms (so a link-scanner's prefetch can't disable a channel),
        // POST performs the stop and serves the RFC 8058 one-click.
        .route(
            "/alert-channel/stop",
            get(views::alert_channel_stop::confirm).post(views::alert_channel_stop::stop),
        )
        // Same GET-confirms/POST-acts shape: a prefetch must not silence a
        // live incident.
        .route(
            "/incident/ack",
            get(views::incident_ack::confirm).post(views::incident_ack::ack),
        )
        // Public status-page subscriptions: confirm/unsubscribe carry their own
        // token/HMAC proof, so they're always mounted like the verify link.
        .route("/subscribe", post(views::subscribe::subscribe))
        .route("/subscribe/confirm", get(views::subscribe::confirm))
        // GET confirms (prefetch-safe); POST unsubscribes and serves the RFC
        // 8058 one-click from mail clients.
        .route(
            "/subscribe/unsubscribe",
            get(views::subscribe::unsubscribe_confirm).post(views::subscribe::unsubscribe),
        )
        // Public delegation connect page: a single-use code attaches one
        // notification channel to the inviting org; same always-mounted
        // reasoning as the share links.
        .route("/c/done", get(views::delegate_connect::done))
        .route("/c/{code}", get(views::delegate_connect::page))
        .route("/c/{code}/status", get(views::delegate_connect::status))
        .route("/c/{code}/create", post(views::delegate_connect::create))
        .route(
            "/c/{code}/slack/start",
            get(views::delegate_connect::slack_start),
        )
        .route(
            "/c/{code}/discord/start",
            get(views::delegate_connect::discord_start),
        )
        .route(
            "/web/partials/dashboard",
            get(views::dashboard::table_partial),
        )
        .route("/web/partials/nav", get(views::nav::context))
        .route("/login", get(views::auth::login))
        // Both reachable without an active account: the first with a session
        // whose user is soft-deleted, the second with no session at all.
        .route("/account/restore", get(views::auth::restore_page))
        .route("/account/deleted", get(views::auth::deleted_page))
        .route(
            "/invitations/accept",
            get(views::invitations::accept_landing),
        )
        .route(
            "/invitations/decline",
            get(views::invitations::decline_landing),
        )
        .route(
            "/settings/account",
            get(views::auth::settings::account_page),
        )
        .route(
            "/settings/sessions",
            get(views::auth::settings::sessions_page),
        )
        .route(
            "/settings/api-tokens",
            get(views::auth::settings::api_tokens_page),
        )
        .route("/settings/usage", get(views::auth::settings::usage_page))
        .route("/settings/team", get(views::team::page))
        .route("/web/partials/settings/team", get(views::team::partial))
        .route("/settings/organizations", get(views::organizations::page))
        .route(
            "/web/partials/settings/organizations",
            get(views::organizations::partial),
        )
        .route("/settings/pages", get(views::pages::pages_list))
        .route("/settings/pages/{id}", get(views::pages::page_editor))
        .route(
            "/web/partials/settings/pages",
            get(views::pages::pages_partial),
        )
        .route(
            "/settings/notifications",
            get(views::notification_channels::index),
        )
        .route(
            "/settings/notifications/new",
            get(views::notification_channels::new_form),
        )
        .route(
            "/settings/notifications/{id}/edit",
            get(views::notification_channels::edit_form),
        )
        .route(
            "/web/partials/settings/notifications",
            get(views::notification_channels::list_partial),
        )
        .route("/settings/variables", get(views::variables::index))
        .route(
            "/web/partials/settings/variables",
            get(views::variables::list_partial),
        )
        .route(
            "/web/partials/settings/sessions",
            get(views::auth::settings::sessions_partial),
        )
        .route(
            "/web/partials/settings/api-tokens",
            get(views::auth::settings::api_tokens_partial),
        )
        .route("/terms", get(views::legal::terms))
        .route("/privacy", get(views::legal::privacy))
        .route("/cookies", get(views::legal::cookies))
        .route("/impressum", get(views::legal::impressum))
        .route("/abuse-policy", get(views::legal::abuse_policy))
        .route("/security-policy", get(views::legal::security_policy))
        .route("/licenses", get(views::legal::licenses))
        .route("/.well-known/security.txt", get(views::legal::security_txt));

    // Team-paging surfaces — mounted only when escalation is enabled, so a
    // single-responder deployment never exposes policy/schedule config that
    // nothing consumes.
    if cfg.escalation.enabled {
        r = r
            .route("/settings/escalation", get(views::escalation::index))
            .route("/settings/escalation/new", get(views::escalation::new_form))
            .route(
                "/settings/escalation/{id}/edit",
                get(views::escalation::edit_form),
            )
            .route(
                "/web/partials/settings/escalation",
                get(views::escalation::list_partial),
            )
            .route("/settings/on-call", get(views::on_call::index))
            .route("/settings/on-call/new", get(views::on_call::new_form))
            .route(
                "/settings/on-call/{id}/edit",
                get(views::on_call::edit_form),
            )
            .route(
                "/web/partials/settings/on-call",
                get(views::on_call::list_partial),
            );
    }

    // Only where an operator staffs an inbox, so no page goes nowhere.
    if cfg.email.support_enabled() {
        r = r.route("/help", get(views::help::page));
    }

    // Central Telegram bot receiver. Mounted only when an operator bot token
    // is configured; otherwise the feature is absent. Lives on the app host
    // alongside the share links, authenticated by the per-update secret token.
    if cfg.telegram.enabled() {
        r = r.route(
            crate::telegram::WEBHOOK_PATH,
            post(views::telegram::webhook),
        );
    }

    // Resend bounce/complaint receiver. Mounted only when the endpoint's
    // Svix signing secret is configured; the signature is the only auth.
    if cfg.email.resend.webhook_enabled() {
        r = r.route("/hooks/resend", post(views::resend_hook::webhook));
    }

    // Payment-provider receiver, the pay page the provider's checkout opens
    // on, and the card-update hop. Absent without a provider.
    if state.billing.is_some() {
        r = r
            .route(
                "/hooks/billing/{provider}",
                post(views::billing_hook::webhook),
            )
            .route("/pay", get(views::billing::pay_page))
            .route("/settings/billing", get(views::billing::billing_page))
            .route(
                "/settings/billing/payment-method",
                get(views::billing::payment_method),
            );
    }

    // Operator WhatsApp number receiver: Meta's GET subscribe handshake +
    // signed message deliveries. Mounted only when the operator number is
    // configured AND the spend flag is on.
    if cfg.whatsapp_app.enabled() {
        r = r.route(
            crate::whatsapp::WEBHOOK_PATH,
            get(views::whatsapp::verify).post(views::whatsapp::webhook),
        );
    }

    // "Add to Slack" / "Add to Discord" connect dances. Mounted only when
    // the operator app is configured; manual webhook paste works either way.
    if cfg.slack_oauth.enabled() {
        r = r
            .route("/auth/slack/start", get(views::slack_connect::start))
            .route("/auth/slack/callback", get(views::slack_connect::callback));
    }
    if cfg.discord_oauth.enabled() {
        r = r
            .route("/auth/discord/start", get(views::discord_connect::start))
            .route(
                "/auth/discord/callback",
                get(views::discord_connect::callback),
            );
    }

    if public_routes_active(cfg) {
        r = r
            .route("/status", get(views::public_status::status_path))
            .route("/status/incidents", get(views::public_status::archive))
            .route(
                "/status/incidents/{id}",
                get(views::public_status::incident),
            )
            .route(
                views::public_status::LOGO_ROUTE,
                get(views::public_status::logo),
            );
    }

    // OAuth 2.1 authorization-server + RFC 9728 resource metadata for the MCP
    // connector. Merged here so the endpoints inherit cookies (the consent flow
    // reads the session) + CSRF + the cross-cutting layers. Gated internally on
    // mcp config.
    r = r.merge(crate::oauth::routes(cfg));

    // This host serves the API the catalog describes, so an agent handed an
    // API URL looks for it here. The document itself stays single-sourced on
    // the marketing host.
    if cfg.marketing.enabled && !cfg.marketing.canonical_origin.is_empty() {
        let catalog = format!(
            "{}{}",
            cfg.marketing.canonical_origin.trim_end_matches('/'),
            crate::marketing::discovery::CATALOG_PATH
        );
        r = r.route(
            crate::marketing::discovery::CATALOG_PATH,
            get(move || {
                let catalog = catalog.clone();
                async move { Redirect::temporary(&catalog) }
            }),
        );
    }

    assets::mount_static(r)
        .fallback(error::not_found)
        .layer(CookieManagerLayer::new())
        .with_state(state)
}
