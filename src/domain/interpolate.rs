//! `{{ key }}` interpolation of org variables into HTTP request fields, resolved
//! at probe time. Pure and single-pass: a substituted value is never re-scanned,
//! so a variable whose value itself contains `{{x}}` cannot inject another
//! reference. The per-field policy keeps secrets out of fields that leak them.

use std::collections::HashMap;

use crate::domain::{FlowCheck, FlowStep, HttpCheck, VarMap, validate_var_key};

/// Where a resolved value lands, deciding whether a secret may go there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Url,
    HeaderValue,
    Body,
    Assertion,
    StepValue,
}

impl FieldKind {
    /// Secrets leak through a URL (logs, redirects) and can echo back from an
    /// assertion, so they are confined to header values, bodies, and the value
    /// typed into a flow-step form field.
    fn allows_secret(self) -> bool {
        matches!(
            self,
            FieldKind::HeaderValue | FieldKind::Body | FieldKind::StepValue
        )
    }

    fn label(self) -> &'static str {
        match self {
            FieldKind::Url => "URL",
            FieldKind::HeaderValue => "header value",
            FieldKind::Body => "body",
            FieldKind::Assertion => "assertion",
            FieldKind::StepValue => "step value",
        }
    }
}

/// A resolution failure. The `Display` form is safe to surface as a check error
/// — it carries variable keys and field names, never a secret value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    Unknown(String),
    SecretNotAllowed { key: String, field: FieldKind },
    MalformedToken,
    InvalidUrl,
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(key) => write!(f, "unknown variable: {key}"),
            Self::SecretNotAllowed { key, field } => {
                write!(f, "secret variable {key} not allowed in {}", field.label())
            }
            Self::MalformedToken => f.write_str("malformed variable reference"),
            Self::InvalidUrl => f.write_str("variable produced an invalid url"),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Resolved HTTP request fields, ready to build a probe from. Carries owned
/// strings so the stored `check_spec` is never mutated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHttp {
    pub url: String,
    pub headers: HashMap<String, String>,
    pub body: Option<String>,
    pub expected_body_contains: Option<String>,
}

/// Cheap gate for the probe hot path: true if any interpolable field contains a
/// `{{` token, so a check with no variables skips the var-map lookup entirely.
/// The URL path round-trips through `Url` percent-encoded, so check both forms.
pub fn uses_vars(check: &HttpCheck) -> bool {
    let url = check.url.as_str();
    url.contains("{{")
        || url.contains("%7B%7B")
        || check.headers.values().any(|v| v.contains("{{"))
        || check.body.as_deref().is_some_and(|b| b.contains("{{"))
        || check
            .expected_body_contains
            .as_deref()
            .is_some_and(|a| a.contains("{{"))
}

/// Substitute every `{{ key }}` in `template` from `vars`, enforcing `field`'s
/// secret policy. A complete `{{...}}` whose contents are not a valid key is a
/// [`ResolveError::MalformedToken`]; an unterminated `{{` is left literal.
pub fn resolve(template: &str, vars: &VarMap, field: FieldKind) -> Result<String, ResolveError> {
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' && bytes.get(i + 1) == Some(&b'{') {
            let Some(close) = find_close(bytes, i + 2) else {
                out.push_str(&template[i..]);
                break;
            };
            let key = template[i + 2..close].trim();
            if validate_var_key(key).is_err() {
                return Err(ResolveError::MalformedToken);
            }
            let var = vars
                .get(key)
                .ok_or_else(|| ResolveError::Unknown(key.to_string()))?;
            if var.is_secret && !field.allows_secret() {
                return Err(ResolveError::SecretNotAllowed {
                    key: key.to_string(),
                    field,
                });
            }
            out.push_str(&var.value);
            i = close + 2;
        } else {
            let len = utf8_len(bytes[i]);
            out.push_str(&template[i..i + len]);
            i += len;
        }
    }
    Ok(out)
}

