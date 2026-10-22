use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Days, Utc};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Bytes;
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap};
use hyper::{Request, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::json;

use super::templates::single_line;
use super::trait_def::{
    BatchFailure, BatchResult, EmailAddress, EmailError, EmailResult, EmailSender, MessageId,
    PreparedBatch, SendClass, TransactionalEmail, check_batch_size,
};
use crate::http_outbound::{OutboundHttpClient, REQUEST_TIMEOUT, error_chain};

const RESEND_API_URL: &str = "https://api.resend.com/emails";
const RESEND_BATCH_URL: &str = "https://api.resend.com/emails/batch";
const MAX_RESEND_RESPONSE_BYTES: usize = 64 * 1024;
/// What a 429 without a usable `retry-after` waits.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(1);
/// Resend's limit is per second, so a longer wait is a broken or foreign
/// header; honoured as asked it would hold every instance's sign-in links
/// and alerts for as long.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

pub struct ResendEmailSender {
    api_key: SecretString,
    site_name: String,
    http: OutboundHttpClient,
}

impl ResendEmailSender {
    pub fn new(
        api_key: SecretString,
        site_name: impl Into<String>,
        http: OutboundHttpClient,
    ) -> Self {
        Self {
            api_key,
            site_name: site_name.into(),
            http,
        }
    }

    /// One mail as Resend's send and batch endpoints both take it.
    fn message(&self, email: &TransactionalEmail) -> serde_json::Value {
        let rendered = email.template.render(&self.site_name);
        let mut body = json!({
            "from": from_header(&email.from),
            "to": [email.to.address],
            "subject": rendered.subject,
            "text": rendered.text_body,
            "html": rendered.html_body,
        });
        if let Some(addr) = email.template.reply_to() {
            body["reply_to"] = json!(single_line(addr));
        }
        let mut headers = serde_json::Map::new();
        // RFC 8058 one-click unsubscribe for public-subscriber mail: a stable
        // header link plus the One-Click marker so Gmail/Outlook render it.
        if let Some(url) = email.template.list_unsubscribe_url() {
            headers.insert("List-Unsubscribe".into(), json!(format!("<{url}>")));
            headers.insert(
                "List-Unsubscribe-Post".into(),
                json!("List-Unsubscribe=One-Click"),
            );
        }
        for (name, value) in email.template.custom_headers() {
            headers.insert(name.into(), json!(single_line(&value)));
        }
        if !headers.is_empty() {
            body["headers"] = serde_json::Value::Object(headers);
        }
        body
    }

    /// The response body of a 2xx, or the provider's answer as its kind.
    async fn post(
        &self,
        url: &str,
        payload: Bytes,
        batch_key: Option<&str>,
    ) -> Result<Bytes, EmailError> {
        let mut req = Request::post(url)
            .header(CONTENT_TYPE, "application/json")
            .header(
                AUTHORIZATION,
                format!("Bearer {}", self.api_key.expose_secret()),
            );
        if let Some(key) = batch_key {
            // Permissive: one bad address fails its own entry, not the 99
            // beside it.
            req = req
                .header("Idempotency-Key", key)
                .header("x-batch-validation", "permissive");
        }
        let req = req
            .body(Full::new(payload))
            .map_err(|e| EmailError::Transport(format!("build request: {e}")))?;

        // Runs while the caller holds one of sixteen global paging permits, so
        // a provider that answers and then stalls stops alerting for everyone.
        let at = tokio::time::Instant::now() + REQUEST_TIMEOUT;
        let resp = tokio::time::timeout_at(at, self.http.request(req))
            .await
            .map_err(|_| EmailError::Transport(format!("no response within {REQUEST_TIMEOUT:?}")))?
            .map_err(|e| EmailError::Transport(error_chain(e)))?;
        let status = resp.status();
        let retry_after = parse_retry_after(resp.headers());
        let collected = tokio::time::timeout_at(
            at,
            Limited::new(resp.into_body(), MAX_RESEND_RESPONSE_BYTES).collect(),
        )
        .await;

        if !status.is_success() {
            // A stalled body must not replace a 429: the retry decision
            // reads this.
            let body = collected
                .ok()
                .and_then(std::result::Result::ok)
                .map(|c| c.to_bytes())
                .unwrap_or_default();
            // Provider body may echo recipient/subject; surface enough for
            // operator triage without committing to a stable schema.
            let text = String::from_utf8_lossy(&body);
            return Err(classify(status, retry_after, &text, Utc::now()));
        }

        Ok(collected
            .map_err(|_| EmailError::Transport(format!("no body within {REQUEST_TIMEOUT:?}")))?
            .map_err(|e| EmailError::Transport(format!("read body: {e}")))?
            .to_bytes())
    }
}

#[derive(Debug, Deserialize)]
struct ResendResponse {
    id: String,
}

#[derive(Debug, Deserialize)]
struct BatchResponse {
    data: Vec<ResendResponse>,
    #[serde(default)]
    errors: Vec<BatchEntryError>,
}

#[derive(Debug, Deserialize)]
struct BatchEntryError {
    index: usize,
    message: String,
}

impl BatchResponse {
    /// `data` lists ids only for the entries that were taken, in order;
    /// `errors` names the positions that were not.
    fn per_mail(self, len: usize) -> Vec<EmailResult<MessageId>> {
        let mut failed: HashMap<usize, String> = self
            .errors
            .into_iter()
            .map(|e| (e.index, e.message))
            .collect();
        let mut ids = self.data.into_iter();
        (0..len)
            .map(|i| match failed.remove(&i) {
                Some(message) => Err(EmailError::Rejected(message)),
                None => ids.next().map(|r| MessageId(r.id)).ok_or_else(|| {
                    EmailError::Transport("batch answer has no id for this mail".into())
                }),
            })
            .collect()
    }
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    name: String,
}

/// What a non-2xx answer means for a retry.
fn classify(
    status: StatusCode,
    retry_after: Option<Duration>,
    body: &str,
    now: DateTime<Utc>,
) -> EmailError {
    let name = serde_json::from_str::<ErrorBody>(body)
        .map(|b| b.name)
        .unwrap_or_default();
    let detail = format!("{status}: {body}");
    match (status.as_u16(), name.as_str()) {
        (429, "daily_quota_exceeded") => EmailError::QuotaExceeded {
            until: next_utc_midnight(now),
            detail,
        },
        (429, "monthly_quota_exceeded") => EmailError::QuotaExceeded {
            until: None,
            detail,
        },
        (429, _) => EmailError::RateLimited {
            retry_after: retry_after.unwrap_or(DEFAULT_RETRY_AFTER),
            detail,
        },
        (409, "concurrent_idempotent_requests") => EmailError::InProgress(detail),
        // Another request is updating the same resource: try again shortly.
        (409, "resource_locked") => EmailError::Transport(detail),
        (401 | 403 | 404 | 405, _) => EmailError::Refused(detail),
        (500.., _) => EmailError::Transport(detail),
        _ => EmailError::Rejected(detail),
    }
}

fn next_utc_midnight(now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    now.date_naive()
        .checked_add_days(Days::new(1))
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|t| t.and_utc())
}

