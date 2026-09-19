//! Server-rendered notification-channel pages under `/settings/notifications`:
//! a list (with send-test and delete row actions) and a create/edit form.
//!
//! Mutations are driven from the page against the JSON API
//! (`/api/v1/notification-channels`), so this module only renders chrome and
//! prefills the form. The org is resolved by [`CurrentOrg`] exactly as the API
//! resolves it, so the UI always acts on the caller's own tenant.
//!
//! Secrets never reach the browser: the edit form prefills config from the
//! redacted channel (every secret-bearing field is `***`). Editing config
//! therefore goes through a single "Replace transport config" toggle — when
//! off, the form omits `config` from the PATCH and the stored secret is kept;
//! when on, the operator re-enters the whole config (the API rejects a
//! re-submitted `***`).

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{
    ChannelConfig, MAX_CHANNEL_NAME_LEN, NewNotificationChannel, NotificationChannel, OrgId,
    Target, WriteSource,
};
use crate::error::AppError;
use crate::error::codes;
use crate::storage::NotificationChannelStore;
use crate::storage::traits::TargetFilter;
use crate::web::CurrentOrg;
use crate::web::error::WebResult;
use crate::web::filters;
use crate::web::views::{describe_check, json_pretty, resolve_org};

const TAB_NOTIFICATIONS: &str = "notifications";

pub struct ChannelRow {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub icon: &'static str,
    pub enabled: bool,
    /// Email channel still awaiting address verification.
    pub unverified: bool,
    /// Why the platform turned this channel off; empty when an operator did.
    pub disabled_reason: String,
    /// How long nothing has landed on a channel that is still being paged.
    pub failing_for: Option<String>,
    /// How long since the last landed delivery.
    pub last_delivered: Option<String>,
    pub created: chrono::DateTime<chrono::Utc>,
    /// `terraform`/`api` chip for externally-managed channels; `None` (UI) hides it.
    pub managed_by: Option<&'static str>,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/notifications.html")]
pub struct ChannelsPage {
    pub active_tab: &'static str,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/notifications_partial.html")]
pub struct ChannelsPartial {
    pub channels: Vec<ChannelRow>,
}

/// One config field group. Only the fields for the selected `kind` are
/// submitted (the JS prunes the rest); on edit, secret-bearing fields carry
/// the `***` sentinel until the operator opts to replace the config.
pub struct ConfigFields {
    pub slack_webhook_url: String,
    pub slack_mention: String,
    pub discord_webhook_url: String,
    pub discord_mention: String,
    pub msteams_webhook_url: String,
    pub google_chat_webhook_url: String,
    pub email_to: String,
    pub webhook_url: String,
    pub webhook_headers_json: String,
    pub webhook_secret: String,
    pub telegram_bot_token: String,
    pub telegram_chat_id: String,
    pub telegram_app_chat_id: String,
    pub telegram_app_chat_title: String,
    pub whatsapp_app_phone: String,
    pub whatsapp_app_profile_name: String,
    pub whatsapp_access_token: String,
    pub whatsapp_phone_number_id: String,
    pub whatsapp_to: String,
    pub whatsapp_template_name: String,
    pub whatsapp_language_code: String,
    pub pagerduty_routing_key: String,
    pub ntfy_server_url: String,
    pub ntfy_topic: String,
    pub ntfy_access_token: String,
    pub gotify_server_url: String,
    pub gotify_token: String,
    pub mattermost_webhook_url: String,
    pub mattermost_mention: String,
    pub pushover_token: String,
    pub pushover_user: String,
    pub pushover_device: String,
    pub pushover_emergency: bool,
    pub sms_provider: String,
    pub sms_to: String,
    pub sms_twilio_from: String,
    pub sms_twilio_account_sid: String,
    pub sms_twilio_auth_token: String,
    pub sms_telnyx_from: String,
    pub sms_telnyx_api_key: String,
    pub sms_telnyx_messaging_profile_id: String,
    pub sms_vonage_from: String,
    pub sms_vonage_api_key: String,
    pub sms_vonage_api_secret: String,
    pub sms_plivo_from: String,
    pub sms_plivo_auth_id: String,
    pub sms_plivo_auth_token: String,
    pub sms_sinch_from: String,
    pub sms_sinch_service_plan_id: String,
    pub sms_sinch_api_token: String,
    pub sms_sinch_region: String,
}

impl Default for ConfigFields {
    fn default() -> Self {
        Self {
            slack_webhook_url: String::new(),
            slack_mention: String::new(),
            discord_webhook_url: String::new(),
            discord_mention: String::new(),
            msteams_webhook_url: String::new(),
            google_chat_webhook_url: String::new(),
            email_to: String::new(),
            webhook_url: String::new(),
            webhook_headers_json: "{}".into(),
            webhook_secret: String::new(),
            telegram_bot_token: String::new(),
            telegram_chat_id: String::new(),
            telegram_app_chat_id: String::new(),
            telegram_app_chat_title: String::new(),
            whatsapp_app_phone: String::new(),
            whatsapp_app_profile_name: String::new(),
            whatsapp_access_token: String::new(),
            whatsapp_phone_number_id: String::new(),
            whatsapp_to: String::new(),
            whatsapp_template_name: String::new(),
            whatsapp_language_code: String::new(),
            pagerduty_routing_key: String::new(),
            ntfy_server_url: "https://ntfy.sh".into(),
            ntfy_topic: String::new(),
            ntfy_access_token: String::new(),
            gotify_server_url: String::new(),
            gotify_token: String::new(),
            mattermost_webhook_url: String::new(),
            mattermost_mention: String::new(),
            pushover_token: String::new(),
            pushover_user: String::new(),
            pushover_device: String::new(),
            pushover_emergency: false,
            sms_provider: "twilio".into(),
            sms_to: String::new(),
            sms_twilio_from: String::new(),
            sms_twilio_account_sid: String::new(),
            sms_twilio_auth_token: String::new(),
            sms_telnyx_from: String::new(),
            sms_telnyx_api_key: String::new(),
            sms_telnyx_messaging_profile_id: String::new(),
            sms_vonage_from: String::new(),
            sms_vonage_api_key: String::new(),
            sms_vonage_api_secret: String::new(),
            sms_plivo_from: String::new(),
            sms_plivo_auth_id: String::new(),
            sms_plivo_auth_token: String::new(),
            sms_sinch_from: String::new(),
            sms_sinch_service_plan_id: String::new(),
            sms_sinch_api_token: String::new(),
            sms_sinch_region: "us".into(),
        }
    }
}

/// A monitor card on the channel edit page — either already bound to the
/// channel (used-by grid) or offered by the "+ add monitor" picker.
pub struct MonitorCard {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub addr: String,
    pub enabled: bool,
    /// `terraform`/`api` chip — a UI bind may be overwritten on the next
    /// apply, so the card says who manages the monitor.
    pub managed_by: Option<&'static str>,
    /// Space-joined tags, feeding the picker's client-side search.
    pub tags: String,
    /// The same tags as JSON, for matching a rule tag that contains a space.
    pub tags_json: String,
}

pub struct ChannelFormModel {
    /// `"create"` or `"edit"`. Drives the heading, submit verb, and (when not
    /// `"create"`) the "Replace transport config" toggle.
    pub mode: &'static str,
    /// Channel id; empty on create. The bind picker PATCHes it into a
    /// monitor's alert bindings.
    pub channel_id: String,
    pub action: String,
    pub submit_method: &'static str,
    pub name: String,
    pub enabled: bool,
    /// The channel's tag rule.
    pub auto_bind_tags: Vec<String>,
    /// The org's tags, offered as chips. Carries the rule's own tags even
    /// where they fall outside the capped inventory, so none renders missing.
    pub tag_options: Vec<String>,
    /// Platform-disable note; empty when none.
    pub disabled_reason: String,
    pub kind: &'static str,
    pub config: ConfigFields,
    /// Monitors bound to this channel; always empty on create.
    pub used_by: Vec<MonitorCard>,
    /// Monitors NOT bound to this channel, offered by the bind picker.
    pub bindable: Vec<MonitorCard>,
    /// Gates the one-tap "telegram" card; the BYO card is always offered.
    pub central_telegram: bool,
    /// Gates the one-tap "whatsapp" card; the BYO card is always offered.
    pub central_whatsapp: bool,
    /// Gates the "add to Slack" button on the slack panel (create mode).
    pub slack_oauth: bool,
    /// Gates the "add to Discord" button on the discord panel (create mode).
    pub discord_oauth: bool,
    /// Email channel still awaiting address verification (edit mode).
    pub email_unverified: bool,
}

impl ChannelFormModel {
    /// On create when the bot is configured; on edit only for an
    /// already-linked channel (stays viewable if the bot is later removed).
    pub fn offers_telegram_app(&self) -> bool {
        (self.central_telegram && self.mode == "create") || self.kind == "telegram_app"
    }

