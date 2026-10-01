use super::*;
use uuid::Uuid;

#[test]
fn sanitize_redacts_echoed_values_in_body_and_headers() {
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
    sanitize_delivered(&mut delivered, &["sk-live-secret".to_string()]);
    assert_eq!(
        delivered.response_body_snippet.as_deref(),
        Some("{\"echo\":\"***\"}")
    );
    assert_eq!(delivered.response_headers_preview[0].value, "***");
}

#[test]
fn header_preview_is_cut_after_secrets_are_scrubbed() {
    use crate::ad_hoc_dispatch::DeliveredResult;
    use crate::domain::CheckResult;
    use crate::domain::agent_wire::HeaderPreview;

    let secret = "sk-live-".to_string() + &"s".repeat(150);
    let echoed = "a".repeat(450) + &secret + &"b".repeat(100);
    let mut delivered = DeliveredResult {
        result: CheckResult::error(Uuid::nil(), Uuid::nil(), "x"),
        response_body_snippet: None,
        response_headers_preview: vec![HeaderPreview {
            name: "x-echo".into(),
            value: echoed,
        }],
        flow_evidence: None,
        flow_steps: vec![],
    };
    sanitize_delivered(&mut delivered, &[secret]);

    let value = &delivered.response_headers_preview[0].value;
    assert!(value.len() <= 512, "{}", value.len());
    assert!(value.ends_with('…'));
    assert!(value.contains("***"));
    assert!(!value.contains("sk-live-"));
}

#[test]
fn a_body_snippet_cut_inside_a_secret_leaves_no_prefix() {
    use crate::ad_hoc_dispatch::DeliveredResult;
    use crate::domain::CheckResult;

    let secret = "sk-live-".to_string() + &"s".repeat(40);
    let snippet = "a".repeat(1000) + &secret[..24] + "…";
    let mut delivered = DeliveredResult {
        result: CheckResult::error(Uuid::nil(), Uuid::nil(), "x"),
        response_body_snippet: Some(snippet),
        response_headers_preview: vec![],
        flow_evidence: None,
        flow_steps: vec![],
    };
    sanitize_delivered(&mut delivered, &[secret]);

    let snippet = delivered.response_body_snippet.unwrap();
    assert!(snippet.ends_with("***…"), "{snippet}");
    assert!(!snippet.contains("sk-live-"));
}

#[test]
fn a_flow_final_url_is_scrubbed_without_any_secret_variable() {
    use crate::ad_hoc_dispatch::DeliveredResult;
    use crate::domain::CheckResult;
    use crate::domain::agent_wire::FlowEvidence;

    let mut delivered = DeliveredResult {
        result: CheckResult::error(Uuid::nil(), Uuid::nil(), "x"),
        response_body_snippet: None,
        response_headers_preview: vec![],
        flow_evidence: Some(FlowEvidence {
            final_url: Some("https://app.example/cb?code=c1786972104789&tenant=acme".into()),
            ..Default::default()
        }),
        flow_steps: vec![],
    };
    sanitize_delivered(&mut delivered, &[]);

    let url = delivered.flow_evidence.unwrap().final_url.unwrap();
    assert!(!url.contains("c1786972104789"), "{url}");
    assert!(url.contains("tenant=acme"));
}

// Every evidence field is a place a just-typed secret can come back.
#[test]
fn sanitize_reaches_every_field_of_flow_evidence() {
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
    sanitize_delivered(&mut delivered, &[secret.to_string()]);

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
