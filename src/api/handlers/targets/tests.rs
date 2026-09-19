use super::dispatch::scrub_secrets;
use super::validate::validate_heartbeat_cadence;
use super::*;

#[test]
fn a_tag_is_bounded_in_length_count_and_content() {
    use crate::domain::target::{MAX_TAG_LEN, MAX_TAGS_PER_TARGET};

    assert_eq!(
        normalize_tags(&["prod".into(), "eu west".into()]).unwrap(),
        vec!["prod".to_string(), "eu west".to_string()]
    );
    assert!(normalize_tags(&[]).unwrap().is_empty());

    let too_many: Vec<String> = (0..=MAX_TAGS_PER_TARGET).map(|i| format!("t{i}")).collect();
    assert_eq!(
        code_of(normalize_tags(&too_many).unwrap_err()),
        codes::TOO_MANY_TAGS
    );
    assert_eq!(
        code_of(normalize_tags(&["x".repeat(MAX_TAG_LEN + 1)]).unwrap_err()),
        codes::TAG_TOO_LONG
    );
    normalize_tags(&["x".repeat(MAX_TAG_LEN)]).unwrap();
    assert_eq!(
        code_of(normalize_tags(&["  ".into()]).unwrap_err()),
        codes::INVALID_TAG
    );
    // A tag rides into confirmation prompts and terminals.
    assert_eq!(
        code_of(normalize_tags(&["prod\u{202e}etc".into()]).unwrap_err()),
        codes::INVALID_TAG
    );
}

#[test]
fn a_tag_error_points_at_the_tag_not_the_list() {
    use crate::domain::target::MAX_TAG_LEN;

    let err = normalize_tags(&["ok".into(), "  ".into()]).unwrap_err();
    let AppError::BadRequest { message, field, .. } = err else {
        panic!("expected a bad request");
    };
    assert_eq!(field.as_deref(), Some("tags"));
    // Position, since an invisible character is invisible in the message.
    assert!(message.contains("tag 2"), "{message}");

    let err = normalize_tags(&["fine".into(), "x".repeat(MAX_TAG_LEN + 1)]).unwrap_err();
    assert!(format!("{err}").contains("tag 2"), "{err}");
}

/// Trim and de-duplicate before the count, so a list that stores small is
/// not refused for the shape it arrived in.
#[test]
fn tags_are_normalized_the_same_way_at_every_door() {
    use crate::domain::target::{MAX_TAG_LEN, MAX_TAGS_PER_TARGET};

    assert_eq!(
        normalize_tags(&[" prod ".into(), "prod".into(), "api".into()]).unwrap(),
        vec!["prod".to_string(), "api".to_string()]
    );
    let padded = format!("  {}  ", "x".repeat(MAX_TAG_LEN));
    assert_eq!(
        normalize_tags(&[padded]).unwrap(),
        vec!["x".repeat(MAX_TAG_LEN)]
    );
    let dupes: Vec<String> = (0..=MAX_TAGS_PER_TARGET).map(|_| "prod".into()).collect();
    assert_eq!(normalize_tags(&dupes).unwrap(), vec!["prod".to_string()]);
    // Case is meaning: `Prod` is not `prod`.
    assert_eq!(
        normalize_tags(&["Prod".into(), "prod".into()])
            .unwrap()
            .len(),
        2
    );
}

fn code_of(err: AppError) -> &'static str {
    match err {
        AppError::BadRequest { code, .. } => code,
        other => panic!("expected a bad request, got {other:?}"),
    }
}

fn heartbeat_spec(period_s: u64, grace_s: u64) -> CheckSpec {
    CheckSpec::Heartbeat(crate::domain::HeartbeatCheck {
        period: std::time::Duration::from_secs(period_s),
        grace: std::time::Duration::from_secs(grace_s),
        max_runtime: None,
    })
}