/// Seconds, as Resend sends it. A date or garbage reads as absent.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    crate::http_outbound::retry_after(headers).map(|wait| wait.min(MAX_RETRY_AFTER))
}

#[async_trait]
impl EmailSender for ResendEmailSender {
    async fn send(&self, email: TransactionalEmail, _class: SendClass) -> EmailResult<MessageId> {
        let payload = serde_json::to_vec(&self.message(&email))
            .map_err(|e| EmailError::Transport(format!("serialize: {e}")))?;
        let body = self.post(RESEND_API_URL, payload.into(), None).await?;
        let parsed: ResendResponse = serde_json::from_slice(&body)
            .map_err(|e| EmailError::Transport(format!("parse body: {e}")))?;
        Ok(MessageId(parsed.id))
    }

    fn prepare_batch(
        &self,
        emails: Vec<TransactionalEmail>,
        key: String,
    ) -> EmailResult<PreparedBatch> {
        check_batch_size(emails.len())?;
        let messages: Vec<serde_json::Value> = emails.iter().map(|e| self.message(e)).collect();
        let body = serde_json::to_vec(&messages)
            .map_err(|e| EmailError::Transport(format!("serialize: {e}")))?;
        Ok(PreparedBatch::from_parts(key, body.into(), emails.len()))
    }

    async fn send_prepared(&self, batch: PreparedBatch) -> BatchResult {
        let answer = self
            .post(RESEND_BATCH_URL, batch.body().clone(), Some(batch.key()))
            .await
            .and_then(|body| {
                serde_json::from_slice::<BatchResponse>(&body)
                    .map_err(|e| EmailError::Transport(format!("parse body: {e}")))
            });
        match answer {
            Ok(parsed) => Ok(parsed.per_mail(batch.len())),
            Err(error) => Err(BatchFailure { error, batch }),
        }
    }
}

