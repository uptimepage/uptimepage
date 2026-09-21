//! Scrubs credentials out of what leaves the process: check specs headed to a
//! client, captured probe output, and URLs that carry secrets in their query.

use crate::domain::agent_wire::FlowEvidence;
use crate::domain::{CheckSpec, FlowStep, NotificationChannel, REDACTED, Target, VarMap};

/// Scrubbing a one or two character string would mangle unrelated text for
/// no real protection.
const MIN_SECRET_CHARS: usize = 4;

/// Secret plaintexts in an org's variable map, for scrubbing whatever a probe
/// captured.
pub fn secret_values(vars: &VarMap) -> Vec<String> {
    vars.values()
        .filter(|v| v.is_secret && v.value.chars().count() >= MIN_SECRET_CHARS)
        .map(|v| v.value.clone())
        .collect()
}

pub fn redact_secrets(s: &mut String, secrets: &[String]) {
    for secret in secrets {
        if s.contains(secret.as_str()) {
            *s = s.replace(secret.as_str(), REDACTED);
        }
    }
    if let Some(body) = s.strip_suffix('…')
        && let Some(keep) = cut_secret_start(body, secrets)
    {
        s.truncate(keep);
        s.push_str(REDACTED);
        s.push('…');
    }
}

/// Probes end what they cut with `…`, and a secret straddling that cut
/// survives as the text's tail. Where the longest such prefix starts; the
/// [`secret_values`] minimum keeps a chance overlap from mangling text.
fn cut_secret_start(body: &str, secrets: &[String]) -> Option<usize> {
    secrets
        .iter()
        .filter_map(|secret| {
            (1..secret.len())
                .rev()
                .filter(|&end| secret.is_char_boundary(end))
                .find(|&end| body.ends_with(&secret[..end]))
                .filter(|&end| secret[..end].chars().count() >= MIN_SECRET_CHARS)
        })
        .max()
        .map(|len| body.len() - len)
}

/// Words that make a key a credential when they stand alone as one of its
/// segments, so `auth_code`, `verification-code` and `oauth_state` are caught
/// while `codeword` stays the locator it is. Too short to match as substrings.
const SECRET_KEY_SEGMENTS: [&str; 9] = [
    "code", "state", "sid", "jwt", "pw", "otp", "ticket", "key", "sig",
];

/// Substrings that make a key a credential wherever they appear, matched against
/// the key with punctuation removed, so the endless spellings (`client_secret`,
/// `oauth_token`, `X-Api-Key`, `SAMLResponse`) need no enumerating.
const SECRET_PARAMS_CONTAINING: [&str; 11] = [
    "token",
    "secret",
    "password",
    "passwd",
    "signature",
    "assertion",
    "session",
    "apikey",
    "samlresponse",
    "credential",
    "bearer",
];

pub(crate) fn is_secret_param(key: &str) -> bool {
    let compact: String = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    SECRET_PARAMS_CONTAINING.iter().any(|s| compact.contains(s))
        || key_words(key)
            .iter()
            .any(|w| SECRET_KEY_SEGMENTS.contains(&w.as_str()))
}

/// Splits a key into its words, on punctuation and on camelCase humps alike, so
/// `oobCode` and `auth_code` both read as carrying the word `code` while
/// `codeword` stays one word. Firebase, Auth0 and SAML all spell their callback
/// parameters in camelCase, so ignoring humps leaves live credentials in place.
fn key_words(key: &str) -> Vec<String> {
    let chars: Vec<char> = key.chars().collect();
    let mut words = Vec::new();
    let mut word = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_ascii_alphanumeric() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            continue;
        }
        let prev = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1).copied();
        // A hump is `oobCode` (lower/digit then upper) or `SAMLResponse` (a run
        // of uppercase whose last letter starts the next word).
        let hump = c.is_ascii_uppercase()
            && (prev.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit())
                || (prev.is_some_and(|p| p.is_ascii_uppercase())
                    && next.is_some_and(|n| n.is_ascii_lowercase())));
        if hump && !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        word.push(c.to_ascii_lowercase());
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// Replaces the value of every credential-keyed pair in one `&`-joined pair
/// list, leaving every other byte exactly as the page produced it. Byte fidelity
/// is the point, and the reason this works on the raw string rather than through
/// `Url`: org secrets are stripped by literal match in
/// [`redact_secrets`], and `Url`'s own serialization
/// re-encodes characters like `'`, which would hide a secret from that pass and
/// persist it.
fn scrub_pairs(pairs: &str) -> String {
    pairs
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, _)) if is_secret_param(k) => format!("{k}={REDACTED}"),
            _ => pair.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// A hash route carries its own query (`#/callback?code=…`), so the pair list