#[test]
fn an_interval_coarser_than_the_evaluation_cadence_is_refused() {
    let secs = std::time::Duration::from_secs;
    // A 2100s window is judged every 210s.
    let spec = heartbeat_spec(900, 1200);
    let err = validate_heartbeat_cadence(&spec, secs(300), 60)
        .expect_err("300s is coarser than the 210s cadence");
    assert!(format!("{err:?}").contains("coarser than the evaluation cadence"));
    validate_heartbeat_cadence(&spec, secs(210), 60).unwrap();
    validate_heartbeat_cadence(&spec, secs(60), 60).unwrap();

    // A small window floors the cadence at a minute.
    validate_heartbeat_cadence(&heartbeat_spec(60, 60), secs(120), 60)
        .expect_err("120s cannot judge a 120s window in time");
    validate_heartbeat_cadence(&heartbeat_spec(60, 60), secs(60), 60).unwrap();

    // A plan floor above the cadence is allowed, not refused as too coarse.
    validate_heartbeat_cadence(&heartbeat_spec(300, 180), secs(180), 180).unwrap();
    validate_heartbeat_cadence(&heartbeat_spec(300, 180), secs(300), 180)
        .expect_err("300s is above both the cadence and the floor");

    // A day-long window caps at five minutes, and other kinds are not judged.
    validate_heartbeat_cadence(&heartbeat_spec(86_400, 3_600), secs(300), 60).unwrap();
}

#[test]
fn the_form_default_pairs_with_the_default_interval() {
    let hb = crate::web::views::targets_form::HeartbeatFields::default();
    let interval = crate::domain::interval_hints_for_kind("heartbeat").default;
    validate_heartbeat_cadence(
        &heartbeat_spec(hb.period_s, hb.grace_s),
        std::time::Duration::from_secs(interval),
        60,
    )
    .unwrap();
}

#[test]
fn scrub_secrets_redacts_echoed_values_in_body_and_headers() {
    use crate::ad_hoc_dispatch::DeliveredResult;
    use crate::domain::CheckResult;
    use crate::domain::agent_wire::HeaderPreview;

    let mut delivered = DeliveredResult {
        result: CheckResult::error(Uuid::nil(), Uuid::nil(), "x"),
        response_body_snippet: Some("{\"echo\":\"sk-live-secret\"}".into()),
        response_headers_preview: vec![HeaderPreview {
            name: "x-echo".into(),
            value: "sk-live-secret".into(),
        }],
        flow_evidence: None,
        flow_steps: vec![],
    };
    scrub_secrets(&mut delivered, &["sk-live-secret".to_string()]);
    assert_eq!(
        delivered.response_body_snippet.as_deref(),
        Some("{\"echo\":\"***\"}")
    );
    assert_eq!(delivered.response_headers_preview[0].value, "***");
}

// Every evidence field is a place a just-typed secret can come back.
#[test]
fn scrub_secrets_reaches_every_field_of_flow_evidence() {
    use crate::ad_hoc_dispatch::DeliveredResult;
    use crate::domain::CheckResult;
    use crate::domain::agent_wire::{ConsoleLine, FlowEvidence};

    let secret = "sk-live-secret";
    let mut delivered = DeliveredResult {
        result: CheckResult::error(Uuid::nil(), Uuid::nil(), "x"),
        response_body_snippet: None,
        response_headers_preview: vec![],
        flow_evidence: Some(FlowEvidence {
            final_url: Some(format!("https://app.example.com/login?t={secret}")),
            title: Some(format!("Sign in {secret}")),
            text_snippet: Some(format!("rejected {secret}")),
            console: vec![ConsoleLine {
                level: "error".into(),
                text: format!("auth failed for {secret}"),
            }],
        }),
        flow_steps: vec![],
    };
    scrub_secrets(&mut delivered, &[secret.to_string()]);

    let ev = delivered.flow_evidence.unwrap();
    let seen = [
        ev.final_url.unwrap(),
        ev.title.unwrap(),
        ev.text_snippet.unwrap(),
        ev.console[0].text.clone(),
    ];
    for field in seen {
        assert!(!field.contains(secret), "secret survived in {field:?}");
        assert!(field.contains("***"), "no redaction marker in {field:?}");
    }
}

#[test]
fn secret_values_skips_short_and_plain() {
    use crate::domain::ResolvedVar;
    let mut vars = crate::domain::VarMap::new();
    vars.insert(
        "k".into(),
        ResolvedVar {
            value: "sk-live-secret".into(),
            is_secret: true,
        },
    );
    vars.insert(
        "short".into(),
        ResolvedVar {
            value: "ab".into(),
            is_secret: true,
        },
    );
    vars.insert(
        "plain".into(),
        ResolvedVar {
            value: "api.example.com".into(),
            is_secret: false,
        },
    );
    let secrets = crate::security::redaction::secret_values(&vars);
    assert_eq!(secrets, vec!["sk-live-secret".to_string()]);
}

