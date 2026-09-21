//! Product email: sign-in links, invitations, account notices, and the
//! sender behind `crate::notifier`'s alert mail and status-page subscriber
//! updates. Provider is pluggable per the top-level `[email]` config table.

pub mod log_only;
pub mod memory;
pub mod resend;
pub mod svix;
pub mod templates;
pub mod trait_def;

use std::sync::Arc;

use crate::config::{EmailProvider, TransactionalEmailConfig};
use crate::http_outbound::OutboundHttpClient;

pub use log_only::LogOnlyEmailSender;
pub(crate) use log_only::mask_email;
pub use memory::InMemoryEmailSender;
pub use resend::ResendEmailSender;
pub use trait_def::{
    EmailAddress, EmailError, EmailResult, EmailSender, EmailTemplate, MessageId, RenderedEmail,
    TransactionalEmail,
};

/// Builds an `EmailSender` from config. `http` is the shared outbound client
/// (the same one used by Slack/webhook notifiers) — only the `resend`
/// provider actually uses it; `log` and `memory` ignore it. Whether the
/// config is complete is the validator's question, asked at boot.
pub fn build_email_sender(
    config: &TransactionalEmailConfig,
    http: &OutboundHttpClient,
) -> Arc<dyn EmailSender> {
    match config.provider {
        EmailProvider::Resend => Arc::new(ResendEmailSender::new(
            config.resend.api_key.clone(),
            config.from_name.clone(),
            http.clone(),
        )),
        EmailProvider::Log => Arc::new(LogOnlyEmailSender::new(config.from_name.clone())),
        EmailProvider::Memory => Arc::new(InMemoryEmailSender::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn sample_invitation() -> TransactionalEmail {
        TransactionalEmail {
            to: EmailAddress::new("alice@example.com", "Alice"),
            from: EmailAddress::new("no-reply@example.invalid", "Uptimepage"),
            template: EmailTemplate::Invitation {
                org_name: "Acme".into(),
                inviter_display: "Bob <bob@acme.test>".into(),
                accept_url: "https://example.test/invitations/accept?token=abc".into(),
                decline_url: "https://example.test/invitations/decline?token=abc".into(),
                expires_at: Utc::now() + Duration::days(7),
            },
        }
    }

    fn sample_magic_link() -> TransactionalEmail {
        TransactionalEmail {
            to: EmailAddress::new("alice@example.com", "Alice"),
            from: EmailAddress::new("no-reply@example.invalid", "Uptimepage"),
            template: EmailTemplate::MagicLink {
                url: "https://example.test/auth/magic-link/verify?token=xyz".into(),
                expires_in_minutes: 15,
                code: "4KP9RT".into(),
                ip_hint: Some("203.0.113.5".into()),
                opens_accounts: false,
            },
        }
    }

    #[test]
    fn invitation_render_includes_action_url_and_org() {
        let email = sample_invitation();
        let rendered = email.template.render("Uptimepage");
        // Named by whoever sent the invitation, so they stay out of the
        // subject. See `templates::invitation`.
        assert!(!rendered.subject.contains("Acme"));
        assert!(!rendered.subject.contains("Bob"));
        assert!(rendered.text_body.contains("Acme"));
        assert!(rendered.text_body.contains("Bob"));
        assert!(
            rendered
                .text_body
                .contains("https://example.test/invitations/accept?token=abc")
        );
        assert!(rendered.text_body.contains("decline"));
        assert!(rendered.html_body.contains("Acme"));
        assert!(rendered.html_body.starts_with("<!doctype html>"));
        // Inviter display name contains '<' and '>'; HTML body must escape both.
        assert!(rendered.html_body.contains("Bob &lt;bob@acme.test&gt;"));
        assert!(!rendered.html_body.contains("<bob@acme.test>"));
    }

    #[test]
    fn magic_link_render_produces_subject_text_and_html() {
        let email = sample_magic_link();
        let rendered = email.template.render("Uptimepage");
        assert!(rendered.subject.contains("Sign in"));
        assert!(
            rendered
                .text_body
                .contains("https://example.test/auth/magic-link/verify?token=xyz")
        );
        assert!(rendered.text_body.contains("15 minutes"));
        assert!(rendered.text_body.contains("203.0.113.5"));
        assert!(rendered.html_body.contains("Sign in"));
    }

    #[test]
    fn primary_url_for_each_template() {
        assert_eq!(
            sample_invitation().template.primary_url().unwrap(),
            "https://example.test/invitations/accept?token=abc",
        );
        assert_eq!(
            sample_magic_link().template.primary_url().unwrap(),
            "https://example.test/auth/magic-link/verify?token=xyz",
        );
    }

    #[tokio::test]
    async fn log_only_sender_emits_tracing_and_returns_id() {
        let sender = LogOnlyEmailSender::new("Uptimepage [TEST]");
        let id = sender
            .send(sample_invitation())
            .await
            .expect("log-only send");
        assert!(id.0.starts_with("log-only-"));
    }

    #[tokio::test]
    async fn in_memory_sender_captures_sent_emails() {
        let sender = InMemoryEmailSender::new();
        assert!(sender.is_empty());
        let id1 = sender.send(sample_invitation()).await.unwrap();
        let id2 = sender.send(sample_magic_link()).await.unwrap();
        assert_eq!(id1.0, "memory-0");
        assert_eq!(id2.0, "memory-1");

        let captured = sender.sent();
        assert_eq!(captured.len(), 2);
        match &captured[0].template {
            EmailTemplate::Invitation { org_name, .. } => assert_eq!(org_name, "Acme"),
            _ => panic!("expected invitation in slot 0"),
        }
        match &captured[1].template {
            EmailTemplate::MagicLink { url, .. } => {
                assert!(url.contains("magic-link/verify"));
            }
            _ => panic!("expected magic link in slot 1"),
        }
    }

    use crate::http_outbound::build_outbound_client;

    #[test]
    fn factory_log_provider() {
        let cfg = TransactionalEmailConfig {
            provider: EmailProvider::Log,
            ..Default::default()
        };
        let http = build_outbound_client(crate::security::SsrfGuard::strict());
        let sender = build_email_sender(&cfg, &http);
        let _ = format!("{:p}", Arc::as_ptr(&sender));
    }

    #[test]
    fn factory_memory_provider() {
        let cfg = TransactionalEmailConfig {
            provider: EmailProvider::Memory,
            ..Default::default()
        };
        let http = build_outbound_client(crate::security::SsrfGuard::strict());
        let sender = build_email_sender(&cfg, &http);
        let _ = sender;
    }

    #[test]
    fn factory_resend_constructs() {
        let cfg = TransactionalEmailConfig {
            provider: EmailProvider::Resend,
            resend: crate::config::ResendConfig {
                api_key: secrecy::SecretString::from("re_test_key".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let http = build_outbound_client(crate::security::SsrfGuard::strict());
        let sender = build_email_sender(&cfg, &http);
        let _ = sender;
    }
}