/// starts after the route's `?` rather than at the start of the fragment. A
/// fragment with no pairs anywhere is a route, and stays whole.
fn scrub_fragment(fragment: &str) -> String {
    match fragment.split_once('?') {
        Some((route, params)) => format!("{route}?{}", scrub_pairs(params)),
        None if fragment.contains('=') => scrub_pairs(fragment),
        None => fragment.to_owned(),
    }
}

/// Drops any `user:password@` from the authority of a URL's head.
fn strip_userinfo(head: &str) -> String {
    let Some((scheme, rest)) = head.split_once("://") else {
        return head.to_owned();
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://{host}{path}"),
        None => head.to_owned(),
    }
}

/// Rewrites credentials out of a URL, keeping the parts that say where the flow
/// ended up. Splits the raw string rather than round-tripping through `Url`, so
/// that every byte it does not redact reaches storage unchanged; `Url::parse` is
/// the validity gate only. Something unparseable keeps just its path-ish head,
/// since nothing can be reasoned about the rest.
pub(crate) fn scrub_url(raw: &str) -> String {
    let (before_fragment, fragment) = match raw.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (raw, None),
    };
    let (head, query) = match before_fragment.split_once('?') {
        Some((h, q)) => (h, Some(q)),
        None => (before_fragment, None),
    };
    // Userinfo goes even here: a URL the browser reported but `url` rejects
    // (an out-of-range port, say) must not keep the one thing this removes.
    if url::Url::parse(raw).is_err() {
        return strip_userinfo(head);
    }

    let mut out = strip_userinfo(head);
    if let Some(query) = query {
        out.push('?');
        out.push_str(&scrub_pairs(query));
    }
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(&scrub_fragment(fragment));
    }
    out
}

/// Strip resolved secrets from what a failed flow left on the page. The single
/// place both probe paths scrub evidence: a test run before it is shown back,
/// and a scheduled run before it is stored.
pub fn scrub_flow_evidence(ev: &mut FlowEvidence, secrets: &[String]) {
    // Runs whatever the org's variables hold, and wherever the evidence came
    // from: a probe still on an older build reports the raw URL, and this is the
    // boundary that writes the row.
    if let Some(url) = ev.final_url.as_mut() {
        *url = scrub_url(url);
    }
    if secrets.is_empty() {
        return;
    }
    for field in [&mut ev.final_url, &mut ev.title, &mut ev.text_snippet] {
        if let Some(v) = field.as_mut() {
            redact_secrets(v, secrets);
        }
    }
    for line in &mut ev.console {
        redact_secrets(&mut line.text, secrets);
    }
}

/// In-place credential redaction. Implemented by API-layer wrappers so callers
/// can't serialize a `Target` to a client without going through `Redacted<T>`.
pub trait RedactInPlace {
    fn redact_in_place(&mut self);
}

impl RedactInPlace for Target {
    fn redact_in_place(&mut self) {
        redact_check(&mut self.check);
    }
}

impl RedactInPlace for NotificationChannel {
    fn redact_in_place(&mut self) {
        self.config.redact_in_place();
    }
}

impl<T: RedactInPlace> RedactInPlace for Vec<T> {
    fn redact_in_place(&mut self) {
        for item in self {
            item.redact_in_place();
        }
    }
}

pub(crate) fn redact_check(check: &mut CheckSpec) {
    match check {
        CheckSpec::Http(http) => {
            if let Some((u, p)) = http.basic_auth.as_mut() {
                *u = REDACTED.to_string();
                *p = REDACTED.to_string();
            }
            if let Some(token) = http.bearer_token.as_mut() {
                *token = REDACTED.to_string();
            }
        }
        // A Fill holds credential input (the flow analog of bearer_token), so
        // mask it for org members too, not only outside viewers.
        CheckSpec::Flow(flow) => {
            for step in &mut flow.steps {
                if let FlowStep::Fill { value, .. } = step {
                    *value = REDACTED.to_string();
                }
            }
        }
        _ => {}
    }
}