fn assert_bad_request_with_field(err: AppError, expected_field: &str) {
    match err {
        AppError::BadRequest { field: Some(f), .. } => assert_eq!(f, expected_field),
        other => panic!("expected BadRequest with field={expected_field}, got {other:?}"),
    }
}

fn head_spec(body_match: Option<&str>) -> crate::domain::CheckSpec {
    use crate::domain::{CheckSpec, ExpectedStatus, HttpCheck, HttpMethod};
    use std::time::Duration;
    CheckSpec::Http(HttpCheck {
        url: "https://example.com/".parse().unwrap(),
        method: HttpMethod::Head,
        timeout: Duration::from_secs(5),
        follow_redirects: false,
        max_redirects: 0,
        expected_status: ExpectedStatus::Exact(200),
        expected_body_contains: body_match.map(str::to_owned),
        headers: Default::default(),
        body: None,
        verify_tls: true,
        basic_auth: None,
        bearer_token: None,
    })
}

fn plan_with_flow(max_checks: i32, max_steps: i32) -> crate::domain::Plan {
    let mut p = crate::quotas::service::unlimited_plan();
    p.max_flow_checks = max_checks;
    p.max_flow_steps = max_steps;
    p
}

fn flow_of(steps: usize) -> CheckSpec {
    CheckSpec::Flow(crate::domain::FlowCheck {
        start_url: url::Url::parse("https://example.com/login").unwrap(),
        steps: (0..steps)
            .map(|_| crate::domain::FlowStep::AssertUrl {
                contains: "/x".into(),
            })
            .collect(),
        timeout: std::time::Duration::from_secs(30),
        step_timeout: std::time::Duration::from_secs(10),
        verify_tls: true,
    })
}

#[test]
fn a_flow_longer_than_the_plan_allows_is_refused() {
    let plan = plan_with_flow(5, 3);
    gate_flow(&flow_of(3), &plan).expect("at the cap is fine");
    let err = gate_flow(&flow_of(4), &plan).expect_err("over the cap must reject");
    assert_bad_request_with_field(err, "check.steps");
}

#[test]
fn a_plan_cannot_buy_more_steps_than_the_engine_runs() {
    // The whole-run budget is what a longer journey exhausts, and no plan
    // column can buy more of it.
    let plan = plan_with_flow(5, 500);
    let over = crate::domain::FlowCheck::MAX_STEPS + 1;
    let err = gate_flow(&flow_of(over), &plan).expect_err("engine ceiling still applies");
    assert_bad_request_with_field(err, "check.steps");
}

#[test]
fn an_edit_keeps_the_step_cap_but_not_the_capability_check() {
    let downgraded = plan_with_flow(0, 3);
    gate_flow(&flow_of(3), &downgraded).expect_err("a new flow is refused outright");
    gate_flow_steps(&flow_of(3), &downgraded).expect("an existing flow can still be edited");
    let err = gate_flow_steps(&flow_of(4), &downgraded).expect_err("but not grown past the plan");
    assert_bad_request_with_field(err, "check.steps");
}

#[test]
fn a_non_flow_check_ignores_the_flow_limits() {
    gate_flow(&head_spec(None), &plan_with_flow(0, 1)).expect("http is not gated by flow caps");
}

#[test]
fn validate_check_rejects_head_with_body_match() {
    use crate::security::ssrf::SsrfGuard;
    let err = validate_check(&head_spec(Some("hello")), &SsrfGuard::strict())
        .expect_err("HEAD + body match must reject");
    assert_bad_request_with_field(err, "check.expected_body_contains");
}

#[test]
fn validate_check_accepts_head_without_body_match() {
    use crate::security::ssrf::SsrfGuard;
    validate_check(&head_spec(None), &SsrfGuard::strict())
        .expect("HEAD without body match must pass");
}

// An import leaves a row blank when the recording had no usable selector,
// so authors meet this. Without a row number they hunt through thirty steps.
#[test]
fn validate_check_names_the_step_with_the_empty_selector() {
    use crate::security::ssrf::SsrfGuard;
    let spec: crate::domain::CheckSpec = serde_json::from_str(
        r##"{"type":"flow","start_url":"https://app.example.com/login",
                "steps":[
                  {"op":"fill","selector":"#user","value":"bob"},
                  {"op":"assert_url","contains":"/home"},
                  {"op":"click","selector":"  "}
                ],
                "timeout":30000,"step_timeout":5000,"verify_tls":true}"##,
    )
    .expect("spec must deserialize");
    let err =
        validate_check(&spec, &SsrfGuard::strict()).expect_err("a blank selector must reject");
    let msg = format!("{err:?}");
    assert!(msg.contains("step 3"), "message must name the row: {msg}");
    assert!(
        msg.contains("selector"),
        "message must name the field: {msg}"
    );
}