/// RFC 5322 name-addr. The display name goes bare unless it holds a special
/// (`,` `<` `"` and the rest), in which case it becomes a quoted-string so the
/// mailbox cannot be split; `single_line` owns the control-character rule.
fn from_header(from: &EmailAddress) -> String {
    let name = single_line(&from.name);
    if name.is_empty() {
        return from.address.clone();
    }
    if !name.chars().any(|c| "()<>[]:;@\\,.\"".contains(c)) {
        return format!("{name} <{}>", from.address);
    }
    let mut quoted = String::with_capacity(name.len() + 2);
    quoted.push('"');
    for c in name.chars() {
        if c == '"' || c == '\\' {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    format!("{quoted} <{}>", from.address)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::{HeaderValue, RETRY_AFTER};

    #[test]
    fn from_header_quotes_the_display_name_only_when_it_must() {
        let plain = EmailAddress::new("hello@example.test", "Uptimepage");
        assert_eq!(from_header(&plain), "Uptimepage <hello@example.test>");

        let comma = EmailAddress::new("hello@example.test", "Acme, Inc.");
        assert_eq!(from_header(&comma), "\"Acme, Inc.\" <hello@example.test>");

        let tricky = EmailAddress::new("hello@example.test", "Acme \"Status\" <x>\r\nBcc: y");
        assert_eq!(
            from_header(&tricky),
            "\"Acme \\\"Status\\\" <x> Bcc: y\" <hello@example.test>"
        );

        let bare = EmailAddress::new("hello@example.test", "");
        assert_eq!(from_header(&bare), "hello@example.test");
    }

    fn kind(status: u16, name: &str) -> EmailError {
        let body = format!(r#"{{"name":"{name}","message":"m","statusCode":{status}}}"#);
        let at = "2026-10-21T19:03:51Z".parse().unwrap();
        classify(StatusCode::from_u16(status).unwrap(), None, &body, at)
    }

    #[test]
    fn each_answer_maps_to_the_kind_a_retry_reads() {
        assert!(matches!(
            kind(429, "rate_limit_exceeded"),
            EmailError::RateLimited { retry_after, .. } if retry_after == DEFAULT_RETRY_AFTER
        ));
        let EmailError::QuotaExceeded { until, .. } = kind(429, "daily_quota_exceeded") else {
            panic!("daily quota");
        };
        assert_eq!(until, Some("2026-10-22T00:00:00Z".parse().unwrap()));
        assert!(matches!(
            kind(429, "monthly_quota_exceeded"),
            EmailError::QuotaExceeded { until: None, .. }
        ));
        assert!(matches!(
            kind(409, "concurrent_idempotent_requests"),
            EmailError::InProgress(_)
        ));
        assert!(matches!(
            kind(409, "invalid_idempotent_request"),
            EmailError::Rejected(_)
        ));
        assert!(matches!(
            kind(409, "resource_locked"),
            EmailError::Transport(_)
        ));
        assert!(matches!(
            kind(403, "validation_error"),
            EmailError::Refused(_)
        ));
        assert!(matches!(
            kind(401, "missing_api_key"),
            EmailError::Refused(_)
        ));
        assert!(matches!(
            kind(422, "invalid_parameter"),
            EmailError::Rejected(_)
        ));
        assert!(matches!(
            kind(400, "validation_error"),
            EmailError::Rejected(_)
        ));
        assert!(matches!(
            kind(500, "application_error"),
            EmailError::Transport(_)
        ));
        assert!(matches!(
            kind(503, "service_unavailable"),
            EmailError::Transport(_)
        ));
    }

    #[test]
    fn a_rate_limit_waits_what_the_header_asks_within_a_cap() {
        let at = Utc::now();
        let header = |v: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(RETRY_AFTER, HeaderValue::from_str(v).unwrap());
            parse_retry_after(&headers)
        };
        assert_eq!(header("2"), Some(Duration::from_secs(2)));
        assert_eq!(header("0.5"), Some(Duration::from_millis(500)));
        assert_eq!(header("999999"), Some(MAX_RETRY_AFTER));
        assert_eq!(header("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(header("-1"), None);
        assert_eq!(header("1e30"), None);
        let e = classify(StatusCode::TOO_MANY_REQUESTS, header("3"), "not json", at);
        assert!(
            matches!(e, EmailError::RateLimited { retry_after, .. } if retry_after.as_secs() == 3)
        );
    }

    #[test]
    fn a_rate_limit_keeps_the_text_the_engine_reads_its_wait_from() {
        let e = EmailError::RateLimited {
            retry_after: Duration::from_millis(1500),
            detail: "429 Too Many Requests: {}".into(),
        };
        let text = e.to_string();
        assert!(text.starts_with("email provider rejected: 429"), "{text}");
        assert_eq!(
            crate::notifier::transport::json_int_field(&text, "retry_after"),
            Some(1)
        );
    }

    #[test]
    fn a_permissive_batch_answer_maps_ids_around_the_failed_positions() {
        let answer: BatchResponse = serde_json::from_str(
            r#"{"data":[{"id":"a"},{"id":"b"}],"errors":[{"index":1,"message":"Invalid email address."}]}"#,
        )
        .unwrap();
        let out = answer.per_mail(3);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].as_ref().unwrap().0, "a");
        assert!(matches!(&out[1], Err(EmailError::Rejected(m)) if m == "Invalid email address."));
        assert_eq!(out[2].as_ref().unwrap().0, "b");

        let strict: BatchResponse = serde_json::from_str(r#"{"data":[{"id":"x"}]}"#).unwrap();
        let short = strict.per_mail(2);
        assert_eq!(short[0].as_ref().unwrap().0, "x");
        assert!(matches!(&short[1], Err(EmailError::Transport(_))));
    }
}