/// Stronger redaction for surfaces shown to people outside the owning org (the
/// public `/m/{token}` share view). The operator API only masks the structured
/// credential fields, trusting a member with the rest of the config; a public
/// viewer must not see anything that can carry a secret. On top of
/// [`redact_check`] this masks every request-header value and the request body
/// (common homes for `Authorization` / `X-Api-Key` / `Cookie` secrets) and
/// strips credentials embedded in the URL (userinfo, query, fragment). Mask the
/// whole class rather than allow-list "known sensitive" names — a denylist on a
/// public surface is the wrong default.
pub(crate) fn redact_check_for_public(check: &mut CheckSpec) {
    redact_check(check);
    match check {
        CheckSpec::Http(http) => {
            for value in http.headers.values_mut() {
                *value = REDACTED.to_string();
            }
            if http.body.is_some() {
                http.body = Some(REDACTED.to_string());
            }
            strip_url_credentials(&mut http.url);
        }
        // `redact_check` already masked `Fill.value`; a public viewer additionally
        // loses any userinfo/query/fragment riding in the nav URLs.
        CheckSpec::Flow(flow) => {
            strip_url_credentials(&mut flow.start_url);
            for step in &mut flow.steps {
                if let FlowStep::Goto { url } = step {
                    strip_url_credentials(url);
                }
            }
        }
        _ => {}
    }
}

