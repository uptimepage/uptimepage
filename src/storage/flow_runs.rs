//! The single place a flow run's page snapshot is stripped of an org's secrets
//! before it is stored.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::OrgId;
use crate::domain::agent_wire::FlowRunRecord;
use crate::security::redaction::{redact_secrets, scrub_flow_evidence, secret_values};
use crate::storage::traits::FlowRunSink;
use crate::storage::variables::VariableStore;

/// The node that captured the page cannot scrub it: the control plane
/// substituted the secrets before the spec was ever dispatched, so only here
/// still knows which values they were.
pub struct ScrubbedFlowRunSink {
    inner: Arc<dyn FlowRunSink>,
    variables: Arc<dyn VariableStore>,
}

impl ScrubbedFlowRunSink {
    pub fn new(inner: Arc<dyn FlowRunSink>, variables: Arc<dyn VariableStore>) -> Self {
        Self { inner, variables }
    }

    /// Only runs carrying a page cost a lookup, so a passing run costs none.
    async fn scrubbed(&self, runs: &[FlowRunRecord]) -> Vec<FlowRunRecord> {
        let mut out = runs.to_vec();
        let mut by_org: HashMap<Uuid, Vec<String>> = HashMap::new();
        for run in &mut out {
            let Some(evidence) = run.evidence.as_mut() else {
                continue;
            };
            let secrets = match by_org.get(&run.org_id) {
                Some(s) => s,
                None => {
                    let vars = self
                        .variables
                        .resolve_map(OrgId(run.org_id))
                        .await
                        .unwrap_or_default();
                    by_org.entry(run.org_id).or_insert(secret_values(&vars))
                }
            };
            scrub_flow_evidence(evidence, secrets);
            if let Some(err) = run.error.as_mut() {
                redact_secrets(err, secrets);
            }
        }
        out
    }
}

#[async_trait]
impl FlowRunSink for ScrubbedFlowRunSink {
    async fn write_runs(&self, runs: &[FlowRunRecord]) {
        self.inner.write_runs(&self.scrubbed(runs).await).await;
    }

    async fn write_runs_tagged(&self, runs: &[FlowRunRecord], region: &str) {
        self.inner
            .write_runs_tagged(&self.scrubbed(runs).await, region)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::domain::agent_wire::{ConsoleLine, FlowEvidence};
    use crate::domain::{CheckStatus, NewVariable};
    use crate::storage::InMemoryVariableStore;
    use crate::storage::variables::CreateVariableOutcome;

    #[derive(Default)]
    struct CapturingSink {
        seen: Mutex<Vec<FlowRunRecord>>,
    }

    #[async_trait]
    impl FlowRunSink for CapturingSink {
        async fn write_runs(&self, runs: &[FlowRunRecord]) {
            self.seen.lock().unwrap().extend_from_slice(runs);
        }
    }

    const SECRET: &str = "sk-live-pa55word";

    async fn store_with_secret(org: OrgId) -> Arc<InMemoryVariableStore> {
        let store = Arc::new(InMemoryVariableStore::new());
        let outcome = store
            .create(
                org,
                NewVariable {
                    key: "password".into(),
                    is_secret: true,
                    value: SECRET.into(),
                },
                None,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, CreateVariableOutcome::Created(_)));
        store
    }

    fn run(org: OrgId) -> FlowRunRecord {
        FlowRunRecord {
            org_id: org.0,
            target_id: Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            status: CheckStatus::Down,
            duration_ms: 100,
            error: Some(format!("step 2/3 fill: rejected {SECRET}")),
            steps: Vec::new(),
            evidence: Some(FlowEvidence {
                final_url: Some(format!("https://app.example.com/login?pw={SECRET}")),
                title: Some(format!("Rejected {SECRET}")),
                text_snippet: Some(format!("The password {SECRET} is not valid")),
                console: vec![ConsoleLine {
                    level: "error".into(),
                    text: format!("auth failed for {SECRET}"),
                }],
            }),
        }
    }

    #[tokio::test]
    async fn a_typed_secret_never_reaches_the_inner_sink() {
        let org = OrgId(Uuid::new_v4());
        let inner = Arc::new(CapturingSink::default());
        let sink = ScrubbedFlowRunSink::new(inner.clone(), store_with_secret(org).await);

        sink.write_runs(&[run(org)]).await;

        let seen = inner.seen.lock().unwrap();
        let stored = serde_json::to_string(&*seen).unwrap();
        assert!(
            !stored.contains(SECRET),
            "a resolved secret survived into storage: {stored}"
        );
        let ev = seen[0].evidence.as_ref().unwrap();
        assert!(ev.text_snippet.as_ref().unwrap().contains("***"));
        assert!(ev.final_url.as_ref().unwrap().contains("***"));
        assert!(ev.title.as_ref().unwrap().contains("***"));
        assert!(ev.console[0].text.contains("***"));
        assert!(seen[0].error.as_ref().unwrap().contains("***"));
    }

    #[tokio::test]
    async fn a_run_without_a_page_passes_straight_through() {
        let org = OrgId(Uuid::new_v4());
        let inner = Arc::new(CapturingSink::default());
        let sink = ScrubbedFlowRunSink::new(inner.clone(), store_with_secret(org).await);

        let mut passing = run(org);
        passing.evidence = None;
        passing.status = CheckStatus::Up;
        passing.error = None;
        sink.write_runs(&[passing]).await;

        assert_eq!(inner.seen.lock().unwrap().len(), 1);
    }
}
