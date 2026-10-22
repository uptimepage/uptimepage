use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::trait_def::{
    BatchFailure, BatchResult, EmailError, EmailResult, EmailSender, MessageId, PreparedBatch,
    SendClass, TransactionalEmail, check_batch_size,
};

pub struct LogOnlyEmailSender {
    site_name: String,
}

impl LogOnlyEmailSender {
    pub fn new(site_name: impl Into<String>) -> Self {
        Self {
            site_name: site_name.into(),
        }
    }
}

impl Default for LogOnlyEmailSender {
    fn default() -> Self {
        Self::new("Uptimepage [DEV]")
    }
}

/// Query parameters that carry an account-takeover credential. A 30-day
/// recovery link, a magic-link or an invite token must never land in a
/// 30-day-retained log, so the dev `log` provider truncates these to a short
/// prefix before logging. The rendered body is never logged for the same
/// reason — it embeds the same URL.
const SENSITIVE_PARAMS: [&str; 4] = ["token", "state", "recovery", "code"];

/// Local work on an emailed flow, where the truncated URL is unusable and the
/// DB keeps only an argon2 hash. Anyone who can read the log can then take over
/// the account it was addressed to, so nothing infers this from a log level.
const ENV_SHOW_LINKS: &str = "UPTIMEPAGE_DEV_EMAIL_LINKS";

fn show_links_in_full() -> bool {
    std::env::var(ENV_SHOW_LINKS).is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Truncate the value of any [`SENSITIVE_PARAMS`] query key to its first 6
/// chars + `…`. Only matches a real query key (`?key=` / `&key=`); leaves the
/// host/path intact so the dev still sees which flow fired.
fn redact_sensitive_params(url: &str) -> String {
    let mut out = url.to_string();
    for key in SENSITIVE_PARAMS {
        let mut from = 0;
        while let Some(rel) = out[from..].find(key) {
            let k = from + rel;
            let pre_ok = k == 0 || matches!(out.as_bytes()[k - 1], b'?' | b'&');
            let eq = k + key.len();
            if pre_ok && out.as_bytes().get(eq) == Some(&b'=') {
                let vstart = eq + 1;
                let vend = out[vstart..]
                    .find(|c: char| c == '&' || c.is_whitespace())
                    .map_or(out.len(), |r| vstart + r);
                if vend - vstart > 6 {
                    let keep: String = out[vstart..vend].chars().take(6).collect();
                    let replaced = format!("{keep}…");
                    out.replace_range(vstart..vend, &replaced);
                    from = vstart + replaced.len();
                    continue;
                }
            }
            from = k + key.len();
        }
    }
    out
}

/// `jane@example.com` → `j***@example.com`. Keeps the domain (operationally
/// useful) but not the identity.
pub(crate) fn mask_email(addr: &str) -> String {
    match addr.split_once('@') {
        Some((local, domain)) => {
            let first = local.chars().next().unwrap_or('*');
            format!("{first}***@{domain}")
        }
        None => "***".to_string(),
    }
}

/// What a log-only batch keeps of each mail: enough to say it went.
#[derive(Serialize, Deserialize)]
struct LoggedMail {
    to: String,
    subject: String,
}

#[async_trait]
impl EmailSender for LogOnlyEmailSender {
    async fn send(&self, email: TransactionalEmail, _class: SendClass) -> EmailResult<MessageId> {
        let rendered = email.template.render(&self.site_name);
        // Recipient masked; the rendered body is deliberately not logged.
        let to = mask_email(&email.to.address);
        tracing::info!(
            %to,
            subject = %rendered.subject,
            "📧 EMAIL (not actually sent — log-only mode)"
        );

        if let Some(url) = email.template.primary_url() {
            if show_links_in_full() {
                tracing::warn!(
                    // SAFE: unredacted on purpose and only when the operator
                    // sets ENV_SHOW_LINKS on a deployment that sends no mail.
                    // Warn level so the log says what it now contains.
                    action_url = %url,
                    "📧 Action URL IN FULL — {ENV_SHOW_LINKS} is set, so this log holds a live credential"
                );
            } else {
                // SAFE: redacted — token/state/recovery/code params truncated
                tracing::info!(action_url = %redact_sensitive_params(url), "📧 Action URL");
            }
        }

        Ok(MessageId(format!("log-only-{}", Uuid::now_v7())))
    }

    fn prepare_batch(
        &self,
        emails: Vec<TransactionalEmail>,
        key: String,
    ) -> EmailResult<PreparedBatch> {
        check_batch_size(emails.len())?;
        let logged: Vec<LoggedMail> = emails
            .iter()
            .map(|e| LoggedMail {
                to: mask_email(&e.to.address),
                subject: e.template.render(&self.site_name).subject,
            })
            .collect();
        let body = serde_json::to_vec(&logged)
            .map_err(|e| EmailError::Transport(format!("serialize: {e}")))?;
        Ok(PreparedBatch::from_parts(key, body.into(), emails.len()))
    }

    async fn send_prepared(&self, batch: PreparedBatch) -> BatchResult {
        let logged: Vec<LoggedMail> = match serde_json::from_slice(batch.body()) {
            Ok(logged) => logged,
            Err(e) => {
                return Err(BatchFailure {
                    error: EmailError::Transport(format!("parse batch: {e}")),
                    batch,
                });
            }
        };
        Ok(logged
            .into_iter()
            .map(|mail| {
                tracing::info!(
                    to = %mail.to,
                    subject = %mail.subject,
                    batch = %batch.key(),
                    "📧 EMAIL (not actually sent — log-only mode)"
                );
                Ok(MessageId(format!("log-only-{}", Uuid::now_v7())))
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{mask_email, redact_sensitive_params};

    #[test]
    fn redacts_account_takeover_params_keeps_host_path() {
        let url = "https://app.example.com/auth/magic-link/verify?token=abcdef1234567890&foo=bar";
        let out = redact_sensitive_params(url);
        assert!(out.starts_with("https://app.example.com/auth/magic-link/verify?token=abcdef…"));
        assert!(out.contains("&foo=bar"), "non-sensitive params survive");
        assert!(!out.contains("abcdef1234567890"), "raw token must be gone");
    }

    #[test]
    fn redacts_state_and_recovery_and_first_param() {
        let out =
            redact_sensitive_params("https://h/v?state=longstatevalue&recovery=longrecoveryvalue");
        assert!(out.contains("state=longst…"));
        assert!(out.contains("recovery=longre…"));
        assert!(!out.contains("longstatevalue"));
        assert!(!out.contains("longrecoveryvalue"));
    }

    #[test]
    fn short_value_and_substring_collisions_are_left_alone() {
        // value <= 6 chars: nothing to gain truncating, left intact.
        assert_eq!(
            redact_sensitive_params("https://h/v?token=abc"),
            "https://h/v?token=abc"
        );
        // `state` only as a substring of a path/param name, never a key.
        let s = "https://h/realestate?next_thing=ok";
        assert_eq!(redact_sensitive_params(s), s);
    }

    #[test]
    fn multibyte_value_truncates_on_char_boundary() {
        // 9 multibyte chars; must not panic and must drop the tail.
        let out = redact_sensitive_params("https://h/v?token=日本語日本語日本語");
        assert!(out.contains("token=日本語日本語…"));
        assert!(!out.contains("日本語日本語日本語"));
    }

    #[test]
    fn mask_email_keeps_domain_not_identity() {
        assert_eq!(mask_email("jane.doe@example.com"), "j***@example.com");
        assert_eq!(mask_email("no-at-sign"), "***");
    }
}