#[test]
fn validate_check_rejects_tcp_empty_port() {
    use crate::security::ssrf::SsrfGuard;
    let spec: crate::domain::CheckSpec = serde_json::from_str(
        r#"{"type":"tcp","host":"db.example.com","port":null,"timeout":3000}"#,
    )
    .expect("empty port must deserialize, not 422");
    let err = validate_check(&spec, &SsrfGuard::strict())
        .expect_err("tcp check with empty port must reject");
    assert_bad_request_with_field(err, "check.port");
}

#[test]
fn validate_check_rejects_tls_cert_empty_port() {
    use crate::security::ssrf::SsrfGuard;
    let spec: crate::domain::CheckSpec = serde_json::from_str(
            r#"{"type":"tls_cert","host":"example.com","port":null,"warn_days":14,"critical_days":3,"timeout":5000}"#,
        )
        .expect("empty port must deserialize, not 422");
    let err = validate_check(&spec, &SsrfGuard::strict())
        .expect_err("tls_cert check with empty port must reject");
    assert_bad_request_with_field(err, "check.port");
}

#[test]
fn validate_check_rejects_flow_without_assertion() {
    use crate::security::ssrf::SsrfGuard;
    let spec: crate::domain::CheckSpec = serde_json::from_str(
            r##"{"type":"flow","start_url":"https://app.example.com/login","steps":[{"op":"fill","selector":"#pw","value":"x"},{"op":"click","selector":"#submit"}],"timeout":30000,"step_timeout":5000,"verify_tls":true}"##,
        )
        .expect("flow must deserialize");
    let err = validate_check(&spec, &SsrfGuard::strict())
        .expect_err("a flow with no assertion can never fail and must reject");
    assert_bad_request_with_field(err, "check.steps");
}

#[test]
fn validate_check_accepts_valid_login_flow() {
    use crate::security::ssrf::SsrfGuard;
    let spec: crate::domain::CheckSpec = serde_json::from_str(
            r##"{"type":"flow","start_url":"https://app.example.com/login","steps":[{"op":"fill","selector":"#email","value":"u@x.com"},{"op":"fill","selector":"#pw","value":"{{password}}"},{"op":"click","selector":"#submit"},{"op":"assert_url","contains":"/dashboard"}],"timeout":30000,"step_timeout":5000,"verify_tls":true}"##,
        )
        .expect("flow must deserialize");
    validate_check(&spec, &SsrfGuard::strict()).expect("valid login flow must pass");
}

#[test]
fn validate_check_flow_blocks_internal_goto() {
    use crate::security::ssrf::SsrfGuard;
    let spec: crate::domain::CheckSpec = serde_json::from_str(
            r##"{"type":"flow","start_url":"https://app.example.com/login","steps":[{"op":"goto","url":"http://169.254.169.254/latest/meta-data/"},{"op":"assert_text","selector":null,"contains":"x"}],"timeout":30000,"step_timeout":5000,"verify_tls":true}"##,
        )
        .expect("flow must deserialize");
    let err = validate_check(&spec, &SsrfGuard::strict())
        .expect_err("a goto to a metadata IP must be SSRF-blocked");
    assert_bad_request_with_field(err, "check.url");
}

#[test]
fn validate_check_rejects_http_timeout_over_max() {
    use crate::domain::CheckSpec;
    use crate::security::ssrf::SsrfGuard;
    use std::time::Duration;
    let mut h = http_auth(None, None);
    h.timeout = Duration::from_millis(999_999);
    let err = validate_check(&CheckSpec::Http(h), &SsrfGuard::strict())
        .expect_err("http timeout above 60000 ms must reject");
    assert_bad_request_with_field(err, "check.timeout");
}

#[test]
fn validate_check_rejects_http_zero_timeout() {
    use crate::domain::CheckSpec;
    use crate::security::ssrf::SsrfGuard;
    use std::time::Duration;
    let mut h = http_auth(None, None);
    h.timeout = Duration::ZERO;
    let err = validate_check(&CheckSpec::Http(h), &SsrfGuard::strict())
        .expect_err("zero timeout must reject");
    assert_bad_request_with_field(err, "check.timeout");
}

