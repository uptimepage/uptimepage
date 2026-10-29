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
        ip_version: crate::domain::IpVersion::Any,
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

#[test]
fn every_url_the_probe_fetches_refuses_a_mail_port() {
    use crate::domain::{DnsRecordType, FlowStep};
    use crate::security::ssrf::SsrfGuard;
    let guard = SsrfGuard::strict();
    let refused = |spec: CheckSpec, field: &str| {
        let err = validate_check(&spec, &guard).expect_err("mail port must reject");
        match err {
            AppError::BadRequest {
                code, field: got, ..
            } => {
                assert_eq!(code, codes::MAIL_PORT_NOT_ALLOWED);
                assert_eq!(got.as_deref(), Some(field));
            }
            other => panic!("expected a bad request, got {other:?}"),
        }
    };
    for port in crate::domain::check::MAIL_PORTS {
        let mut http = head_spec(None);
        if let CheckSpec::Http(h) = &mut http {
            h.url = format!("https://mx.example.com:{port}/").parse().unwrap();
        }
        refused(http, "check.url");

        let mut flow = flow_of(1);
        if let CheckSpec::Flow(f) = &mut flow {
            f.start_url = format!("http://mx.example.com:{port}/").parse().unwrap();
        }
        refused(flow, "check.start_url");

        let mut goto = flow_of(1);
        if let CheckSpec::Flow(f) = &mut goto {
            f.steps.insert(
                0,
                FlowStep::Goto {
                    url: format!("https://mx.example.com:{port}/").parse().unwrap(),
                },
            );
        }
        refused(goto, "check.steps");

        let mut dns = dns_spec("example.com", DnsRecordType::A);
        if let CheckSpec::Dns(d) = &mut dns {
            d.resolver = Some(format!("1.1.1.1:{port}"));
        }
        refused(dns, "check.resolver");
    }
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
fn a_literal_host_refuses_the_family_it_is_not_in() {
    use crate::security::ssrf::SsrfGuard;
    let spec = |json: &str| -> crate::domain::CheckSpec { serde_json::from_str(json).unwrap() };
    let refused = [
        r#"{"type":"tcp","host":"1.1.1.1","port":53,"timeout":3000,"ip_version":"ipv6"}"#,
        r#"{"type":"tls_cert","host":"2606:4700:4700::1111","port":443,"warn_days":14,"critical_days":3,"timeout":5000,"ip_version":"ipv4"}"#,
        r#"{"type":"http","url":"https://[2606:4700:4700::1111]/","method":"GET","timeout":5000,"follow_redirects":false,"max_redirects":0,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true,"ip_version":"ipv4"}"#,
    ];
    for json in refused {
        let err = validate_check(&spec(json), &SsrfGuard::strict()).expect_err(json);
        assert_eq!(code_of(err), codes::INVALID_IP_VERSION, "{json}");
    }
    let accepted = [
        r#"{"type":"tcp","host":"1.1.1.1","port":53,"timeout":3000,"ip_version":"ipv4"}"#,
        r#"{"type":"tcp","host":"1.1.1.1","port":53,"timeout":3000,"ip_version":"any"}"#,
        r#"{"type":"tcp","host":"db.example.com","port":5432,"timeout":3000,"ip_version":"ipv6"}"#,
        r#"{"type":"http","url":"https://[2606:4700:4700::1111]/","method":"GET","timeout":5000,"follow_redirects":false,"max_redirects":0,"expected_status":{"kind":"exact","value":200},"headers":{},"verify_tls":true,"ip_version":"ipv6"}"#,
    ];
    for json in accepted {
        validate_check(&spec(json), &SsrfGuard::strict()).expect(json);
    }
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
    for (start, goto, field) in [
        (
            "https://app.example.com/login",
            "http://169.254.169.254/latest/meta-data/",
            "check.steps",
        ),
        (
            "http://10.0.0.1/login",
            "https://app.example.com/",
            "check.start_url",
        ),
    ] {
        let spec: crate::domain::CheckSpec = serde_json::from_value(serde_json::json!({
            "type": "flow",
            "start_url": start,
            "steps": [
                {"op": "goto", "url": goto},
                {"op": "assert_text", "selector": null, "contains": "x"}
            ],
            "timeout": 30000,
            "step_timeout": 5000,
            "verify_tls": true
        }))
        .expect("flow must deserialize");
        let err = validate_check(&spec, &SsrfGuard::strict())
            .expect_err("an internal address must be SSRF-blocked");
        let AppError::BadRequest {
            message,
            field: got,
            ..
        } = err
        else {
            panic!("expected a bad request, got {err:?}");
        };
        assert_eq!(got.as_deref(), Some(field));
        assert_eq!(
            message.starts_with("step 1: "),
            field == "check.steps",
            "{message}"
        );
    }
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
        ip_version: crate::domain::IpVersion::Any,
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
        ip_version: crate::domain::IpVersion::Any,
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
        ip_version: crate::domain::IpVersion::Any,
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
        (" example.com ", "example.com"),
        ("example.com\n", "example.com"),
        (" [2001:db8::1] ", "2001:db8::1"),
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
fn a_refused_host_is_not_echoed_back() {
    use crate::domain::{CheckSpec, DnsRecordType, PingCheck};
    use std::time::Duration;

    let pasted = "postgres://alice:s3cret@db.example.com:5432/app";
    let ping = CheckSpec::Ping(PingCheck {
        host: pasted.into(),
        timeout: Duration::from_secs(3),
    });
    for (mut spec, want_code, want_field) in [
        (ping, codes::INVALID_PING_HOST, "check.host"),
        (
            dns_spec(pasted, DnsRecordType::A),
            codes::INVALID_DNS_PARAMS,
            "check.domain",
        ),
    ] {
        let err = canonicalize_check(&mut spec).expect_err("a connection string is not a host");
        let AppError::BadRequest {
            code,
            message,
            field,
        } = err
        else {
            panic!("expected a bad request, got {err:?}");
        };
        assert_eq!(code, want_code);
        assert_eq!(field.as_deref(), Some(want_field));
        assert!(
            !message.contains("alice") && !message.contains("s3cret"),
            "{message}"
        );
    }
}

fn dns_spec(domain: &str, record_type: crate::domain::DnsRecordType) -> CheckSpec {
    CheckSpec::Dns(crate::domain::DnsCheck {
        domain: domain.into(),
        record_type,
        resolver: None,
        expected_contains: None,
        expected_records: None,
        timeout: std::time::Duration::from_secs(3),
    })
}

fn dns_domain(spec: CheckSpec) -> String {
    match spec {
        CheckSpec::Dns(d) => d.domain,
        _ => panic!("variant changed"),
    }
}

#[test]
fn dns_names_accept_underscore_and_leftmost_wildcard_labels_and_keep_the_other_rules() {
    use crate::domain::DnsRecordType;

    let canon = |input: &str| {
        let mut spec = dns_spec(input, DnsRecordType::Txt);
        canonicalize_check(&mut spec).map(|()| dns_domain(spec))
    };
    let name_of_len =
        |len: usize| format!("{0}.{0}.{0}.{1}", "a".repeat(63), "a".repeat(len - 192));
    let wildcard_253 = format!("*.{}", name_of_len(251));

    for (input, want) in [
        ("_dmarc.example.com", "_dmarc.example.com"),
        ("s1._domainkey.example.com", "s1._domainkey.example.com"),
        ("_sip._tcp.example.com", "_sip._tcp.example.com"),
        ("foo_bar.example.com", "foo_bar.example.com"),
        (" _dmarc.example.com\t", "_dmarc.example.com"),
        (
            "_ACME-challenge.Example.COM.",
            "_acme-challenge.example.com",
        ),
        ("*.Example.COM.", "*.example.com"),
        ("*._tcp.example.com", "*._tcp.example.com"),
        (&wildcard_253, &wildcard_253),
    ] {
        assert_eq!(canon(input).unwrap(), want, "canonicalising {input:?}");
    }

    let label = "a".repeat(64);
    let over_253 = name_of_len(254);
    let wildcard_over_253 = format!("*.{}", name_of_len(252));
    for input in [
        "-lead.example.com",
        "a..b.example.com",
        "exa mple.com",
        &format!("{label}.example.com"),
        &over_253,
        "a.*.example.com",
        "*a.example.com",
        "*.*.example.com",
        "*",
        "*.",
        &wildcard_over_253,
    ] {
        assert!(canon(input).is_err(), "{input:?} accepted");
    }
}

#[test]
fn an_address_is_a_dns_domain_only_for_ptr() {
    use crate::domain::DnsRecordType;
    use crate::security::ssrf::SsrfGuard;

    let vet = |input: &str, record_type| {
        let mut spec = dns_spec(input, record_type);
        canonicalize_check(&mut spec)?;
        validate_check(&spec, &SsrfGuard::strict())?;
        Ok::<_, AppError>(dns_domain(spec))
    };

    for (input, stored) in [
        ("192.0.2.1", "192.0.2.1"),
        ("2001:db8::1", "2001:db8::1"),
        ("[2001:DB8::1]", "2001:db8::1"),
        ("::ffff:192.0.2.1", "::ffff:192.0.2.1"),
        ("1.2.0.192.in-addr.arpa", "1.2.0.192.in-addr.arpa"),
    ] {
        assert_eq!(vet(input, DnsRecordType::Ptr).unwrap(), stored, "{input:?}");
    }

    for record_type in [
        DnsRecordType::A,
        DnsRecordType::Aaaa,
        DnsRecordType::Txt,
        DnsRecordType::Mx,
    ] {
        for input in ["192.0.2.1", "2001:db8::1"] {
            let err = vet(input, record_type).expect_err("an address outside PTR must reject");
            let AppError::BadRequest { code, field, .. } = err else {
                panic!("expected a bad request, got {err:?}");
            };
            assert_eq!(code, codes::INVALID_DNS_PARAMS);
            assert_eq!(field.as_deref(), Some("check.domain"));
        }
    }
}

#[test]
fn an_exact_record_set_is_refused_when_it_could_never_match() {
    use crate::domain::{DnsCheck, DnsRecordType};
    use crate::security::ssrf::SsrfGuard;

    let vet = |record_type, records: &[&str], contains: Option<&str>| {
        let mut spec = dns_spec("example.com", record_type);
        if let CheckSpec::Dns(d) = &mut spec {
            d.expected_records = Some(records.iter().map(|r| r.to_string()).collect());
            d.expected_contains = contains.map(str::to_string);
        }
        validate_check(&spec, &SsrfGuard::strict())
    };

    for (record_type, records, contains) in [
        (DnsRecordType::A, &["192.0.2.1", "192.0.2.2"][..], None),
        (DnsRecordType::Aaaa, &["2001:db8::1"][..], Some("")),
        (DnsRecordType::Mx, &["10 mail.example.com."][..], None),
        (
            DnsRecordType::Mx,
            &["smtp.google.com", "5 backup.example.com."][..],
            None,
        ),
        (
            DnsRecordType::Mx,
            &["mail.example.com", "10 mail.example.com"][..],
            None,
        ),
        (DnsRecordType::Mx, &["пошта.укр"][..], None),
        (
            DnsRecordType::Srv,
            &["10 5 5060 sip.example.com."][..],
            None,
        ),
        (
            DnsRecordType::Caa,
            &["0 issue \"letsencrypt.org\""][..],
            None,
        ),
        (
            DnsRecordType::Soa,
            &["ns1.example.com. hostmaster.example.com. 1 7200 3600 1209600 3600"][..],
            None,
        ),
        (DnsRecordType::Ns, &["NS1.Example.NET"][..], None),
        (DnsRecordType::Cname, &["app_1.cdn.example.net."][..], None),
        (DnsRecordType::A, &[][..], Some("192.0.2")),
    ] {
        assert!(
            vet(record_type, records, contains).is_ok(),
            "{records:?} refused"
        );
    }

    let too_many: Vec<String> = (0..=DnsCheck::MAX_EXPECTED_RECORDS)
        .map(|i| format!("192.0.2.{i}"))
        .collect();
    let too_many: Vec<&str> = too_many.iter().map(String::as_str).collect();
    let too_long = "a".repeat(DnsCheck::MAX_EXPECTED_RECORD_LEN + 1);
    for (record_type, records, contains) in [
        (DnsRecordType::A, &["192.0.2.1"][..], Some("192.0.2")),
        (DnsRecordType::A, &too_many[..], None),
        (DnsRecordType::Txt, &[too_long.as_str()][..], None),
        (DnsRecordType::Txt, &["v=spf1 -all", "  "][..], None),
        (DnsRecordType::A, &["2001:db8::1"][..], None),
        (DnsRecordType::Aaaa, &["192.0.2.1"][..], None),
        (DnsRecordType::A, &["mail.example.com"][..], None),
        (DnsRecordType::Mx, &["192.0.2.10"][..], None),
        (DnsRecordType::Mx, &["2001:db8::1"][..], None),
        (DnsRecordType::Mx, &["10"][..], None),
        (DnsRecordType::Mx, &["70000"][..], None),
        (DnsRecordType::Mx, &["10.5"][..], None),
        (DnsRecordType::Mx, &["mail..example.com"][..], None),
        (DnsRecordType::Mx, &["10 mail.example.com. extra"][..], None),
        (
            DnsRecordType::Mx,
            &["mail.example.com", "Mail.Example.com."][..],
            None,
        ),
        (DnsRecordType::Srv, &["5060 sip.example.com."][..], None),
        (
            DnsRecordType::Mx,
            &["10 mail.example.com.", "010 Mail.example.com"][..],
            None,
        ),
        (DnsRecordType::Caa, &["issue letsencrypt.org"][..], None),
        (DnsRecordType::Cname, &["edge.example.net. extra"][..], None),
        (DnsRecordType::Txt, &["v=spf1\n-all"][..], None),
    ] {
        let err = vet(record_type, records, contains)
            .expect_err("an exact set that cannot match must reject");
        let AppError::BadRequest { code, field, .. } = err else {
            panic!("expected a bad request, got {err:?}");
        };
        assert_eq!(code, codes::INVALID_DNS_PARAMS);
        assert_eq!(field.as_deref(), Some("check.expected_records"));
    }
}

#[test]
fn a_named_region_set_is_cleaned_and_an_empty_one_refused() {
    use super::normalize_region_ids;

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

#[test]
fn renotify_interval_fits_the_column() {
    assert!(validate_renotify_interval(Some(0)).is_ok());
    assert!(validate_renotify_interval(Some(60)).is_ok());
    assert!(validate_renotify_interval(Some(i32::MAX as u32)).is_ok());
    assert!(validate_renotify_interval(Some(59)).is_err());
    assert!(validate_renotify_interval(Some(i32::MAX as u32 + 1)).is_err());
}

#[test]
fn alert_confirmations_fit_the_column() {
    assert!(validate_alert_confirmations(Some(1)).is_ok());
    assert!(validate_alert_confirmations(Some(i32::MAX as u32)).is_ok());
    assert!(validate_alert_confirmations(Some(0)).is_err());
    assert!(validate_alert_confirmations(Some(i32::MAX as u32 + 1)).is_err());
}

#[test]
fn an_interval_fits_the_column() {
    use std::time::Duration;
    assert!(validate_interval_fits(Duration::from_secs(i32::MAX as u64)).is_ok());
    assert_eq!(
        code_of(validate_interval_fits(Duration::from_secs((1 << 32) + 60)).unwrap_err()),
        codes::INVALID_INTERVAL
    );
}