    /// Same shape as [`Self::offers_telegram_app`], for the operator
    /// WhatsApp number.
    pub fn offers_whatsapp_app(&self) -> bool {
        (self.central_whatsapp && self.mode == "create") || self.kind == "whatsapp_app"
    }
}

impl ChannelFormModel {
    /// `"1 monitor"` / `"3 monitors"` — one pluralization for the header
    /// count and the delete warning.
    pub fn used_by_label(&self) -> String {
        let n = self.used_by.len();
        if n == 1 {
            "1 monitor".into()
        } else {
            format!("{n} monitors")
        }
    }

    /// The org has no monitors at all — the picker can only offer creating
    /// one.
    pub fn has_no_monitors(&self) -> bool {
        self.used_by.is_empty() && self.bindable.is_empty()
    }

    /// Every monitor is already bound to this channel.
    pub fn all_bound(&self) -> bool {
        self.bindable.is_empty() && !self.used_by.is_empty()
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/notification_form.html")]
pub struct ChannelFormPage {
    pub active_tab: &'static str,
    pub form: ChannelFormModel,
}

impl ChannelFormPage {
    /// A rule names monitor tags, so it inherits their bounds.
    fn max_tags(&self) -> usize {
        crate::domain::target::MAX_TAGS_PER_TARGET
    }

    fn max_tag_len(&self) -> usize {
        crate::domain::target::MAX_TAG_LEN
    }
}

pub async fn index(org: Result<CurrentOrg, AppError>) -> Response {
    match resolve_org(org, "/settings/notifications") {
        Ok(_) => ChannelsPage {
            active_tab: TAB_NOTIFICATIONS,
        }
        .into_response(),
        Err(resp) => *resp,
    }
}

pub async fn list_partial(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
) -> WebResult<Response> {
    let org = match resolve_org(org, "/settings/notifications") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let failure_limit = state.cfg.escalation.channel_failure_limit;
    let now = chrono::Utc::now();
    let channels = state
        .notification_channel_store
        .list(org)
        .await?
        .into_iter()
        .map(|c| ChannelRow {
            id: c.id.to_string(),
            kind: super::channel_kind_label(c.kind),
            icon: super::channel_kind_icon(c.kind),
            enabled: c.enabled,
            unverified: c.awaiting_verification(),
            disabled_reason: c.disabled_reason.clone().unwrap_or_default(),
            failing_for: c
                .is_failing(failure_limit)
                .then(|| c.failing_since.map(|s| super::humanize_duration(now - s)))
                .flatten(),
            last_delivered: c
                .last_delivered_at
                .map(|t| super::humanize_duration(now - t)),
            created: c.created_at,
            managed_by: c.write_source.managed_label(),
            name: c.name,
        })
        .collect();
    Ok(ChannelsPartial { channels }.into_response())
}

pub async fn new_form(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
) -> WebResult<Response> {
    let org = match resolve_org(org, "/settings/notifications/new") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let mut form = empty_create_form();
    form.central_telegram = state.cfg.telegram.enabled();
    form.central_whatsapp = state.cfg.whatsapp_app.enabled();
    form.slack_oauth = state.cfg.slack_oauth.enabled();
    form.discord_oauth = state.cfg.discord_oauth.enabled();
    let (options, picked) = rule_tag_options(&state, org, &form.auto_bind_tags).await?;
    form.tag_options = options;
    form.auto_bind_tags = picked;
    (_, form.bindable) = org_monitor_cards(&state, org, None).await?;
    Ok(ChannelFormPage {
        active_tab: TAB_NOTIFICATIONS,
        form,
    }
    .into_response())
}

/// The tag chips a rule can be built from, and the rule restated in the
/// spellings those chips carry. Matching folds case, so a rule reading `DB`
/// ticks the org's own `db` chip instead of adding a second one — and a chip
/// left unticked is one the next save would clear.
async fn rule_tag_options(
    state: &AppState,
    org: crate::domain::OrgId,
    rule: &[String],
) -> Result<(Vec<String>, Vec<String>), AppError> {
    let options: Vec<String> = state
        .target_store
        .list_tags(org, None, 200)
        .await?
        .into_iter()
        .map(|t| t.name)
        .collect();
    Ok(fold_rule_into_options(options, rule))
}

/// The pure half of [`rule_tag_options`]: the org's inventory and a stored rule
/// in, the chips to show and the ones to tick out.
fn fold_rule_into_options(mut options: Vec<String>, rule: &[String]) -> (Vec<String>, Vec<String>) {
    let mut picked: Vec<String> = Vec::with_capacity(rule.len());
    for t in rule {
        match options
            .iter()
            .find(|o| o.to_lowercase() == t.to_lowercase())
        {
            Some(chip) => picked.push(chip.clone()),
            None => {
                options.push(t.clone());
                picked.push(t.clone());
            }
        }
    }
    (options, picked)
}

/// All org monitors as cards, split by alert binding to `channel`:
/// `(used_by, bindable)`. With no channel everything is bindable.
async fn org_monitor_cards(
    state: &AppState,
    org: crate::domain::OrgId,
    channel: Option<Uuid>,
) -> Result<(Vec<MonitorCard>, Vec<MonitorCard>), AppError> {
    // The default filter caps at 100, which would silently hide monitors
    // past that on large orgs — fetch the full set.
    let targets = state
        .target_store
        .list(
            org,
            TargetFilter {
                limit: Some(10_000),
                ..Default::default()
            },
        )
        .await?;
    let card = |t: Target| {
        let (kind, addr) = describe_check(&t.check);
        MonitorCard {
            id: t.id.to_string(),
            name: t.name,
            kind,
            addr,
            enabled: t.enabled,
            managed_by: t.write_source.managed_label(),
            tags: t.tags.join(" "),
            tags_json: serde_json::to_string(&t.tags).unwrap_or_else(|_| "[]".into()),
        }
    };
    let (used_by, bindable): (Vec<_>, Vec<_>) = targets
        .into_iter()
        .partition(|t| channel.is_some_and(|id| t.alerts.iter().any(|b| b.channel_id == id)));
    let cards = |v: Vec<Target>| {
        let mut cards: Vec<MonitorCard> = v.into_iter().map(&card).collect();
        // Alphabetical keeps the picker's search + pagination predictable.
        cards.sort_by_cached_key(|c| c.name.to_lowercase());
        cards
    };
    Ok((cards(used_by), cards(bindable)))
}

pub async fn edit_form(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
    Path(id): Path<Uuid>,
) -> WebResult<Response> {
    let org = match resolve_org(org, &format!("/settings/notifications/{id}/edit")) {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let channel = state
        .notification_channel_store
        .get(org, id)
        .await?
        .ok_or_else(|| {
            AppError::not_found("CHANNEL_NOT_FOUND", "notification channel not found")
        })?;
    let mut form = form_from_channel(channel);
    form.central_telegram = state.cfg.telegram.enabled();
    form.central_whatsapp = state.cfg.whatsapp_app.enabled();
    let (options, picked) = rule_tag_options(&state, org, &form.auto_bind_tags).await?;
    form.tag_options = options;
    form.auto_bind_tags = picked;
    (form.used_by, form.bindable) = org_monitor_cards(&state, org, Some(id)).await?;
    Ok(ChannelFormPage {
        active_tab: TAB_NOTIFICATIONS,
        form,
    }
    .into_response())
}

fn empty_create_form() -> ChannelFormModel {
    ChannelFormModel {
        mode: "create",
        channel_id: String::new(),
        action: "/api/v1/notification-channels".into(),
        submit_method: "POST",
        name: String::new(),
        enabled: true,
        auto_bind_tags: Vec::new(),
        tag_options: Vec::new(),
        disabled_reason: String::new(),
        kind: "slack",
        config: ConfigFields::default(),
        used_by: Vec::new(),
        bindable: Vec::new(),
        central_telegram: false,
        central_whatsapp: false,
        slack_oauth: false,
        discord_oauth: false,
        email_unverified: false,
    }
}

fn form_from_channel(c: NotificationChannel) -> ChannelFormModel {
    let email_unverified = c.awaiting_verification();
    // Prefill from the *redacted* config so non-secret routing (header names,
    // chat id) survives the edit while secrets stay masked.
    let mut redacted = c.config.clone();
    redacted.redact_in_place();
    let mut config = ConfigFields::default();
    match redacted {
        ChannelConfig::Slack(c) => {
            config.slack_webhook_url = c.webhook_url;
            config.slack_mention = c.mention.unwrap_or_default();
        }
        ChannelConfig::Discord(c) => {
            config.discord_webhook_url = c.webhook_url;
            config.discord_mention = c.mention.unwrap_or_default();
        }
        ChannelConfig::MsTeams(c) => config.msteams_webhook_url = c.webhook_url,
        ChannelConfig::GoogleChat(c) => config.google_chat_webhook_url = c.webhook_url,
        ChannelConfig::Email(c) => config.email_to = c.to,
        ChannelConfig::Webhook(c) => {
            config.webhook_url = c.url;
            config.webhook_headers_json = json_pretty(&c.headers);
            config.webhook_secret = c.secret.unwrap_or_default();
        }
        ChannelConfig::Telegram(c) => {
            config.telegram_bot_token = c.bot_token;
            config.telegram_chat_id = c.chat_id;
        }
        // Display-only: the API rejects this kind in request bodies.
        ChannelConfig::TelegramApp(c) => {
            config.telegram_app_chat_id = c.chat_id;
            config.telegram_app_chat_title = c.chat_title.unwrap_or_default();
        }
        // Display-only, same contract as telegram_app.
        ChannelConfig::WhatsAppApp(c) => {
            config.whatsapp_app_phone = c.phone;
            config.whatsapp_app_profile_name = c.profile_name.unwrap_or_default();
        }
        ChannelConfig::WhatsApp(c) => {
            config.whatsapp_access_token = c.access_token;
            config.whatsapp_phone_number_id = c.phone_number_id;
            config.whatsapp_to = c.to;
            config.whatsapp_template_name = c.template_name;
            config.whatsapp_language_code = c.language_code.unwrap_or_default();
        }
        ChannelConfig::PagerDuty(c) => config.pagerduty_routing_key = c.routing_key,
        ChannelConfig::Ntfy(c) => {
            config.ntfy_server_url = c.server_url;
            config.ntfy_topic = c.topic;
            config.ntfy_access_token = c.access_token.unwrap_or_default();
        }
        ChannelConfig::Mattermost(c) => {
            config.mattermost_webhook_url = c.webhook_url;
            config.mattermost_mention = c.mention.unwrap_or_default();
        }
        ChannelConfig::Gotify(c) => {
            config.gotify_server_url = c.server_url;
            config.gotify_token = c.token;
        }
        ChannelConfig::Pushover(c) => {
            config.pushover_token = c.token;
            config.pushover_user = c.user;
            config.pushover_device = c.device.unwrap_or_default();
            config.pushover_emergency = c.emergency;
        }
        ChannelConfig::Sms(c) => {
            config.sms_to = c.to().to_string();
            match c {
                crate::domain::SmsConfig::Twilio {
                    from,
                    account_sid,
                    auth_token,
                    ..
                } => {
                    config.sms_provider = "twilio".into();
                    config.sms_twilio_from = from;
                    config.sms_twilio_account_sid = account_sid;
                    config.sms_twilio_auth_token = auth_token;
                }
                crate::domain::SmsConfig::Telnyx {
                    from,
                    api_key,
                    messaging_profile_id,
                    ..
                } => {
                    config.sms_provider = "telnyx".into();
                    config.sms_telnyx_from = from;
                    config.sms_telnyx_api_key = api_key;
                    config.sms_telnyx_messaging_profile_id =
                        messaging_profile_id.unwrap_or_default();
                }
                crate::domain::SmsConfig::Vonage {
                    from,
                    api_key,
                    api_secret,
                    ..
                } => {
                    config.sms_provider = "vonage".into();
                    config.sms_vonage_from = from;
                    config.sms_vonage_api_key = api_key;
                    config.sms_vonage_api_secret = api_secret;
                }
                crate::domain::SmsConfig::Plivo {
                    from,
                    auth_id,
                    auth_token,
                    ..
                } => {
                    config.sms_provider = "plivo".into();
                    config.sms_plivo_from = from;
                    config.sms_plivo_auth_id = auth_id;
                    config.sms_plivo_auth_token = auth_token;
                }
                crate::domain::SmsConfig::Sinch {
                    from,
                    service_plan_id,
                    api_token,
                    region,
                    ..
                } => {
                    config.sms_provider = "sinch".into();
                    config.sms_sinch_from = from;
                    config.sms_sinch_service_plan_id = service_plan_id;
                    config.sms_sinch_api_token = api_token;
                    config.sms_sinch_region = region;
                }
            }
        }
    }
    ChannelFormModel {
        mode: "edit",
        channel_id: c.id.to_string(),
        action: format!("/api/v1/notification-channels/{}", c.id),
        submit_method: "PATCH",
        name: c.name,
        enabled: c.enabled,
        auto_bind_tags: c.auto_bind_tags,
        tag_options: Vec::new(),
        disabled_reason: c.disabled_reason.unwrap_or_default(),
        kind: c.kind.as_db_str(),
        config,
        used_by: Vec::new(),
        bindable: Vec::new(),
        central_telegram: false,
        central_whatsapp: false,
        slack_oauth: false,
        discord_oauth: false,
        email_unverified,
    }
}

/// Quota-block telemetry for a connect-flow create. The dashboard/API path
/// samples cap hits through the quota pre-check (uncontended path only);
/// link/OAuth flows go straight to the store cap, so
/// [`create_channel_deduped`] writes the sample itself.
pub struct QuotaBlockLog {
    pub db: Option<sqlx::PgPool>,
    pub user: Option<crate::domain::UserId>,
    pub flow: &'static str,
}

/// Create a connect-flow channel (telegram link, Slack OAuth, …), deduping
/// the name by suffixing (`Ops`, `Ops 2`, …); any non-name-collision error
/// propagates unchanged.
pub async fn create_channel_deduped(
    store: &dyn NotificationChannelStore,
    org: OrgId,
    base_name: &str,
    config: ChannelConfig,
    max_channels: i64,
    block_log: QuotaBlockLog,
) -> Result<NotificationChannel, AppError> {
    const MAX_SUFFIX: u32 = 50;
    let mut attempt = 1;
    loop {
        let suffix = (attempt > 1).then_some(attempt);
        let new = NewNotificationChannel {
            name: channel_name_with_suffix(base_name, suffix),
            config: config.clone(),
            enabled: true,
            // A connect flow knows only the destination; rules come later.
            auto_bind_tags: Vec::new(),
        };
        match store
            .create(org, new, WriteSource::Ui, max_channels, block_log.user)
            .await
        {
            Err(AppError::Unprocessable { code, .. })
                if code == codes::CHANNEL_NAME_TAKEN && attempt < MAX_SUFFIX =>
            {
                attempt += 1;
            }
            Err(err) => {
                if matches!(&err, AppError::Unprocessable { code, .. }
                    if *code == codes::CHANNEL_QUOTA_EXCEEDED)
                {
                    // current == limit by construction: the store cap refuses
                    // the INSERT at count >= limit. Keeping the {current,
                    // limit} shape of every other quota_exceeded row.
                    crate::quotas::service::record_quota_event(
                        block_log.db,
                        Some(org),
                        block_log.user,
                        "quota_exceeded",
                        Some("max_notification_channels"),
                        serde_json::json!({
                            "current": max_channels,
                            "limit": max_channels,
                            "flow": block_log.flow,
                        }),
                        None,
                    );
                }
                return Err(err);
            }
            ok => return ok,
        }
    }
}

/// `base` (optionally `base N`), trimmed so a long provider-supplied name
/// still leaves room for the dedupe suffix.
pub fn channel_name_with_suffix(base: &str, suffix: Option<u32>) -> String {
    let suffix = suffix.map(|n| format!(" {n}")).unwrap_or_default();
    let budget = MAX_CHANNEL_NAME_LEN - suffix.chars().count();
    let base: String = base.trim().chars().take(budget).collect();
    format!("{}{suffix}", base.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SlackConfig;

    #[test]
    fn new_form_renders_empty_create() {
        let html = ChannelFormPage {
            active_tab: TAB_NOTIFICATIONS,
            form: empty_create_form(),
        }
        .render()
        .unwrap();
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("New notification channel"));
        assert!(html.contains(r#"data-action="/api/v1/notification-channels""#));
        assert!(html.contains(r#"data-method="POST""#));
        assert!(html.contains(r#"data-mode="create""#));
        // Every transport offers a type card + config panel.
        for kind in [
            "slack",
            "discord",
            "msteams",
            "google_chat",
            "email",
            "webhook",
            "telegram",
            "whatsapp",
            "sms",
        ] {
            assert!(html.contains(&format!(r#"value="{kind}""#)), "{kind} card");
            assert!(
                html.contains(&format!(r#"data-variant="{kind}""#)),
                "{kind} panel"
            );
        }
        // Telegram setup helper: bot QR + chat-id probe.
        assert!(html.contains("data-tg-qr"));
        assert!(html.contains("data-tg-detect"));
        // Fingerprinted URL proves the vendored lib is actually embedded —
        // a missing file silently falls back to the bare path and 404s.
        assert!(
            crate::web::assets::url("js/qrcode.min.js").contains("?v="),
            "qrcode.min.js must be embedded"
        );
        // Create has no "replace config" toggle — config is always sent.
        assert!(!html.contains("Replace transport config"));
        // "Test now" works pre-save (ad-hoc config test); delete needs a
        // saved channel and stays edit-only.
        assert!(html.contains("data-send-test"));
        assert!(html.contains("test now"));
        assert!(!html.contains("delete channel"));
        // Create offers the same + card picker; picks are bound after create.
        assert!(html.contains("data-add-monitor"));
        assert!(html.contains("# none picked yet"));
        assert!(html.contains("# no monitors yet"));
    }

    #[test]
    fn create_form_one_tap_telegram_gated_on_central_bot() {
        // Without the central bot (self-host) the one-tap card is absent and
        // only the BYO "telegram bot" card is offered.
        let off = ChannelFormPage {
            active_tab: TAB_NOTIFICATIONS,
            form: empty_create_form(),
        }
        .render()
        .unwrap();
        assert!(!off.contains(r#"value="telegram_app""#));
        assert!(off.contains("telegram bot"));

        let mut form = empty_create_form();
        form.central_telegram = true;
        let on = ChannelFormPage {
            active_tab: TAB_NOTIFICATIONS,
            form,
        }
        .render()
        .unwrap();
        assert!(on.contains(r#"value="telegram_app""#));
        assert!(on.contains("one-tap chat link"));
        assert!(on.contains("data-tga-connect"));
        assert!(on.contains("data-tga-qr-box"));
    }

    #[test]
    fn create_form_oauth_connect_buttons_gated_on_config() {
        let off = ChannelFormPage {
            active_tab: TAB_NOTIFICATIONS,
            form: empty_create_form(),
        }
        .render()
        .unwrap();
        assert!(!off.contains("data-oauth-connect"));
        // Manual paste survives without the operator apps.
        assert!(off.contains("slack_webhook_url"));
        assert!(off.contains("discord_webhook_url"));

        let mut form = empty_create_form();
        form.slack_oauth = true;
        form.discord_oauth = true;
        let on = ChannelFormPage {
            active_tab: TAB_NOTIFICATIONS,
            form,
        }
        .render()
        .unwrap();
        assert!(on.contains(r#"href="/auth/slack/start""#));
        assert!(on.contains(r#"data-oauth-connect="slack""#));
        assert!(on.contains(r#"href="/auth/discord/start""#));
        assert!(on.contains(r#"data-oauth-connect="discord""#));
        assert!(on.contains("data-oauth-qr-box"));
        assert!(on.contains("slack_webhook_url"));
        assert!(on.contains("discord_webhook_url"));
    }

    #[test]
    fn edit_form_prefills_the_tag_rule_outside_the_replace_config_toggle() {
        let mut ch = slack_channel("https://hooks.slack.com/services/T/B/x");
        ch.auto_bind_tags = vec!["db".into(), "us east".into()];
        let mut form = form_from_channel(ch);
        assert_eq!(form.auto_bind_tags, vec!["db", "us east"]);
        // A tag holding a space rules out any joined form, so it stays a chip.
        form.tag_options = vec!["db".into(), "us east".into(), "web".into()];
        let html = ChannelFormPage {
            active_tab: TAB_NOTIFICATIONS,
            form,
        }
        .render()
        .unwrap();
        assert!(html.contains(r#"data-tag-chips="rule""#));
        // A chip widget without the script that wires it renders fine, refuses
        // to add a tag, and clears the rule on the next save.
        assert!(html.contains("js/ui/tag_chip_input"));
        assert!(html.contains(r#"value="us east" class="sr-only" checked"#));
        assert!(html.contains(r#"value="web" class="sr-only">"#));
    }

    /// A rule stored in another case must tick the org's own chip. Rendering
    /// it unticked reads as "no rule", and the next save PATCHes an empty
    /// list, dropping a routing rule the operator never touched.
    #[test]
    fn a_rule_spelled_in_another_case_ticks_the_orgs_own_chip() {
        let (options, picked) = fold_rule_into_options(
            vec!["db".to_string(), "web".to_string()],
            &["DB".to_string()],
        );
        assert_eq!(options, ["db", "web"], "no second chip for the same tag");
        assert_eq!(
            picked,
            ["db"],
            "the rule is restated in the chip's spelling"
        );

        let mut form = form_from_channel(slack_channel("https://hooks.slack.com/services/T/B/x"));
        form.tag_options = options;
        form.auto_bind_tags = picked;
        let html = ChannelFormPage {
            active_tab: TAB_NOTIFICATIONS,
            form,
        }
        .render()
        .unwrap();
        assert!(html.contains(r#"value="db" class="sr-only" checked"#));
        assert!(html.contains(r#"value="web" class="sr-only">"#));
    }

    #[test]
    fn edit_form_linked_telegram_shows_chat_info_not_inputs() {
        use chrono::Utc;
        let ch = NotificationChannel {
            id: Uuid::nil(),
            name: "Ops Telegram".into(),
            kind: crate::domain::ChannelKind::TelegramApp,
            config: ChannelConfig::TelegramApp(crate::domain::TelegramAppConfig {
                chat_id: "-100123".into(),
                chat_title: Some("Ops".into()),
            }),
            enabled: true,
            disabled_reason: None,
            verified_at: None,
            consecutive_failures: 0,
            failing_since: None,
            last_delivered_at: None,
            write_source: crate::domain::WriteSource::Ui,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            auto_bind_tags: Vec::new(),
        };
        let form = form_from_channel(ch);
        assert_eq!(form.kind, "telegram_app");
        assert_eq!(form.config.telegram_app_chat_id, "-100123");
        let html = ChannelFormPage {
            active_tab: TAB_NOTIFICATIONS,
            form,
        }
        .render()
        .unwrap();
        // The one-tap card stays visible for an already-linked channel even
        // with central_telegram=false, so the edit page renders coherently.
        assert!(html.contains(r#"value="telegram_app""#));
        assert!(html.contains("# linked via the bot"));
        assert!(html.contains("-100123"));
        assert!(html.contains("Ops"));
        // Display-only: no connect button on edit.
        assert!(!html.contains("data-tga-connect"));
    }

    #[test]
    fn create_form_offers_monitor_binding() {
        let mut form = empty_create_form();
        form.bindable = vec![MonitorCard {
            id: "t1".into(),
            name: "api-prod".into(),
            kind: "HTTP",
            addr: "https://api.example.com/health".into(),
            enabled: true,
            managed_by: None,
            tags: String::new(),
            tags_json: "[]".into(),
        }];
        let html = ChannelFormPage {
            active_tab: TAB_NOTIFICATIONS,
            form,
        }
        .render()
        .unwrap();
        assert!(html.contains("data-add-monitor"));
        assert!(html.contains(r#"data-bind-monitor data-target-id="t1""#));
        assert!(html.contains("api-prod"));
        // Search + show-disabled + pager controls ride along for big orgs.
        assert!(html.contains("data-picker-search"));
        assert!(html.contains("data-picker-show-disabled"));
        assert!(html.contains("data-picker-pager"));
    }

    fn slack_channel(webhook_url: &str) -> NotificationChannel {
        use chrono::Utc;
        NotificationChannel {
            id: Uuid::nil(),
            name: "Ops".into(),
            kind: crate::domain::ChannelKind::Slack,
            config: ChannelConfig::Slack(SlackConfig {
                webhook_url: webhook_url.into(),
                mention: None,
            }),
            enabled: true,
            disabled_reason: None,
            verified_at: None,
            consecutive_failures: 0,
            failing_since: None,
            last_delivered_at: None,
            write_source: crate::domain::WriteSource::Ui,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            auto_bind_tags: Vec::new(),
        }
    }

    #[test]
    fn edit_form_prefills_redacted_config_and_replace_toggle() {
        let ch = slack_channel("https://hooks.slack.com/services/T/B/zzUNIQUESECRETzz");
        let form = form_from_channel(ch);
        assert_eq!(form.submit_method, "PATCH");
        assert_eq!(form.mode, "edit");
        // Secret comes back masked, never the real webhook.
        assert_eq!(form.config.slack_webhook_url, "***");
        let html = ChannelFormPage {
            active_tab: TAB_NOTIFICATIONS,
            form,
        }
        .render()
        .unwrap();
        assert!(html.contains("Replace transport config"));
        assert!(html.contains(r#"data-method="PATCH""#));
        assert!(html.contains("data-send-test"));
        assert!(html.contains(
            r#"hx-delete="/api/v1/notification-channels/00000000-0000-0000-0000-000000000000""#
        ));
        // Empty used_by: quiet placeholders in the header and the card.
        assert!(html.contains("# not bound to any monitor"));
        assert!(html.contains("# not bound to any monitor yet"));
        // The real webhook never reaches the browser — only the `***` mask.
        assert!(!html.contains("zzUNIQUESECRETzz"));
        assert!(html.contains(r#"value="***""#));
    }

    /// An empty box would let a save with replace-config on wipe a routing rule
    /// the operator never saw.
    #[test]
    fn edit_form_keeps_the_discord_ping_beside_the_masked_webhook() {
        let mut ch = slack_channel("https://discord.com/api/webhooks/1/zzUNIQUESECRETzz");
        ch.kind = crate::domain::ChannelKind::Discord;
        ch.config = ChannelConfig::Discord(crate::domain::DiscordConfig {
            webhook_url: "https://discord.com/api/webhooks/1/zzUNIQUESECRETzz".into(),
            mention: Some("@here &123456789012345678".into()),
        });
        let form = form_from_channel(ch);
        assert_eq!(form.config.discord_webhook_url, "***");
        assert_eq!(form.config.discord_mention, "@here &123456789012345678");

        let html = ChannelFormPage {
            active_tab: TAB_NOTIFICATIONS,
            form,
        }
        .render()
        .unwrap();
        assert!(
            html.contains("&amp;123456789012345678"),
            "ping is prefilled"
        );
        assert!(!html.contains("zzUNIQUESECRETzz"));
    }

    #[test]
    fn edit_form_shows_platform_disable_note() {
        let mut form = form_from_channel(slack_channel("https://hooks.slack.com/services/T/B/x"));
        form.enabled = false;
        form.disabled_reason = "unlinked from the Telegram side".into();
        let html = ChannelFormPage {
            active_tab: TAB_NOTIFICATIONS,
            form,
        }
        .render()
        .unwrap();
        assert!(html.contains("# disabled by the platform: unlinked from the Telegram side"));

        // No note → no platform line.
        let clean = ChannelFormPage {
            active_tab: TAB_NOTIFICATIONS,
            form: form_from_channel(slack_channel("https://hooks.slack.com/services/T/B/x")),
        }
        .render()
        .unwrap();
        assert!(!clean.contains("# disabled by the platform"));
    }

    #[test]
    fn edit_form_lists_bound_monitors() {
        let mut form = form_from_channel(slack_channel("https://hooks.slack.com/services/T/B/x"));
        form.used_by = vec![
            MonitorCard {
                id: "t1".into(),
                name: "api-prod".into(),
                kind: "HTTP",
                addr: "https://api.example.com/health".into(),
                enabled: true,
                managed_by: None,
                tags: String::new(),
                tags_json: "[]".into(),
            },
            MonitorCard {
                id: "t2".into(),
                name: "old-worker".into(),
                kind: "TCP",
                addr: "10.0.0.5:9000".into(),
                enabled: false,
                managed_by: None,
                tags: String::new(),
                tags_json: "[]".into(),
            },
        ];
        let html = ChannelFormPage {
            active_tab: TAB_NOTIFICATIONS,
            form,
        }
        .render()
        .unwrap();
        assert!(html.contains(r#"href="/targets/t1/edit""#));
        assert!(html.contains("api-prod"));
        assert!(html.contains("http · https://api.example.com/health"));
        assert!(html.contains("old-worker"));
        assert!(html.contains("tcp · 10.0.0.5:9000"));
        assert!(html.contains(r#"<span class="cli-brackets font-normal">disabled</span>"#));
        // Each bound card carries its × unbind affordance.
        assert!(html.contains(r#"data-unbind-monitor data-target-id="t1""#));
        assert!(html.contains(r#"aria-label="unbind api-prod""#));
        // The empty-state note renders hidden so the JS can re-show it after
        // the last unbind.
        assert!(html.contains(r#"data-used-by-note class="font-mono text-xs text-quiet hidden""#));
    }

    #[test]
    fn edit_form_renders_bind_picker() {
        let mut form = form_from_channel(slack_channel("https://hooks.slack.com/services/T/B/x"));
        form.bindable = vec![
            MonitorCard {
                id: "t3".into(),
                name: "staging-api".into(),
                kind: "HTTP",
                addr: "https://staging.example.com".into(),
                enabled: true,
                managed_by: None,
                tags: String::new(),
                tags_json: "[]".into(),
            },
            MonitorCard {
                id: "t4".into(),
                name: "tf-api".into(),
                kind: "HTTP",
                addr: "https://tf.example.com".into(),
                enabled: true,
                managed_by: Some("terraform"),
                tags: String::new(),
                tags_json: "[]".into(),
            },
        ];
        let html = ChannelFormPage {
            active_tab: TAB_NOTIFICATIONS,
            form,
        }
        .render()
        .unwrap();
        assert!(html.contains("data-add-monitor"));
        assert!(html.contains(r#"data-channel-id="00000000-0000-0000-0000-000000000000""#));
        assert!(html.contains(r#"data-bind-monitor data-target-id="t3""#));
        assert!(html.contains("staging-api"));
        // Externally-managed monitors carry their chip — a UI bind may be
        // overwritten on the next apply.
        assert!(html.contains(r#"<span class="cli-brackets font-normal" title="managed externally — changes made here may be overwritten on the next apply">terraform</span>"#));
        // Monitors are available, so the all-bound note starts hidden.
        assert!(
            html.contains(r#"data-picker-allbound class="font-mono text-xs text-quiet hidden""#)
        );
    }

    #[test]
    fn edit_form_bind_picker_empty_states() {
        // Every state renders (the JS toggles them after in-place moves);
        // the server decides which starts visible.
        const NONE_VISIBLE: &str = r#"data-picker-none class="font-mono text-xs text-quiet""#;
        const NONE_HIDDEN: &str = r#"data-picker-none class="font-mono text-xs text-quiet hidden""#;
        const ALLBOUND_VISIBLE: &str =
            r#"data-picker-allbound class="font-mono text-xs text-quiet""#;
        const ALLBOUND_HIDDEN: &str =
            r#"data-picker-allbound class="font-mono text-xs text-quiet hidden""#;

        // No monitors at all → invite to create one.
        let form = form_from_channel(slack_channel("https://hooks.slack.com/services/T/B/x"));
        let html = ChannelFormPage {
            active_tab: TAB_NOTIFICATIONS,
            form,
        }
        .render()
        .unwrap();
        assert!(html.contains("data-add-monitor"));
        assert!(html.contains(NONE_VISIBLE));
        assert!(html.contains(ALLBOUND_HIDDEN));
        assert!(html.contains(r#"href="/targets/new""#));

        // Every monitor already bound → say so instead of an empty grid.
        let mut form = form_from_channel(slack_channel("https://hooks.slack.com/services/T/B/x"));
        form.used_by = vec![MonitorCard {
            id: "t1".into(),
            name: "api-prod".into(),
            kind: "HTTP",
            addr: "https://api.example.com/health".into(),
            enabled: true,
            managed_by: None,
            tags: String::new(),
            tags_json: "[]".into(),
        }];
        let html = ChannelFormPage {
            active_tab: TAB_NOTIFICATIONS,
            form,
        }
        .render()
        .unwrap();
        assert!(html.contains(ALLBOUND_VISIBLE));
        assert!(html.contains(NONE_HIDDEN));
    }

    #[test]
    fn channels_partial_renders_rows_and_empty_state() {
        let empty = ChannelsPartial { channels: vec![] }.render().unwrap();
        assert!(empty.contains("# no channels configured"));
        assert!(!empty.contains("<!doctype html>"));

        let html = ChannelsPartial {
            channels: vec![ChannelRow {
                id: "abc".into(),
                name: "Ops Slack".into(),
                kind: "slack",
                icon: "slack",
                enabled: true,
                unverified: false,
                disabled_reason: String::new(),
                failing_for: None,
                last_delivered: None,
                created: "2026-05-18T12:00:00Z".parse().unwrap(),
                managed_by: None,
            }],
        }
        .render()
        .unwrap();
        assert!(html.contains("Ops Slack"));
        assert!(html.contains(r#"href="/settings/notifications/abc/edit""#));
        assert!(html.contains(r#"hx-post="/api/v1/notification-channels/abc/test""#));
        assert!(html.contains(r#"hx-delete="/api/v1/notification-channels/abc""#));
        assert!(html.contains(r##"<use href="#ci-slack">"##));
    }

    /// A channel the platform turned off has to say so where an operator looks
    /// at the fleet, not only inside its edit form.
    #[test]
    fn channels_partial_shows_why_the_platform_disabled_a_channel() {
        let html = ChannelsPartial {
            channels: vec![ChannelRow {
                id: "abc".into(),
                name: "Ops Slack".into(),
                kind: "slack",
                icon: "slack",
                enabled: false,
                unverified: false,
                disabled_reason: "unlinked from the Telegram side".into(),
                failing_for: None,
                last_delivered: None,
                created: "2026-05-18T12:00:00Z".parse().unwrap(),
                managed_by: None,
            }],
        }
        .render()
        .unwrap();
        assert!(html.contains("# unlinked from the Telegram side"));
    }

    /// A channel nothing lands on stays enabled, so without this the fleet
    /// view reads "enabled" for an endpoint that swallows every alert.
    #[test]
    fn channels_partial_flags_a_channel_that_stopped_delivering() {
        let html = ChannelsPartial {
            channels: vec![ChannelRow {
                id: "abc".into(),
                name: "Ops Slack".into(),
                kind: "slack",
                icon: "slack",
                enabled: true,
                unverified: false,
                disabled_reason: String::new(),
                failing_for: Some("2d 3h".into()),
                last_delivered: Some("2d 3h".into()),
                created: "2026-05-18T12:00:00Z".parse().unwrap(),
                managed_by: None,
            }],
        }
        .render()
        .unwrap();
        assert!(html.contains("not delivering"));
        assert!(html.contains("# nothing delivered for 2d 3h"));
        assert!(html.contains("enabled"));
        assert!(html.contains("alerts are still being sent"));
    }

    mod deduped_create {
        use super::*;
        use crate::domain::{ChannelKind, MAX_CHANNEL_NAME_LEN, OrgId, TelegramAppConfig};
        use crate::error::codes;
        use crate::storage::InMemoryNotificationChannelStore;
        use uuid::Uuid;

        fn org() -> OrgId {
            OrgId(Uuid::from_u128(0xC3))
        }

        fn app_config(chat_id: &str) -> ChannelConfig {
            ChannelConfig::TelegramApp(TelegramAppConfig {
                chat_id: chat_id.into(),
                chat_title: Some("Ops".into()),
            })
        }

        fn no_block_log() -> QuotaBlockLog {
            QuotaBlockLog {
                db: None,
                user: None,
                flow: "test",
            }
        }

        #[tokio::test]
        async fn dedupes_name_with_suffix() {
            let store = InMemoryNotificationChannelStore::new();
            for expected in ["Ops", "Ops 2", "Ops 3"] {
                let ch = create_channel_deduped(
                    &store,
                    org(),
                    "Ops",
                    app_config("-1"),
                    10,
                    no_block_log(),
                )
                .await
                .unwrap();
                assert_eq!(ch.name, expected);
                assert_eq!(ch.kind, ChannelKind::TelegramApp);
                assert!(ch.enabled);
            }
        }

        #[tokio::test]
        async fn quota_error_passes_through() {
            let store = InMemoryNotificationChannelStore::new();
            create_channel_deduped(&store, org(), "Ops", app_config("-1"), 1, no_block_log())
                .await
                .unwrap();
            let err =
                create_channel_deduped(&store, org(), "Other", app_config("-2"), 1, no_block_log())
                    .await
                    .unwrap_err();
            assert!(
                matches!(err, AppError::Unprocessable { code, .. } if code == codes::CHANNEL_QUOTA_EXCEEDED)
            );
        }

        #[test]
        fn name_budget_keeps_room_for_suffix() {
            let long = "x".repeat(MAX_CHANNEL_NAME_LEN + 20);
            let plain = channel_name_with_suffix(&long, None);
            assert_eq!(plain.chars().count(), MAX_CHANNEL_NAME_LEN);
            let suffixed = channel_name_with_suffix(&long, Some(12));
            assert_eq!(suffixed.chars().count(), MAX_CHANNEL_NAME_LEN);
            assert!(suffixed.ends_with(" 12"));
            assert_eq!(channel_name_with_suffix("  Ops  ", Some(2)), "Ops 2");
        }
    }
}