/// Resolve every `{{var}}` in an HTTP check into a new check, re-parsing the
/// resolved URL. The single substitution path shared by save-time validation,
/// interactive dispatch, and the region config-pull, so all three stay in step.
pub fn resolve_http_spec(check: &HttpCheck, vars: &VarMap) -> Result<HttpCheck, ResolveError> {
    let resolved = resolve_http(check, vars)?;
    let url = url::Url::parse(&resolved.url).map_err(|_| ResolveError::InvalidUrl)?;
    let mut out = check.clone();
    out.url = url;
    out.headers = resolved.headers;
    out.body = resolved.body;
    out.expected_body_contains = resolved.expected_body_contains;
    Ok(out)
}

/// Apply the per-field policy across an HTTP check's interpolable fields.
pub fn resolve_http(check: &HttpCheck, vars: &VarMap) -> Result<ResolvedHttp, ResolveError> {
    let url_src = decode_brace_tokens(check.url.as_str());
    let url = resolve(&url_src, vars, FieldKind::Url)?;
    let mut headers = HashMap::with_capacity(check.headers.len());
    for (name, value) in &check.headers {
        headers.insert(name.clone(), resolve(value, vars, FieldKind::HeaderValue)?);
    }
    let body = check
        .body
        .as_deref()
        .map(|b| resolve(b, vars, FieldKind::Body))
        .transpose()?;
    let expected_body_contains = check
        .expected_body_contains
        .as_deref()
        .map(|a| resolve(a, vars, FieldKind::Assertion))
        .transpose()?;
    Ok(ResolvedHttp {
        url,
        headers,
        body,
        expected_body_contains,
    })
}

/// True if any step value carries a `{{` token, so a flow with none skips
/// resolution entirely.
pub fn flow_uses_vars(check: &FlowCheck) -> bool {
    check.steps.iter().any(|s| match s {
        FlowStep::Fill { value, .. } => value.contains("{{"),
        _ => false,
    })
}

/// Resolve `{{var}}` in a flow's step values into a fresh check (the stored spec
/// is never mutated). Only `Fill.value` is interpolable — a secret is allowed
/// there because it is typed into a form field, never echoed to a URL/assertion.
pub fn resolve_flow_spec(check: &FlowCheck, vars: &VarMap) -> Result<FlowCheck, ResolveError> {
    let mut out = check.clone();
    for step in &mut out.steps {
        if let FlowStep::Fill { value, .. } = step {
            *value = resolve(value, vars, FieldKind::StepValue)?;
        }
    }
    Ok(out)
}

/// A repoint-and-exfil shape on one check: a variable in the URL (whose value
/// sets the request host) alongside a secret variable in a header. Whoever can
/// rewrite the URL variable could then steer the secret to a host of their
/// choosing. Surfaced as a save-time advisory, never a hard rejection — a fixed
/// `{{base_url}}` plus an `{{api_key}}` header is a legitimate, common shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepointRisk {
    pub url_keys: Vec<String>,
    pub secret_header_keys: Vec<String>,
}

/// Detect the [`RepointRisk`] shape on `check`. `None` unless the URL references
/// at least one known variable and a header references at least one secret one.
pub fn repoint_risk(check: &HttpCheck, vars: &VarMap) -> Option<RepointRisk> {
    let url_src = decode_brace_tokens(check.url.as_str());
    let url_keys = sorted_unique(
        referenced_keys(&url_src)
            .into_iter()
            .filter(|k| vars.contains_key(k)),
    );
    if url_keys.is_empty() {
        return None;
    }
    let secret_header_keys = sorted_unique(
        check
            .headers
            .values()
            .flat_map(|v| referenced_keys(v))
            .filter(|k| vars.get(k).is_some_and(|r| r.is_secret)),
    );
    if secret_header_keys.is_empty() {
        return None;
    }
    Some(RepointRisk {
        url_keys,
        secret_header_keys,
    })
}