#[test]
fn validate_check_rejects_excess_redirects() {
    use crate::domain::CheckSpec;
    use crate::security::ssrf::SsrfGuard;
    let mut h = http_auth(None, None);
    h.max_redirects = crate::domain::HttpCheck::MAX_REDIRECTS + 1;
    let err = validate_check(&CheckSpec::Http(h), &SsrfGuard::strict())
        .expect_err("max_redirects above the ceiling must reject");
    assert_bad_request_with_field(err, "check.max_redirects");
}

#[test]
fn validate_check_rejects_tcp_timeout_out_of_range() {
    use crate::security::ssrf::SsrfGuard;
    let spec: crate::domain::CheckSpec = serde_json::from_str(
        r#"{"type":"tcp","host":"db.example.com","port":5432,"timeout":999999}"#,
    )
    .unwrap();
    let err = validate_check(&spec, &SsrfGuard::strict())
        .expect_err("tcp timeout above 60000 ms must reject");
    assert_bad_request_with_field(err, "check.timeout");
}

#[test]
fn validate_check_rejects_tls_warn_days_over_max() {
    use crate::security::ssrf::SsrfGuard;
    let spec: crate::domain::CheckSpec = serde_json::from_str(
            r#"{"type":"tls_cert","host":"example.com","port":443,"warn_days":999,"critical_days":3,"timeout":5000}"#,
        )
        .unwrap();
    let err =
        validate_check(&spec, &SsrfGuard::strict()).expect_err("warn_days above 365 must reject");
    assert_bad_request_with_field(err, "check.warn_days");
}

#[test]
fn validate_check_accepts_tls_days_in_range() {
    use crate::security::ssrf::SsrfGuard;
    let spec: crate::domain::CheckSpec = serde_json::from_str(
            r#"{"type":"tls_cert","host":"example.com","port":443,"warn_days":30,"critical_days":7,"timeout":5000}"#,
        )
        .unwrap();
    validate_check(&spec, &SsrfGuard::strict()).expect("valid tls days must pass");
}

#[test]
fn validate_check_rejects_domain_expiry_zero_days() {
    use crate::security::ssrf::SsrfGuard;
    let spec: crate::domain::CheckSpec = serde_json::from_str(
            r#"{"type":"domain_expiry","domain":"example.com","warn_days":0,"critical_days":0,"timeout":5000}"#,
        )
        .unwrap();
    let err = validate_check(&spec, &SsrfGuard::strict()).expect_err("zero days must reject");
    assert_bad_request_with_field(err, "check.warn_days");
}

#[test]
fn validate_check_rejects_domain_expiry_for_registry_without_public_expiry() {
    use crate::security::ssrf::SsrfGuard;
    for domain in ["denic.DE", "europa.eu"] {
        let spec: crate::domain::CheckSpec = serde_json::from_str(&format!(
                r#"{{"type":"domain_expiry","domain":"{domain}","warn_days":30,"critical_days":7,"timeout":5000}}"#
            ))
            .unwrap();
        let err = validate_check(&spec, &SsrfGuard::strict())
            .expect_err("registry without public expiry must reject");
        assert_bad_request_with_field(err, "check.domain");
    }
}

#[test]
fn validate_check_accepts_domain_expiry_for_whois_only_tld() {
    use crate::security::ssrf::SsrfGuard;
    let spec: crate::domain::CheckSpec = serde_json::from_str(
            r#"{"type":"domain_expiry","domain":"postyourstartup.co","warn_days":30,"critical_days":7,"timeout":5000}"#,
        )
        .unwrap();
    validate_check(&spec, &SsrfGuard::strict()).expect("whois-only TLD is monitorable");
}

fn http_auth(basic: Option<(&str, &str)>, bearer: Option<&str>) -> crate::domain::HttpCheck {
    use crate::domain::{ExpectedStatus, HttpCheck, HttpMethod};
    use std::time::Duration;
    HttpCheck {
        url: "https://example.com/".parse().unwrap(),
        method: HttpMethod::Get,
        timeout: Duration::from_secs(5),
        follow_redirects: false,
        max_redirects: 0,
        expected_status: ExpectedStatus::Exact(200),
        expected_body_contains: None,
        headers: Default::default(),
        body: None,
        verify_tls: true,
        basic_auth: basic.map(|(u, p)| (u.to_owned(), p.to_owned())),
        bearer_token: bearer.map(str::to_owned),
    }
}

