//! Human-in-the-loop confirmation for write tools, via MCP elicitation.
//!
//! A client that can ask writes only on an explicit `confirm = true`. A client
//! that cannot ask (no elicitation, or "method not found" for the prompt)
//! writes on the token's scope, the same grant the REST API acts on with no
//! prompt, and the audit row says `unconfirmed`. Anything that may still be a
//! person's answer (timeout, dropped transport, unreadable form, any other
//! error code) fails closed.

use rmcp::RoleServer;
use rmcp::model::ErrorCode;
use rmcp::service::{ElicitationError, ElicitationMode, RequestContext, ServiceError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::auth::McpAuth;
use super::error::{McpToolError, codes};

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct Confirmation {
    /// Set to true to approve this action. Anything else cancels it.
    confirm: bool,
}

rmcp::elicit_safe!(Confirmation);

/// A peer that never initialized reports `None`, which is unknown, not
/// unsupported; the elicit call itself settles it.
pub fn client_can_confirm(ctx: &RequestContext<RoleServer>) -> bool {
    ctx.peer.peer_info().is_none()
        || ctx
            .peer
            .supported_elicitation_modes()
            .contains(&ElicitationMode::Form)
}

const WAY_OUT: &str = "approve the prompt in your MCP client and retry, or make this \
                       change in the Uptimepage app or through the REST API \
                       (https://uptimepage.dev/docs/api)";

const NO_ELICITATION: &str = "no_elicitation";

fn declined(detail: &'static str) -> McpToolError {
    McpToolError::new(
        codes::NOT_CONFIRMED,
        "the action was not confirmed; no change was made",
        false,
    )
    .with_detail(detail)
}

#[derive(Debug)]
enum Verdict {
    /// Software answered, not a person; the write proceeds under this reason.
    CannotAsk(String),
    Refused(McpToolError),
}

fn classify(err: &ElicitationError) -> Verdict {
    match err {
        ElicitationError::UserDeclined => Verdict::Refused(declined("declined")),
        ElicitationError::UserCancelled => Verdict::Refused(declined("cancelled")),
        ElicitationError::CapabilityNotSupported => Verdict::CannotAsk(NO_ELICITATION.into()),
        // Only -32601 proves nobody was asked; any other code may be a dialog
        // handler that threw on dismissal.
        ElicitationError::Service(ServiceError::McpError(e))
            if e.code == ErrorCode::METHOD_NOT_FOUND =>
        {
            Verdict::CannotAsk(format!("client_error:{}", e.code.0))
        }
        _ => Verdict::Refused(
            McpToolError::new(
                codes::CONFIRMATION_FAILED,
                format!(
                    "this MCP client did not answer the confirmation prompt; no change \
                     was made; {WAY_OUT}"
                ),
                false,
            )
            .with_detail(match err {
                ElicitationError::ParseError { .. } => "parse_error".to_string(),
                ElicitationError::NoContent => "no_content".to_string(),
                ElicitationError::Service(ServiceError::McpError(e)) => {
                    format!("client_error:{}", e.code.0)
                }
                ElicitationError::Service(ServiceError::Timeout { .. }) => "timed_out".to_string(),
                ElicitationError::Service(ServiceError::TransportClosed) => {
                    "transport_closed".to_string()
                }
                ElicitationError::Service(ServiceError::TransportSend(_)) => {
                    "transport_send".to_string()
                }
                ElicitationError::Service(ServiceError::Cancelled { .. }) => {
                    "cancelled".to_string()
                }
                ElicitationError::Service(ServiceError::UnexpectedResponse) => {
                    "unexpected_response".to_string()
                }
                ElicitationError::Service(_) => "service_error".to_string(),
                _ => "unknown".to_string(),
            }),
        ),
    }
}

/// `Ok(())` on an explicit approval, or unasked when no client can ask; the
/// latter is noted on `auth` for the audit row.
pub async fn require_confirmation(
    ctx: &RequestContext<RoleServer>,
    auth: &McpAuth,
    message: impl Into<String>,
) -> Result<(), McpToolError> {
    if !client_can_confirm(ctx) {
        proceed_unconfirmed(auth, NO_ELICITATION.into());
        return Ok(());
    }
    match ctx.peer.elicit::<Confirmation>(message.into()).await {
        Ok(Some(c)) if c.confirm => Ok(()),
        // Accepted the form with the box left false: a decision, not a fault.
        Ok(_) => Err(declined("unchecked")),
        // The client's error may echo the prompt, and so the incident text in
        // it: never logged.
        Err(e) => match classify(&e) {
            Verdict::CannotAsk(reason) => {
                proceed_unconfirmed(auth, reason);
                Ok(())
            }
            Verdict::Refused(mapped) => {
                tracing::warn!(
                    target: "mcp",
                    client = auth.client.as_deref().unwrap_or(""),
                    detail = mapped.audit_detail(),
                    "elicitation failed"
                );
                Err(mapped)
            }
        },
    }
}

fn proceed_unconfirmed(auth: &McpAuth, reason: String) {
    tracing::info!(
        target: "mcp",
        client = auth.client.as_deref().unwrap_or(""),
        reason,
        "write proceeds unconfirmed: client cannot relay the prompt"
    );
    auth.note_unconfirmed(reason);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_err() -> ElicitationError {
        ElicitationError::ParseError {
            error: serde_json::from_str::<u32>("\"x\"").unwrap_err(),
            data: serde_json::json!({"confirm": "x"}),
        }
    }

    fn refused(err: &ElicitationError) -> McpToolError {
        match classify(err) {
            Verdict::Refused(e) => e,
            other => panic!("{err:?} did not refuse: {other:?}"),
        }
    }

    fn cannot_ask(err: &ElicitationError) -> String {
        match classify(err) {
            Verdict::CannotAsk(reason) => reason,
            other => panic!("{err:?} did not proceed: {other:?}"),
        }
    }

    #[test]
    fn a_human_decision_and_a_broken_client_are_told_apart() {
        let declined = refused(&ElicitationError::UserDeclined);
        let broken = refused(&parse_err());

        assert_eq!(declined.code, codes::NOT_CONFIRMED);
        assert_eq!(broken.code, codes::CONFIRMATION_FAILED);
        assert_ne!(declined.audit_detail(), broken.audit_detail());
    }

    #[test]
    fn declining_and_dismissing_share_a_code_but_not_an_audit_detail() {
        let declined = refused(&ElicitationError::UserDeclined);
        let cancelled = refused(&ElicitationError::UserCancelled);

        assert_eq!(declined.code, cancelled.code);
        assert_eq!(declined.audit_detail(), "not_confirmed:declined");
        assert_eq!(cancelled.audit_detail(), "not_confirmed:cancelled");
    }

    /// The pre-change vocabulary was the bare code, so both must still match.
    #[test]
    fn a_refusal_audits_under_its_code_whether_or_not_it_carries_a_reason() {
        for refusal in [
            refused(&ElicitationError::UserDeclined),
            declined("unchecked"),
        ] {
            assert!(
                refusal.audit_detail().starts_with(codes::NOT_CONFIRMED),
                "{} would not match a not_confirmed query",
                refusal.audit_detail()
            );
        }
    }

    #[test]
    fn a_timeout_fails_closed_without_reading_as_a_human_saying_no() {
        let timed_out = refused(&ElicitationError::Service(ServiceError::Timeout {
            timeout: std::time::Duration::from_secs(30),
        }));

        assert_eq!(timed_out.code, codes::CONFIRMATION_FAILED);
        assert_eq!(timed_out.audit_detail(), "confirmation_failed:timed_out");
        assert!(!timed_out.retryable);
        assert!(
            timed_out.message.contains("REST API"),
            "{}",
            timed_out.message
        );
    }

    #[test]
    fn a_client_that_never_implemented_the_prompt_proceeds_on_scope_alone() {
        let reason = cannot_ask(&ElicitationError::Service(ServiceError::McpError(
            rmcp::model::ErrorData::new(ErrorCode::METHOD_NOT_FOUND, "Method not found", None),
        )));

        assert_eq!(reason, "client_error:-32601");
    }

    #[test]
    fn any_other_client_error_fails_closed_with_its_code() {
        let thrown = refused(&ElicitationError::Service(ServiceError::McpError(
            rmcp::model::ErrorData::new(ErrorCode::INTERNAL_ERROR, "user did not respond", None),
        )));

        assert_eq!(thrown.code, codes::CONFIRMATION_FAILED);
        assert_eq!(
            thrown.audit_detail(),
            "confirmation_failed:client_error:-32603"
        );
        assert!(!thrown.retryable);
        assert!(
            !thrown.message.contains("user did not respond"),
            "{}",
            thrown.message
        );
    }

    #[test]
    fn a_client_without_form_mode_proceeds_on_scope_alone() {
        assert_eq!(
            cannot_ask(&ElicitationError::CapabilityNotSupported),
            NO_ELICITATION
        );
    }

    #[test]
    fn transport_faults_are_told_apart_in_the_audit_trail() {
        let closed = refused(&ElicitationError::Service(ServiceError::TransportClosed));
        let silent = refused(&ElicitationError::NoContent);

        assert_eq!(
            closed.audit_detail(),
            "confirmation_failed:transport_closed"
        );
        assert_eq!(silent.audit_detail(), "confirmation_failed:no_content");
    }

    #[test]
    fn every_refusal_is_final() {
        for err in [
            ElicitationError::UserDeclined,
            ElicitationError::UserCancelled,
            ElicitationError::NoContent,
            parse_err(),
        ] {
            assert!(!refused(&err).retryable, "{err:?} offered a retry");
        }
    }

    #[test]
    fn the_clients_own_form_response_never_reaches_the_audit_trail() {
        let secret = "sk-live-must-not-be-logged";
        let mapped = refused(&ElicitationError::ParseError {
            error: serde_json::from_str::<u32>("\"x\"").unwrap_err(),
            data: serde_json::json!({ "confirm": secret }),
        });

        assert!(!mapped.audit_detail().contains(secret));
        assert!(!mapped.message.contains(secret));
    }
}