/// Valid variable keys referenced by `{{ key }}` tokens in `template`, in order
/// of appearance. Malformed or unterminated tokens are skipped — a best-effort
/// scan for advisory checks, not validation.
fn referenced_keys(template: &str) -> Vec<String> {
    let bytes = template.as_bytes();
    let mut keys = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && bytes.get(i + 1) == Some(&b'{')
            && let Some(close) = find_close(bytes, i + 2)
        {
            let key = template[i + 2..close].trim();
            if validate_var_key(key).is_ok() {
                keys.push(key.to_string());
            }
            i = close + 2;
            continue;
        }
        i += utf8_len(bytes[i]);
    }
    keys
}

fn sorted_unique(keys: impl Iterator<Item = String>) -> Vec<String> {
    let mut v: Vec<String> = keys.collect();
    v.sort();
    v.dedup();
    v
}

/// `Url::as_str` percent-encodes `{{ }}` in the path component (`%7B%7B` /
/// `%7D%7D`, always uppercase), so decode those delimiter pairs back before
/// resolving; the host and query keep braces verbatim. The resolved string is
/// re-parsed to a `Url` downstream.
fn decode_brace_tokens(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains("%7B%7B") || s.contains("%7D%7D") {
        std::borrow::Cow::Owned(s.replace("%7B%7B", "{{").replace("%7D%7D", "}}"))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// Index of the first `}}` at or after `from`, or `None` if unterminated.
fn find_close(bytes: &[u8], from: usize) -> Option<usize> {
    let mut j = from;
    while j + 1 < bytes.len() {
        if bytes[j] == b'}' && bytes[j + 1] == b'}' {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// Byte length of the UTF-8 sequence starting at `lead`.
fn utf8_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ResolvedVar;

    fn vars(pairs: &[(&str, &str, bool)]) -> VarMap {
        pairs
            .iter()
            .map(|(k, v, is_secret)| {
                (
                    (*k).to_string(),
                    ResolvedVar {
                        value: (*v).to_string(),
                        is_secret: *is_secret,
                    },
                )
            })
            .collect()
    }

    fn plain(pairs: &[(&str, &str)]) -> VarMap {
        vars(
            &pairs
                .iter()
                .map(|(k, v)| (*k, *v, false))
                .collect::<Vec<_>>(),
        )
    }

    fn r(t: &str, v: &VarMap) -> Result<String, ResolveError> {
        resolve(t, v, FieldKind::Body)
    }

    #[test]
    fn single_var() {
        assert_eq!(r("{{a}}", &plain(&[("a", "X")])).unwrap(), "X");
    }

    #[test]
    fn surrounding_text_and_multiple_distinct_vars() {
        let v = plain(&[("host", "api.x"), ("ver", "v2")]);
        assert_eq!(
            r("https://{{host}}/{{ver}}/ping", &v).unwrap(),
            "https://api.x/v2/ping"
        );
    }

    #[test]
    fn same_var_repeated() {
        assert_eq!(r("{{a}}-{{a}}", &plain(&[("a", "Z")])).unwrap(), "Z-Z");
    }

    #[test]
    fn unknown_var() {
        assert_eq!(
            r("{{missing}}", &plain(&[])),
            Err(ResolveError::Unknown("missing".into()))
        );
    }

    #[test]
    fn secret_rejected_in_url_and_assertion() {
        let v = vars(&[("k", "sk", true)]);
        assert_eq!(
            resolve("{{k}}", &v, FieldKind::Url),
            Err(ResolveError::SecretNotAllowed {
                key: "k".into(),
                field: FieldKind::Url
            })
        );
        assert_eq!(
            resolve("{{k}}", &v, FieldKind::Assertion),
            Err(ResolveError::SecretNotAllowed {
                key: "k".into(),
                field: FieldKind::Assertion
            })
        );
    }

    #[test]
    fn secret_allowed_in_header_and_body() {
        let v = vars(&[("k", "sk-123", true)]);
        assert_eq!(
            resolve("{{k}}", &v, FieldKind::HeaderValue).unwrap(),
            "sk-123"
        );
        assert_eq!(resolve("{{k}}", &v, FieldKind::Body).unwrap(), "sk-123");
    }

    #[test]
    fn single_pass_no_reexpansion() {
        // A value containing a token is inserted verbatim, never re-scanned.
        let v = plain(&[("a", "{{b}}"), ("b", "SECRET")]);
        assert_eq!(r("{{a}}", &v).unwrap(), "{{b}}");
    }

    #[test]
    fn whitespace_is_trimmed() {
        let v = plain(&[("key", "V")]);
        assert_eq!(r("{{ key }}", &v).unwrap(), r("{{key}}", &v).unwrap());
        assert_eq!(r("{{  key  }}", &v).unwrap(), "V");
    }

    #[test]
    fn malformed_complete_tokens() {
        let v = plain(&[("a", "X"), ("b", "Y")]);
        for t in [
            "{{}}",
            "{{ }}",
            "{{a b}}",
            "{{1abc}}",
            "{{a{{b}}}}",
            "{{A}}",
        ] {
            assert_eq!(r(t, &v), Err(ResolveError::MalformedToken), "{t}");
        }
    }

    #[test]
    fn unterminated_braces_are_literal() {
        let v = plain(&[("a", "X")]);
        assert_eq!(r("{{", &v).unwrap(), "{{");
        assert_eq!(r("}}", &v).unwrap(), "}}");
        assert_eq!(r("{{a", &v).unwrap(), "{{a");
        assert_eq!(r("a }} b", &v).unwrap(), "a }} b");
        assert_eq!(
            r("prefix {{ unterminated", &v).unwrap(),
            "prefix {{ unterminated"
        );
    }

    #[test]
    fn empty_and_tokenless_templates() {
        let v = plain(&[("a", "X")]);
        assert_eq!(r("", &v).unwrap(), "");
        assert_eq!(r("no tokens here", &v).unwrap(), "no tokens here");
    }

    #[test]
    fn unicode_value_and_surrounding_text() {
        let v = plain(&[("emoji", "✓"), ("name", "naïve")]);
        assert_eq!(
            r("héllo {{emoji}} {{name}} 世界", &v).unwrap(),
            "héllo ✓ naïve 世界"
        );
    }

    #[test]
    fn resolve_http_applies_per_field_policy() {
        let mut check = http_check("https://{{host}}/v1", &[("X-Api-Key", "{{api_key}}")]);
        check.body = Some("{\"k\":\"{{api_key}}\"}".into());
        check.expected_body_contains = Some("{{marker}}".into());
        let v = vars(&[
            ("host", "api.example.com", false),
            ("api_key", "sk-live", true),
            ("marker", "ok", false),
        ]);
        let out = resolve_http(&check, &v).unwrap();
        assert_eq!(out.url, "https://api.example.com/v1");
        assert_eq!(out.headers["X-Api-Key"], "sk-live");
        assert_eq!(out.body.as_deref(), Some("{\"k\":\"sk-live\"}"));
        assert_eq!(out.expected_body_contains.as_deref(), Some("ok"));
    }

    #[test]
    fn uses_vars_detects_tokens_including_encoded_path() {
        let none = http_check("https://api.x/health", &[("Accept", "json")]);
        assert!(!uses_vars(&none));
        assert!(uses_vars(&http_check("https://api.x/{{ver}}/health", &[])));
        assert!(uses_vars(&http_check(
            "https://api.x/",
            &[("X-Api-Key", "{{k}}")]
        )));
    }

    #[test]
    fn resolve_http_resolves_path_token_despite_url_encoding() {
        let check = http_check("https://api.x/{{ver}}/health", &[]);
        assert!(
            check.url.as_str().contains("%7B%7B"),
            "precondition: url crate percent-encodes a path token"
        );
        let out = resolve_http(&check, &plain(&[("ver", "v2")])).unwrap();
        assert_eq!(out.url, "https://api.x/v2/health");
    }

    #[test]
    fn resolve_http_rejects_secret_in_url() {
        let check = http_check("https://{{api_key}}/v1", &[]);
        let v = vars(&[("api_key", "sk", true)]);
        assert!(matches!(
            resolve_http(&check, &v),
            Err(ResolveError::SecretNotAllowed {
                field: FieldKind::Url,
                ..
            })
        ));
    }

    #[test]
    fn repoint_risk_flags_url_var_with_secret_header() {
        let check = http_check("https://{{base}}/v1", &[("X-Api-Key", "{{api_key}}")]);
        let v = vars(&[("base", "api.example.com", false), ("api_key", "sk", true)]);
        let risk = repoint_risk(&check, &v).expect("risky shape detected");
        assert_eq!(risk.url_keys, vec!["base".to_string()]);
        assert_eq!(risk.secret_header_keys, vec!["api_key".to_string()]);
    }

    #[test]
    fn repoint_risk_none_for_plain_header_or_static_url() {
        // URL var but the header references only a plain variable.
        let plain_header = http_check("https://{{base}}/v1", &[("X-Trace", "{{marker}}")]);
        let v = vars(&[("base", "api.x", false), ("marker", "ok", false)]);
        assert!(repoint_risk(&plain_header, &v).is_none());

        // Secret header but a static URL — nothing repointable.
        let static_url = http_check("https://api.x/v1", &[("X-Api-Key", "{{api_key}}")]);
        let v = vars(&[("api_key", "sk", true)]);
        assert!(repoint_risk(&static_url, &v).is_none());
    }

    fn flow(steps: Vec<FlowStep>) -> FlowCheck {
        use std::time::Duration;
        FlowCheck {
            start_url: url::Url::parse("https://app.x/login").unwrap(),
            steps,
            timeout: Duration::from_secs(30),
            step_timeout: Duration::from_secs(5),
            verify_tls: true,
        }
    }

    #[test]
    fn flow_uses_vars_detects_fill_tokens_only() {
        let with = flow(vec![FlowStep::Fill {
            selector: "#pw".into(),
            value: "{{password}}".into(),
        }]);
        let without = flow(vec![
            FlowStep::Fill {
                selector: "#pw".into(),
                value: "literal".into(),
            },
            FlowStep::AssertUrl {
                contains: "{{marker}}".into(),
            },
        ]);
        assert!(flow_uses_vars(&with));
        // Only Fill values are interpolable; a token elsewhere is left literal.
        assert!(!flow_uses_vars(&without));
    }

    #[test]
    fn resolve_flow_spec_substitutes_secret_into_fill_only() {
        let f = flow(vec![
            FlowStep::Fill {
                selector: "#email".into(),
                value: "user@x.com".into(),
            },
            FlowStep::Fill {
                selector: "#pw".into(),
                value: "{{password}}".into(),
            },
            FlowStep::Click {
                selector: "#submit".into(),
            },
        ]);
        let v = vars(&[("password", "s3cr3t", true)]);
        let out = resolve_flow_spec(&f, &v).unwrap();
        assert!(matches!(&out.steps[0], FlowStep::Fill { value, .. } if value == "user@x.com"));
        assert!(matches!(&out.steps[1], FlowStep::Fill { value, .. } if value == "s3cr3t"));
        assert!(matches!(&out.steps[2], FlowStep::Click { selector } if selector == "#submit"));
    }

    fn http_check(url: &str, headers: &[(&str, &str)]) -> HttpCheck {
        use crate::domain::{ExpectedStatus, HttpMethod};
        use std::time::Duration;
        HttpCheck {
            url: url::Url::parse(url).unwrap_or_else(|_| url::Url::parse("https://x/").unwrap()),
            method: HttpMethod::Get,
            timeout: Duration::from_secs(5),
            follow_redirects: false,
            max_redirects: 0,
            expected_status: ExpectedStatus::Exact(200),
            expected_body_contains: None,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            body: None,
            verify_tls: true,
            basic_auth: None,
            bearer_token: None,
        }
    }
}