/// Drop credentials that can ride inside a URL: userinfo, query, and fragment.
pub(crate) fn strip_url_credentials(url: &mut url::Url) {
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redacted(text: &str, secrets: &[&str]) -> String {
        let mut s = text.to_owned();
        let secrets: Vec<String> = secrets.iter().map(|s| s.to_string()).collect();
        redact_secrets(&mut s, &secrets);
        s
    }

    #[test]
    fn a_secret_cut_by_the_probe_leaves_no_prefix() {
        let secret = "sk-live-abcdefgh";
        assert_eq!(redacted("token=sk-live-abcd…", &[secret]), "token=***…");
        assert_eq!(
            redacted("sk-live-abcdefgh and sk-live-abcd…", &[secret]),
            "*** and ***…"
        );
    }

    #[test]
    fn an_uncut_text_ending_in_a_prefix_is_ordinary_content() {
        assert_eq!(
            redacted("see https", &["https://hooks.example/x"]),
            "see https"
        );
        assert_eq!(redacted("plain text", &["sk-live-abcdefgh"]), "plain text");
    }

    #[test]
    fn a_short_overlap_is_not_a_cut_secret() {
        assert_eq!(
            redacted("status ok s…", &["sk-live-abcdefgh"]),
            "status ok s…"
        );
        assert_eq!(
            redacted("Головна гру па…", &["пароль123"]),
            "Головна гру па…"
        );
        assert_eq!(redacted("вхід паро…", &["пароль123"]), "вхід ***…");
    }

    #[test]
    fn the_longest_cut_secret_wins() {
        let out = redacted(
            "x=gh-abcdefgh-11…",
            &["abcdefgh-1111", "gh-abcdefgh-1111-2222"],
        );
        assert_eq!(out, "x=***…");
    }

    #[test]
    fn scrub_url_drops_an_oauth_code_and_keeps_the_location() {
        let got = scrub_url("https://myapps.example.com/oauth/?code=c1786972104789&tenant=acme");
        assert!(
            got.starts_with("https://myapps.example.com/oauth/?"),
            "{got}"
        );
        assert!(
            !got.contains("c1786972104789"),
            "code must not survive: {got}"
        );
        assert!(got.contains("tenant=acme"), "locators stay: {got}");
    }

    #[test]
    fn scrub_url_matches_keys_case_insensitively_and_leaves_others() {
        let got = scrub_url("https://x.test/cb?Code=abc&State=xyz&page=2&codeword=keep");
        assert!(!got.contains("abc") && !got.contains("xyz"), "{got}");
        assert!(got.contains("page=2"), "{got}");
        // Substring matches would eat a legitimate parameter.
        assert!(got.contains("codeword=keep"), "{got}");
    }

    #[test]
    fn scrub_url_redacts_an_implicit_grant_fragment() {
        let got = scrub_url("https://x.test/cb#access_token=abc&expires_in=3600");
        assert!(!got.contains("abc"), "{got}");
        assert!(got.contains("expires_in=3600"), "keeps the rest: {got}");
        // A plain anchor is a locator, not a credential.
        assert_eq!(
            scrub_url("https://x.test/docs#install"),
            "https://x.test/docs#install"
        );
        // Nor is a hash route that merely carries parameters.
        assert_eq!(
            scrub_url("https://x.test/app#/dashboard?tab=alerts"),
            "https://x.test/app#/dashboard?tab=alerts"
        );
    }

    #[test]
    fn scrub_url_keeps_only_the_head_of_something_unparseable() {
        assert_eq!(scrub_url("not a url?code=secret"), "not a url");
    }

    #[test]
    fn scrub_url_leaves_a_surviving_value_byte_identical() {
        // Org secrets are stripped downstream by literal match, so re-encoding
        // a value it has yet to see would hide the secret and persist it.
        let raw = "https://app.example.com/login?pw=pa55w/rd!&next=/home%20page";
        let got = scrub_url(raw);
        assert!(
            !got.contains("pa55w/rd!"),
            "a credential-shaped key is redacted: {got}"
        );
        let kept = scrub_url("https://app.example.com/login?next=/a+b/c!~d*e'f(g)&q=x:y@z");
        assert_eq!(
            kept, "https://app.example.com/login?next=/a+b/c!~d*e'f(g)&q=x:y@z",
            "every non-secret byte must survive untouched"
        );
    }

    #[test]
    fn scrub_url_covers_the_spellings_a_name_list_would_miss() {
        for raw in [
            "https://x.test/cb?client_secret=abc",
            "https://x.test/cb?oauth_token=abc",
            "https://x.test/acs?SAMLResponse=abc",
            "https://x.test/cb?X-Api-Key=abc",
            "https://x.test/cb?session_id=abc",
            "https://x.test/cb?passwd=abc",
            // Compound names: the credential word is one segment of the key,
            // which is how magic-link and OTP callbacks spell it.
            "https://x.test/cb?auth_code=abc",
            // camelCase, which is how Firebase email-link sign-in, Auth0 and
            // SAML actually spell theirs. `oobCode` is account takeover.
            "https://x.test/__/auth/action?mode=signIn&oobCode=abc",
            "https://x.test/cb?authCode=abc",
            "https://x.test/cb?oauthState=abc",
            "https://x.test/cb?loginTicket=abc",
            "https://x.test/cb?magicCode=abc",
            "https://x.test/cb?apiKey=abc",
            "https://x.test/cb?verification-code=abc",
            "https://x.test/cb?otp=abc",
            "https://x.test/cb?oauth_state=abc",
            "https://x.test/cb?x_sig=abc",
        ] {
            let got = scrub_url(raw);
            assert!(!got.contains("abc"), "{raw} -> {got}");
        }
    }

    #[test]
    fn scrub_url_redacts_a_code_inside_a_hash_route() {
        // A hash-routed SPA callback puts the query after the route, so the
        // pair list does not start at the fragment's first character.
        let got = scrub_url("https://app.example.com/#/callback?code=AUTH_CODE&state=xyz");
        assert!(!got.contains("AUTH_CODE"), "{got}");
        assert!(!got.contains("xyz"), "{got}");
        assert!(got.contains("#/callback?"), "the route survives: {got}");
    }

    #[test]
    fn scrub_url_strips_userinfo_even_when_the_url_will_not_parse() {
        let got = scrub_url("https://u:pw@host:99999/x");
        assert!(!got.contains("pw@") && !got.contains("u:"), "{got}");
    }

    #[test]
    fn key_words_splits_humps_without_inventing_them() {
        assert_eq!(key_words("oobCode"), vec!["oob", "code"]);
        assert_eq!(key_words("SAMLResponse"), vec!["saml", "response"]);
        assert_eq!(key_words("auth_code"), vec!["auth", "code"]);
        assert_eq!(key_words("X-Api-Key"), vec!["x", "api", "key"]);
        // One word stays one word, so a locator keeps its value.
        assert_eq!(key_words("codeword"), vec!["codeword"]);
    }

    #[test]
    fn scrub_url_keeps_locators_that_merely_read_like_credentials() {
        let got = scrub_url("https://x.test/p?codeword=keep&zipcode=12345&mode=signIn&page=2");
        for kept in ["codeword=keep", "zipcode=12345", "mode=signIn", "page=2"] {
            assert!(got.contains(kept), "{kept} must survive: {got}");
        }
    }

    #[test]
    fn scrub_url_strips_userinfo() {
        let got = scrub_url("https://user:pa55word@app.example.com/login");
        assert!(!got.contains("pa55word") && !got.contains("user@"), "{got}");
        assert!(got.contains("app.example.com/login"), "{got}");
    }

    use std::collections::HashMap;
    use std::time::Duration;

    use crate::domain::{CheckSpec, ExpectedStatus, HttpCheck, HttpMethod};

    use super::{REDACTED, key_words, redact_check, redact_check_for_public, scrub_url};

    fn http_with_secrets() -> CheckSpec {
        CheckSpec::Http(HttpCheck {
            url: url::Url::parse("https://user:pass@api.example.com/health?token=qsecret#frag")
                .unwrap(),
            method: HttpMethod::Get,
            timeout: Duration::from_secs(5),
            follow_redirects: true,
            max_redirects: 3,
            expected_status: ExpectedStatus::Exact(200),
            expected_body_contains: None,
            headers: HashMap::from([("X-Api-Key".to_string(), "hsecret".to_string())]),
            body: Some("psecret".to_string()),
            verify_tls: true,
            basic_auth: Some(("u".into(), "p".into())),
            bearer_token: Some("bsecret".into()),
        })
    }

    #[test]
    fn public_redaction_masks_every_secret_bearing_field() {
        let mut check = http_with_secrets();
        redact_check_for_public(&mut check);
        let CheckSpec::Http(http) = check else {
            panic!("expected http");
        };
        // Structured credentials.
        assert_eq!(http.bearer_token.as_deref(), Some(REDACTED));
        assert_eq!(
            http.basic_auth,
            Some((REDACTED.to_string(), REDACTED.to_string()))
        );
        // Header values + body — common secret homes.
        assert_eq!(
            http.headers.get("X-Api-Key").map(String::as_str),
            Some(REDACTED)
        );
        assert_eq!(http.body.as_deref(), Some(REDACTED));
        // URL userinfo / query / fragment stripped; host + path kept.
        let rendered = http.url.to_string();
        assert!(
            !rendered.contains("secret"),
            "url leaked a secret: {rendered}"
        );
        assert!(!rendered.contains('@'), "userinfo not stripped: {rendered}");
        assert_eq!(rendered, "https://api.example.com/health");
    }

    #[test]
    fn owner_redaction_masks_flow_fill_value() {
        use crate::domain::{FlowCheck, FlowStep};

        let mut check = CheckSpec::Flow(FlowCheck {
            start_url: url::Url::parse("https://app.example.com/login").unwrap(),
            steps: vec![
                FlowStep::Fill {
                    selector: "#password".into(),
                    value: "hunter2".into(),
                },
                FlowStep::AssertText {
                    selector: None,
                    contains: "Welcome".into(),
                },
            ],
            timeout: Duration::from_secs(30),
            step_timeout: Duration::from_secs(5),
            verify_tls: true,
        });
        redact_check(&mut check);
        let CheckSpec::Flow(flow) = check else {
            panic!("expected flow");
        };
        assert!(matches!(&flow.steps[0], FlowStep::Fill { value, .. } if value == REDACTED));
    }

    #[test]
    fn public_redaction_masks_flow_fill_value_and_url_credentials() {
        use crate::domain::{FlowCheck, FlowStep};

        let mut check = CheckSpec::Flow(FlowCheck {
            start_url: url::Url::parse("https://user:pass@app.example.com/login?t=secret").unwrap(),
            steps: vec![
                FlowStep::Goto {
                    url: url::Url::parse("https://user:pass@app.example.com/dash?t=secret")
                        .unwrap(),
                },
                FlowStep::Fill {
                    selector: "#password".into(),
                    value: "hunter2".into(),
                },
                FlowStep::Click {
                    selector: "#submit".into(),
                },
                FlowStep::AssertText {
                    selector: None,
                    contains: "Welcome".into(),
                },
            ],
            timeout: Duration::from_secs(30),
            step_timeout: Duration::from_secs(5),
            verify_tls: true,
        });
        redact_check_for_public(&mut check);
        let CheckSpec::Flow(flow) = check else {
            panic!("expected flow");
        };
        assert_eq!(flow.start_url.to_string(), "https://app.example.com/login");
        let FlowStep::Goto { url } = &flow.steps[0] else {
            panic!("expected goto");
        };
        assert_eq!(url.to_string(), "https://app.example.com/dash");
        assert!(matches!(&flow.steps[1], FlowStep::Fill { value, .. } if value == REDACTED));
        assert!(matches!(&flow.steps[2], FlowStep::Click { selector } if selector == "#submit"));
        assert!(
            matches!(&flow.steps[3], FlowStep::AssertText { contains, .. } if contains == "Welcome")
        );
    }
}
