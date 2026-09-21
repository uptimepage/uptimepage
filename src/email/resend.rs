use async_trait::async_trait;
use http_body_util::{BodyExt, Full, Limited};
use hyper::Request;
use hyper::body::Bytes;
use hyper::header::{AUTHORIZATION, CONTENT_TYPE};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::json;

use super::templates::single_line;
use super::trait_def::{
    EmailAddress, EmailError, EmailResult, EmailSender, MessageId, TransactionalEmail,
};
use crate::http_outbound::{OutboundHttpClient, REQUEST_TIMEOUT, error_chain};

const RESEND_API_URL: &str = "https://api.resend.com/emails";
const MAX_RESEND_RESPONSE_BYTES: usize = 64 * 1024;

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
}

#[derive(Debug, Deserialize)]
struct ResendResponse {
    id: String,
}

#[async_trait]
impl EmailSender for ResendEmailSender {
    async fn send(&self, email: TransactionalEmail) -> EmailResult<MessageId> {
        let rendered = email.template.render(&self.site_name);
        let from_value = from_header(&email.from);

        let mut body = json!({
            "from": from_value,
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
        let payload = serde_json::to_vec(&body)
            .map_err(|e| EmailError::Transport(format!("serialize: {e}")))?;

        let req = Request::post(RESEND_API_URL)
            .header(CONTENT_TYPE, "application/json")
            .header(
                AUTHORIZATION,
                format!("Bearer {}", self.api_key.expose_secret()),
            )
            .body(Full::new(Bytes::from(payload)))
            .map_err(|e| EmailError::Transport(format!("build request: {e}")))?;

        // Runs while the caller holds one of sixteen global paging permits, so
        // a provider that answers and then stalls stops alerting for everyone.
        let at = tokio::time::Instant::now() + REQUEST_TIMEOUT;
        let resp = tokio::time::timeout_at(at, self.http.request(req))
            .await
            .map_err(|_| EmailError::Transport(format!("no response within {REQUEST_TIMEOUT:?}")))?
            .map_err(|e| EmailError::Transport(error_chain(e)))?;
        let status = resp.status();
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
            return Err(EmailError::ProviderRejected(format!("{status}: {text}")));
        }

        let body = collected
            .map_err(|_| EmailError::Transport(format!("no body within {REQUEST_TIMEOUT:?}")))?
            .map_err(|e| EmailError::Transport(format!("read body: {e}")))?
            .to_bytes();
        let parsed: ResendResponse = serde_json::from_slice(&body)
            .map_err(|e| EmailError::Transport(format!("parse body: {e}")))?;
        Ok(MessageId(parsed.id))
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
}
