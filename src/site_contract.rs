//! `site-contract.json`: the facts the private site's tests check its pages
//! against, generated from the code that owns them. The site pins a commit of
//! this repository and reads that commit's copy, so it never parses this
//! crate's source.

use serde_json::{Value, json};

use crate::config::{
    CHECK_RESULTS_HISTORY_MONTHS, CHECK_RESULTS_RAW_DAYS, FLOW_EVIDENCE_DAYS, HttpClientConfig,
    RetentionConfig, SessionConfig, TenancyConfig,
};
use crate::request::theme;
use crate::security::app_link::PUSHOVER_OFFER_COOLDOWN;

const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/site-contract.json");

fn contract() -> Value {
    let retention = RetentionConfig::default();
    let mut mcp_tools = crate::mcp::tool_names();
    mcp_tools.sort();
    let user_agent = HttpClientConfig {
        tcp_keepalive_secs: 0,
        bot_url: "https://bot.invalid".into(),
        user_agent: String::new(),
    }
    .effective_user_agent()
    .replace("https://bot.invalid", "{bot_url}");
    json!({
        "product_version": env!("CARGO_PKG_VERSION"),
        "user_agent_template": user_agent,
        "dark_app_themes": theme::dark_app_themes(),
        "theme_cookies": {
            "app": theme::COOKIE_NAME,
            "pick": theme::PICK_COOKIE_NAME,
        },
        "mcp_tools": mcp_tools,
        "retention_days": {
            "check_results_raw": CHECK_RESULTS_RAW_DAYS,
            "flow_evidence": FLOW_EVIDENCE_DAYS,
            "login_attempts": retention.login_attempts_days,
            "quota_events": retention.quota_events_days,
            "audit_log": retention.audit_log_days,
            "mcp_audit": retention.mcp_audit_days,
            "private_page_access": retention.page_access_events_days,
            "session_max": SessionConfig::default().absolute_timeout_days,
            "link_codes": PUSHOVER_OFFER_COOLDOWN.num_days() + 1,
            "account_deletion_grace": TenancyConfig::default().deletion_grace_period_days,
        },
        "retention_months": {
            "check_results_history": CHECK_RESULTS_HISTORY_MONTHS,
        },
    })
}

#[test]
fn site_contract_json_is_current() {
    let want = serde_json::to_string_pretty(&contract()).expect("serialise") + "\n";
    if std::env::var_os("UPDATE_SITE_CONTRACT").is_some() {
        std::fs::write(PATH, &want).expect("write site-contract.json");
        return;
    }
    let have = std::fs::read_to_string(PATH).unwrap_or_default();
    assert_eq!(
        have, want,
        "site-contract.json is stale. Regenerate it with:\n  \
         UPDATE_SITE_CONTRACT=1 cargo nextest run --lib -E 'test(site_contract)'\n"
    );
}
