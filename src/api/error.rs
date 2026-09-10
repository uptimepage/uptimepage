use serde::Serialize;
use utoipa::ToSchema;

/// Top-level envelope returned for every 4xx/5xx response.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApiError {
    pub error: ApiErrorBody,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApiErrorBody {
    /// Stable machine-readable code (UPPER_SNAKE_CASE).
    #[schema(example = "INVALID_URL_SCHEME")]
    pub code: &'static str,
    /// Human-readable, safe to display.
    #[schema(example = "URL scheme must be http or https.")]
    pub message: String,
    /// JSON pointer to the offending field for 400s.
    #[schema(example = "check.url", nullable = true)]
    pub field: Option<String>,
    /// Optional structured context.
    #[schema(nullable = true)]
    pub details: Option<serde_json::Value>,
    /// W3C traceparent for support.
    #[schema(example = "00-7c3a4f...-01", nullable = true)]
    pub trace_id: Option<String>,
}

impl ApiErrorBody {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            field: None,
            details: None,
            trace_id: None,
        }
    }

    pub fn with_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }
}

/// Stable error codes returned by the API. Adding new codes is non-breaking;
/// renaming or repurposing existing ones is breaking.
pub mod codes {
    pub const INVALID_JSON: &str = "INVALID_JSON";
    pub const INVALID_CONTENT_TYPE: &str = "INVALID_CONTENT_TYPE";
    pub const MISSING_FIELD: &str = "MISSING_FIELD";
    pub const INVALID_URL_SCHEME: &str = "INVALID_URL_SCHEME";
    pub const INVALID_URL_FORMAT: &str = "INVALID_URL_FORMAT";
    pub const SSRF_BLOCKED: &str = "SSRF_BLOCKED";
    pub const INVALID_INTERVAL: &str = "INVALID_INTERVAL";
    pub const INVALID_TIMEOUT: &str = "INVALID_TIMEOUT";
    pub const INVALID_TCP_PORT: &str = "INVALID_TCP_PORT";
    pub const INVALID_TCP_HOST: &str = "INVALID_TCP_HOST";
    pub const INVALID_PING_HOST: &str = "INVALID_PING_HOST";
    pub const INVALID_HEARTBEAT_PARAMS: &str = "INVALID_HEARTBEAT_PARAMS";
    pub const HEARTBEAT_NOT_PROBEABLE: &str = "HEARTBEAT_NOT_PROBEABLE";
    pub const HEARTBEAT_NOT_CONFIGURED: &str = "HEARTBEAT_NOT_CONFIGURED";
    pub const INVALID_STATUS_RANGE: &str = "INVALID_STATUS_RANGE";
    pub const INVALID_HTTP_METHOD: &str = "INVALID_HTTP_METHOD";
    pub const INVALID_HTTP_PARAMS: &str = "INVALID_HTTP_PARAMS";
    pub const INVALID_HEAD_BODY_MATCH: &str = "INVALID_HEAD_BODY_MATCH";
    pub const INVALID_TLS_CERT_PARAMS: &str = "INVALID_TLS_CERT_PARAMS";
    pub const INVALID_DOMAIN_PARAMS: &str = "INVALID_DOMAIN_PARAMS";
    pub const INVALID_DNS_PARAMS: &str = "INVALID_DNS_PARAMS";
    pub const INVALID_FLOW_PARAMS: &str = "INVALID_FLOW_PARAMS";
    pub const FLOW_CHECKS_DISABLED: &str = "FLOW_CHECKS_DISABLED";
    pub const SMS_ALERTS_DISABLED: &str = "SMS_ALERTS_DISABLED";
    pub const ON_CALL_DISABLED: &str = "ON_CALL_DISABLED";
    pub const PLAN_HOLD: &str = "PLAN_HOLD";
    pub const ACCOUNT_OWNER_REQUIRED: &str = "ACCOUNT_OWNER_REQUIRED";
    pub const NO_FLOW_CAPABLE_AGENT: &str = "NO_FLOW_CAPABLE_AGENT";
    pub const INVALID_TLS_CRED_COMBO: &str = "INVALID_TLS_CRED_COMBO";
    pub const INVALID_ALERT_CONFIG: &str = "INVALID_ALERT_CONFIG";
    pub const INVALID_REGION_POLICY: &str = "INVALID_REGION_POLICY";
    pub const INVALID_CONFIG: &str = "INVALID_CONFIG";
    pub const UNRESOLVED_VARIABLE: &str = "UNRESOLVED_VARIABLE";
    pub const INVALID_VARIABLE_KEY: &str = "INVALID_VARIABLE_KEY";
    pub const VARIABLE_KEY_EXISTS: &str = "VARIABLE_KEY_EXISTS";
    pub const VARIABLE_IN_USE: &str = "VARIABLE_IN_USE";
    pub const VARIABLE_NOT_FOUND: &str = "VARIABLE_NOT_FOUND";
    pub const EMPTY_NAME: &str = "EMPTY_NAME";
    pub const INVALID_NAME: &str = "INVALID_NAME";
    pub const REDACTION_SENTINEL: &str = "REDACTION_SENTINEL";
    pub const BULK_EMPTY: &str = "BULK_EMPTY";
    pub const BULK_VALIDATION: &str = "BULK_VALIDATION";
    pub const BULK_TOO_LARGE: &str = "BULK_TOO_LARGE";
    pub const BAD_TIME_RANGE: &str = "BAD_TIME_RANGE";
    pub const INVALID_CURSOR: &str = "INVALID_CURSOR";
    pub const TARGET_NOT_FOUND: &str = "TARGET_NOT_FOUND";
    pub const TARGET_DUPLICATE: &str = "TARGET_DUPLICATE";
    pub const INCIDENT_ALREADY_OPEN: &str = "INCIDENT_ALREADY_OPEN";
    pub const CIRCUIT_OPEN: &str = "CIRCUIT_OPEN";
    /// No probe available to run the check; probing runs on agents (503).
    pub const PROBE_UNAVAILABLE: &str = "PROBE_UNAVAILABLE";
    pub const DEPENDENCY_DOWN: &str = "DEPENDENCY_DOWN";
    pub const RATE_LIMITED: &str = "RATE_LIMITED";
    /// A resource quota would be exceeded; `details.quota` names which.
    pub const QUOTA_EXCEEDED: &str = "QUOTA_EXCEEDED";
    /// Requested check interval is below the plan minimum.
    pub const MIN_CHECK_INTERVAL: &str = "MIN_CHECK_INTERVAL";
    /// Request blocked by abuse protection; `field` points at the target URL.
    pub const ABUSE_BLOCKED: &str = "ABUSE_BLOCKED";
    /// Specifically: target URL matched an abuse reconnaissance pattern.
    pub const URL_PATTERN_BLOCKED: &str = "URL_PATTERN_BLOCKED";
    /// Specifically: target domain (or a parent) is on the deny-list.
    pub const DOMAIN_DENYLISTED: &str = "DOMAIN_DENYLISTED";
    pub const INTERNAL: &str = "INTERNAL";
    pub const UNAUTHORIZED: &str = "UNAUTHORIZED";
    /// A valid API token reached an endpoint gated to browser sessions (org
    /// lifecycle). Distinct from `UNAUTHORIZED` so token users aren't misled
    /// into thinking their token is invalid (401).
    pub const SESSION_REQUIRED: &str = "SESSION_REQUIRED";
    pub const FORBIDDEN: &str = "FORBIDDEN";
    /// API token lacks the scope its target endpoint requires (403).
    pub const INSUFFICIENT_SCOPE: &str = "INSUFFICIENT_SCOPE";
    /// Agent result batch exceeds the per-request cap (422). Skewed/foreign
    /// rows are dropped per-row, not rejected, so they need no code.
    pub const AGENT_BATCH_TOO_LARGE: &str = "AGENT_BATCH_TOO_LARGE";
    // Maintenance + incident narration (operator surface).
    pub const INVALID_TIME_RANGE: &str = "INVALID_TIME_RANGE";
    pub const INVALID_COMPONENT_ID: &str = "INVALID_COMPONENT_ID";
    pub const INVALID_DURATION: &str = "INVALID_DURATION";
    pub const EMPTY_TITLE: &str = "EMPTY_TITLE";
    pub const TITLE_TOO_LONG: &str = "TITLE_TOO_LONG";
    pub const DESCRIPTION_TOO_LONG: &str = "DESCRIPTION_TOO_LONG";
    pub const GROUP_TOO_LONG: &str = "GROUP_TOO_LONG";
    pub const TAG_TOO_LONG: &str = "TAG_TOO_LONG";
    pub const TOO_MANY_TAGS: &str = "TOO_MANY_TAGS";
    pub const INVALID_TAG: &str = "INVALID_TAG";
    pub const OWNER_NOT_MEMBER: &str = "OWNER_NOT_MEMBER";
    pub const EMPTY_MESSAGE: &str = "EMPTY_MESSAGE";
    pub const MESSAGE_TOO_LONG: &str = "MESSAGE_TOO_LONG";
    pub const INVALID_PHASE: &str = "INVALID_PHASE";
    pub const MAINTENANCE_COMPLETED: &str = "MAINTENANCE_COMPLETED";
    pub const MAINTENANCE_NOT_FOUND: &str = "MAINTENANCE_NOT_FOUND";
    pub const INCIDENT_NOT_FOUND: &str = "INCIDENT_NOT_FOUND";
    pub const INCIDENT_INVALID_STATE: &str = "INCIDENT_INVALID_STATE";
    pub const INCIDENT_DOWNTIME_NOT_EDITABLE: &str = "INCIDENT_DOWNTIME_NOT_EDITABLE";
    pub const POSTMORTEM_NOT_FOUND: &str = "POSTMORTEM_NOT_FOUND";
    pub const ASSIGNEE_NOT_MEMBER: &str = "ASSIGNEE_NOT_MEMBER";
    pub const INVALID_SEVERITY: &str = "INVALID_SEVERITY";
    // Escalation policies (owner config surface).
    pub const ESCALATION_POLICY_NOT_FOUND: &str = "ESCALATION_POLICY_NOT_FOUND";
    pub const ESCALATION_POLICY_NAME_TAKEN: &str = "ESCALATION_POLICY_NAME_TAKEN";
    pub const ESCALATION_POLICY_QUOTA_EXCEEDED: &str = "ESCALATION_POLICY_QUOTA_EXCEEDED";
    pub const ESCALATION_POLICY_INVALID: &str = "ESCALATION_POLICY_INVALID";
    // On-call schedules (owner config surface).
    pub const ON_CALL_SCHEDULE_NOT_FOUND: &str = "ON_CALL_SCHEDULE_NOT_FOUND";
    pub const ON_CALL_SCHEDULE_NAME_TAKEN: &str = "ON_CALL_SCHEDULE_NAME_TAKEN";
    pub const ON_CALL_SCHEDULE_QUOTA_EXCEEDED: &str = "ON_CALL_SCHEDULE_QUOTA_EXCEEDED";
    pub const ON_CALL_SCHEDULE_INVALID: &str = "ON_CALL_SCHEDULE_INVALID";
    pub const ON_CALL_OVERRIDE_NOT_FOUND: &str = "ON_CALL_OVERRIDE_NOT_FOUND";
    pub const CONTACT_CHANNEL_INVALID: &str = "CONTACT_CHANNEL_INVALID";
    // Notification channels (operator surface).
    pub const CHANNEL_NOT_FOUND: &str = "CHANNEL_NOT_FOUND";
    pub const CHANNEL_NAME_TAKEN: &str = "CHANNEL_NAME_TAKEN";
    pub const CHANNEL_NAME_INVALID: &str = "CHANNEL_NAME_INVALID";
    pub const CHANNEL_QUOTA_EXCEEDED: &str = "CHANNEL_QUOTA_EXCEEDED";
    pub const INVALID_CHANNEL_CONFIG: &str = "INVALID_CHANNEL_CONFIG";
    pub const CHANNEL_TEST_FAILED: &str = "CHANNEL_TEST_FAILED";
    pub const CHANNEL_KIND_MANAGED: &str = "CHANNEL_KIND_MANAGED";
    pub const CHANNEL_UNVERIFIED: &str = "CHANNEL_UNVERIFIED";
    pub const CHANNEL_NOT_VERIFIABLE: &str = "CHANNEL_NOT_VERIFIABLE";
    pub const CHANNEL_ALREADY_VERIFIED: &str = "CHANNEL_ALREADY_VERIFIED";
    pub const CHANNEL_VERIFICATION_LIMIT: &str = "CHANNEL_VERIFICATION_LIMIT";
    pub const EMAIL_DESTINATION_BLOCKED: &str = "EMAIL_DESTINATION_BLOCKED";
    pub const TELEGRAM_LINK_LIMIT: &str = "TELEGRAM_LINK_LIMIT";
    pub const TELEGRAM_LINK_NOT_FOUND: &str = "TELEGRAM_LINK_NOT_FOUND";
    pub const WHATSAPP_LINK_LIMIT: &str = "WHATSAPP_LINK_LIMIT";
    pub const WHATSAPP_LINK_NOT_FOUND: &str = "WHATSAPP_LINK_NOT_FOUND";
    pub const DELEGATE_LINK_LIMIT: &str = "DELEGATE_LINK_LIMIT";
    pub const DELEGATE_LINK_NOT_FOUND: &str = "DELEGATE_LINK_NOT_FOUND";
    pub const DELEGATE_KIND_INVALID: &str = "DELEGATE_KIND_INVALID";
    // In-app help form.
    pub const SUPPORT_UNAVAILABLE: &str = "SUPPORT_UNAVAILABLE";
    pub const SUPPORT_DELIVERY_FAILED: &str = "SUPPORT_DELIVERY_FAILED";
    pub const INVALID_TOPIC: &str = "INVALID_TOPIC";
    // Org management.
    pub const ORG_NOT_FOUND: &str = "ORG_NOT_FOUND";
    pub const ORG_DELETED: &str = "ORG_DELETED";
    pub const SLUG_TAKEN: &str = "SLUG_TAKEN";
    pub const SLUG_INVALID: &str = "SLUG_INVALID";
    pub const STATUS_PAGE_NOT_FOUND: &str = "STATUS_PAGE_NOT_FOUND";
    /// Share link unknown, revoked, expired, or its monitor/org gone. One
    /// opaque code for every miss so the public `/m/{token}` surface gives no
    /// enumeration signal.
    pub const SHARE_NOT_FOUND: &str = "SHARE_NOT_FOUND";
    /// Share label blank-after-trim or longer than 80 chars.
    pub const SHARE_LABEL_INVALID: &str = "SHARE_LABEL_INVALID";
    /// POST a component that is already curated onto the page — edit via PATCH.
    pub const COMPONENT_ALREADY_ON_PAGE: &str = "COMPONENT_ALREADY_ON_PAGE";
    /// PATCH with no mutable field set — rejected so a no-op isn't silently 200.
    pub const EMPTY_PATCH: &str = "EMPTY_PATCH";
    pub const OWNER_ORG_LIMIT: &str = "OWNER_ORG_LIMIT";
    pub const NOT_AN_OWNER: &str = "NOT_AN_OWNER";
    pub const MEMBER_NOT_FOUND: &str = "MEMBER_NOT_FOUND";
    pub const LAST_OWNER: &str = "LAST_OWNER";
    /// The member being removed owns the account the org bills to.
    pub const ACCOUNT_OWNER: &str = "ACCOUNT_OWNER";
    /// Deleting this org would leave the caller with no org to sign in to.
    pub const LAST_ORG: &str = "LAST_ORG";
    pub const RESTORE_WINDOW_EXPIRED: &str = "RESTORE_WINDOW_EXPIRED";
    // Auth.
    pub const INVALID_STATE: &str = "INVALID_STATE";
    pub const SESSION_NOT_FOUND: &str = "SESSION_NOT_FOUND";
    pub const INVALID_TOKEN: &str = "INVALID_TOKEN";
    pub const ORG_REQUIRED: &str = "ORG_REQUIRED";
    pub const ORG_HEADER_INVALID: &str = "ORG_HEADER_INVALID";
    /// Org-bound token addressed a different org (via header or path) than the
    /// one it is pinned to (403).
    pub const ORG_HEADER_MISMATCH: &str = "ORG_HEADER_MISMATCH";
    pub const EMAIL_NOT_VERIFIED: &str = "EMAIL_NOT_VERIFIED";
    pub const TOKEN_LIMIT: &str = "TOKEN_LIMIT";
    pub const TOKEN_NAME_INVALID: &str = "TOKEN_NAME_INVALID";
    pub const TOKEN_NOT_FOUND: &str = "TOKEN_NOT_FOUND";
    pub const INVALID_SCOPES: &str = "INVALID_SCOPES";
    pub const INVALID_EXPIRY: &str = "INVALID_EXPIRY";
    // Invitations.
    pub const ALREADY_MEMBER: &str = "ALREADY_MEMBER";
    pub const ALREADY_INVITED: &str = "ALREADY_INVITED";
    pub const INVITATION_INVALID: &str = "INVITATION_INVALID";
    pub const INVITATION_EMAIL_MISMATCH: &str = "INVITATION_EMAIL_MISMATCH";
    pub const INVITATIONS_LIMIT: &str = "INVITATIONS_LIMIT";
    /// Distinct from `INVITATIONS_LIMIT`: that one clears by revoking a
    /// pending invitation, this one only by waiting.
    pub const INVITATION_SEND_LIMIT: &str = "INVITATION_SEND_LIMIT";
    pub const INVALID_ROLE: &str = "INVALID_ROLE";
    pub const INVALID_EMAIL: &str = "INVALID_EMAIL";
    pub const CSRF_PROTECTION: &str = "CSRF_PROTECTION";
    pub const FINGERPRINT_SALT_ROTATED: &str = "FINGERPRINT_SALT_ROTATED";
    // GDPR account export / deletion.
    /// Deletion blocked: caller is the sole owner of an org that still has
    /// other members. `details.orgs` lists the blocking orgs (slug + id).
    pub const OWNS_SHARED_ORGS: &str = "OWNS_SHARED_ORGS";
    /// A second deletion of an already-soft-deleted account.
    pub const ACCOUNT_ALREADY_DELETED: &str = "ACCOUNT_ALREADY_DELETED";
    pub const ACCOUNT_NOT_DELETED: &str = "ACCOUNT_NOT_DELETED";
    // Operator (instance-admin) surface — regions + agents.
    /// Operator surface addressed but `operator.admin_token` is unset (404 —
    /// the surface is invisible when disabled).
    pub const OPERATOR_DISABLED: &str = "OPERATOR_DISABLED";
    pub const REGION_NOT_FOUND: &str = "REGION_NOT_FOUND";
    pub const REGION_EXISTS: &str = "REGION_EXISTS";
    pub const REGION_INVALID: &str = "REGION_INVALID";
    /// Region still referenced by an agent or a target assignment (409).
    pub const REGION_IN_USE: &str = "REGION_IN_USE";
    pub const AGENT_NOT_FOUND: &str = "AGENT_NOT_FOUND";
    pub const AGENT_INVALID: &str = "AGENT_INVALID";
    // Public status-page settings + logo upload.
    pub const BRANDING_INVALID: &str = "BRANDING_INVALID";
    pub const LOGO_MISSING: &str = "LOGO_MISSING";
    pub const LOGO_TYPE_INVALID: &str = "LOGO_TYPE_INVALID";
    pub const LOGO_TOO_LARGE: &str = "LOGO_TOO_LARGE";
    pub const LOGO_DECODE_FAILED: &str = "LOGO_DECODE_FAILED";
}