#[test]
fn credentials_empty_sentinel_clears() {
    let mut h = http_auth(Some(("", "")), Some(""));
    let (cb, cbr) = take_cleared_credentials(&mut h);
    assert!(cb && cbr);
    assert!(h.basic_auth.is_none() && h.bearer_token.is_none());
}

#[test]
fn credentials_omitted_carry_from_stored() {
    let stored = http_auth(Some(("u", "p")), Some("tok"));
    let mut incoming = http_auth(None, None);
    let (cb, cbr) = take_cleared_credentials(&mut incoming);
    let (carry_basic, carry_bearer) = carry_flags(&incoming, cb, cbr);
    carry_credentials(&mut incoming, &stored, carry_basic, carry_bearer);
    assert_eq!(incoming.basic_auth, Some(("u".into(), "p".into())));
    assert_eq!(incoming.bearer_token.as_deref(), Some("tok"));
}

#[test]
fn credentials_cleared_not_carried_back() {
    let stored = http_auth(Some(("u", "p")), Some("tok"));
    let mut incoming = http_auth(Some(("", "")), Some(""));
    let (cb, cbr) = take_cleared_credentials(&mut incoming);
    let (carry_basic, carry_bearer) = carry_flags(&incoming, cb, cbr);
    carry_credentials(&mut incoming, &stored, carry_basic, carry_bearer);
    assert!(incoming.basic_auth.is_none() && incoming.bearer_token.is_none());
}

#[test]
fn credentials_replacement_kept_over_stored() {
    let stored = http_auth(Some(("u", "p")), Some("old"));
    let mut incoming = http_auth(Some(("new", "pw")), Some("new"));
    let (cb, cbr) = take_cleared_credentials(&mut incoming);
    let (carry_basic, carry_bearer) = carry_flags(&incoming, cb, cbr);
    carry_credentials(&mut incoming, &stored, carry_basic, carry_bearer);
    assert_eq!(incoming.basic_auth, Some(("new".into(), "pw".into())));
    assert_eq!(incoming.bearer_token.as_deref(), Some("new"));
}

#[test]
fn canonicalize_check_idn_encodes_tcp_host() {
    use crate::domain::{CheckSpec, TcpCheck};
    use std::time::Duration;
    let mut spec = CheckSpec::Tcp(TcpCheck {
        host: "Bähn.DE.".into(),
        port: 443,
        timeout: Duration::from_secs(5),
    });
    canonicalize_check(&mut spec).unwrap();
    match spec {
        CheckSpec::Tcp(tcp) => assert_eq!(tcp.host, "xn--bhn-qla.de"),
        _ => panic!("variant changed"),
    }
}

#[test]
fn canonicalize_check_preserves_ipv6_brackets_stripped() {
    use crate::domain::{CheckSpec, TcpCheck};
    use std::time::Duration;
    let mut spec = CheckSpec::Tcp(TcpCheck {
        host: "[::1]".into(),
        port: 443,
        timeout: Duration::from_secs(5),
    });
    canonicalize_check(&mut spec).unwrap();
    match spec {
        CheckSpec::Tcp(tcp) => assert_eq!(tcp.host, "::1"),
        _ => panic!("variant changed"),
    }
}

#[test]
fn canonicalize_check_lowercases_domain_expiry() {
    use crate::domain::{CheckSpec, DomainExpiryCheck};
    use std::time::Duration;
    let mut spec = CheckSpec::DomainExpiry(DomainExpiryCheck {
        domain: "EXAMPLE.COM.".into(),
        warn_days: 30,
        critical_days: 7,
        timeout: Duration::from_secs(5),
    });
    canonicalize_check(&mut spec).unwrap();
    match spec {
        CheckSpec::DomainExpiry(d) => assert_eq!(d.domain, "example.com"),
        _ => panic!("variant changed"),
    }
}

/// Hosts are rewritten on write, so what a client sends is not what it reads
/// back, and the breaker and throttle key on the rewritten form. Nothing
/// recorded which input maps to which output, so a change to the IDN handling
/// showed up as a client breaking rather than as a test failing here.
///
/// Empty expectation means the input is not a usable host and must be rejected.
#[test]
fn host_canonicalization_is_pinned_to_a_fixed_corpus() {
    use crate::domain::{CheckSpec, PingCheck};
    use std::time::Duration;

    const CORPUS: &[(&str, &str)] = &[
        ("example.com", "example.com"),
        ("sub.example.co.uk", "sub.example.co.uk"),
        ("db", "db"),
        ("localhost", "localhost"),
        ("my-svc", "my-svc"),
        ("EXAMPLE.com", "example.com"),
        ("Example.COM.", "example.com"),
        ("example.com.", "example.com"),
        ("example.com..", "example.com"),
        ("Bähn.de", "xn--bhn-qla.de"),
        ("BÄHN.de", "xn--bhn-qla.de"),
        ("bähn.de.", "xn--bhn-qla.de"),
        ("xn--bhn-qla.de", "xn--bhn-qla.de"),
        ("приклад.укр", "xn--80aikifvh.xn--j1amh"),
        ("1.2.3.4", "1.2.3.4"),
        ("1.2.3.4.", "1.2.3.4"),
        ("2001:db8::1", "2001:db8::1"),
        ("2001:DB8::1", "2001:db8::1"),
        ("2001:db8:0:0::1", "2001:db8::1"),
        ("2001:0db8:0000:0000:0000:0000:0000:0001", "2001:db8::1"),
        // Rust keeps the v4-mapped prefix where Go's net.ParseIP collapses it
        // to the dotted form, so a client mirroring this cannot use that.
        ("::FFFF:1.2.3.4", "::ffff:1.2.3.4"),
        // A leading zero stops it parsing as an IP, so it stays a host name
        // and takes the IDN path instead.
        ("1.2.3.04", "1.2.3.04"),
        ("[2001:db8::1]", "2001:db8::1"),
        ("[example.com]", "example.com"),
        ("[oops", ""),
        ("1abc.com", "1abc.com"),
        ("--invalid-leading.com", ""),
        ("under_score.com", ""),
        ("ab--cd.com", ""),
        ("-lead.com", ""),
        ("trail-.com", ""),
        ("a..b.com", ""),
        ("exa mple.com", ""),
        ("example.com:8080", ""),
        ("-", ""),
        ("xn--dh0dc.com", "xn--dh0dc.com"),
        ("", ""),
    ];

    let canon = |input: &str| {
        let mut spec = CheckSpec::Ping(PingCheck {
            host: input.into(),
            timeout: Duration::from_secs(3),
        });
        canonicalize_check(&mut spec).map(|()| match spec {
            CheckSpec::Ping(p) => p.host,
            _ => panic!("variant changed"),
        })
    };

    for (input, want) in CORPUS {
        match canon(input) {
            Ok(got) => {
                assert!(
                    !want.is_empty(),
                    "{input:?} should be rejected, was accepted"
                );
                assert_eq!(&got, want, "canonicalising {input:?}");
            }
            Err(e) => assert!(want.is_empty(), "{input:?} rejected: {e}"),
        }
    }

    // The DNS length limits, pinned separately because they are the part a
    // client has to hard-code to predict what gets stored.
    let label = "a".repeat(63);
    let at_63 = format!("{label}.example.com");
    let over_63 = format!("a{label}.example.com");
    let at_253 = format!("{label}.{label}.{label}.{}", "a".repeat(61));
    let over_253 = format!("{at_253}a");
    assert_eq!(canon(&at_63).unwrap(), at_63);
    assert!(canon(&over_63).is_err(), "64-character label accepted");
    assert_eq!(canon(&at_253).unwrap(), at_253);
    assert!(canon(&over_253).is_err(), "254-character host accepted");
}

#[test]
fn a_named_region_set_is_cleaned_and_an_empty_one_refused() {
    use super::validate::normalize_region_ids;

    assert_eq!(
        normalize_region_ids(&[
            "  us-east ".to_string(),
            "eu-helsinki".to_string(),
            "us-east".to_string(),
        ])
        .unwrap(),
        vec!["eu-helsinki".to_string(), "us-east".to_string()]
    );
    for empty in [vec![], vec!["".to_string()], vec!["   ".to_string()]] {
        assert!(
            matches!(
                normalize_region_ids(&empty),
                Err(crate::error::AppError::Unprocessable { code, .. })
                    if code == codes::REGION_INVALID
            ),
            "{empty:?}"
        );
    }
}
