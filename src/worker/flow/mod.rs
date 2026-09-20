//! Browser-driven flow monitor: replays a fixed step sequence (login /
//! transaction) against a real page through a headless CDP engine, verifying a
//! login *session* rather than a single request. Runs on any flow-capable node
//! (the control plane or a flow-capable agent).

pub mod engine;
mod evidence;
mod executor;

use chrono::Utc;
use metrics::{counter, histogram};
use uuid::Uuid;

use crate::domain::agent_wire::{FlowEvidence, StepOutcome, StepTrace};
use crate::domain::{CheckResult, CheckStatus, FlowCheck};
use crate::metric_names;
use engine::CdpEngine;
use executor::RunResult;

/// Everything a run produced beyond the verdict: the page behind a failure, and
/// where the run got to.
#[derive(Debug, Default)]
pub struct FlowProbe {
    pub evidence: Option<FlowEvidence>,
    pub steps: Vec<StepTrace>,
}

pub async fn execute_flow_check(
    target_id: Uuid,
    org_id: Uuid,
    flow: &FlowCheck,
    engine: Option<&CdpEngine>,
) -> CheckResult {
    execute_flow_check_probe(target_id, org_id, flow, engine)
        .await
        .0
}

/// Same run, plus what the page said when a step failed and how far the run
/// got. Backs the test-check UI, where the error string alone rarely places
/// the fault.
pub async fn execute_flow_check_probe(
    target_id: Uuid,
    org_id: Uuid,
    flow: &FlowCheck,
    engine: Option<&CdpEngine>,
) -> (CheckResult, FlowProbe) {
    let Some(engine) = engine else {
        counter!(metric_names::FLOW_RUNS, "outcome" => "unconfigured").increment(1);
        return (
            CheckResult::error(target_id, org_id, "flow engine not configured on this node"),
            FlowProbe::default(),
        );
    };

    let started = Utc::now();
    // `run` applies the flow deadline internally, after it holds a concurrency
    // slot, and returns the elapsed probe time excluding any queue wait.
    let (run, evidence, steps, elapsed) = engine.run(flow).await;

    let total = flow.steps.len();
    let (outcome, status, error) = match run {
        RunResult::Passed => ("passed", CheckStatus::Up, None),
        RunResult::Failed { step, op, reason } => (
            "failed",
            CheckStatus::Down,
            Some(step_line(step, total, op, &reason)),
        ),
        // Shaped like a step failure because that is how it reads to an
        // operator, but reported Error: the target never got to answer. With
        // nothing reached there is no step to name — the run spent its budget
        // getting the browser and the first page up.
        RunResult::Budget { step, op } => (
            "budget",
            CheckStatus::Error,
            Some(if reached(&steps) == 0 {
                "run budget spent before the first step ran".to_string()
            } else {
                step_line(step, total, op, "run budget spent")
            }),
        ),
        RunResult::Engine(e) => ("engine", CheckStatus::Error, Some(e)),
    };

    record_run(target_id, outcome, elapsed, &steps, error.as_deref());

    let result = CheckResult {
        target_id,
        org_id,
        timestamp: started,
        status,
        duration_ms: elapsed,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: None,
        response_size: None,
        diagnostic: None,
        error,
    };
    (result, FlowProbe { evidence, steps })
}

fn step_line(step: usize, total: usize, op: &str, reason: &str) -> String {
    format!("step {}/{total} {op}: {reason}", step + 1)
}

fn reached(steps: &[StepTrace]) -> usize {
    steps
        .iter()
        .filter(|s| s.outcome != StepOutcome::Skipped)
        .count()
}

/// A skipped step is time nobody spent, so it stays out of the op histogram;
/// including it would drag every op's distribution toward zero.
fn record_run(
    target_id: Uuid,
    outcome: &'static str,
    elapsed_ms: u32,
    steps: &[StepTrace],
    error: Option<&str>,
) {
    counter!(metric_names::FLOW_RUNS, "outcome" => outcome).increment(1);
    for step in steps {
        if step.outcome != StepOutcome::Skipped {
            histogram!(metric_names::FLOW_STEP_DURATION_MS, "op" => step.op.clone())
                .record(f64::from(step.duration_ms));
        }
    }
    let reached = reached(steps);
    match outcome {
        "passed" => tracing::debug!(
            %target_id,
            steps = steps.len(),
            elapsed_ms,
            "flow run passed"
        ),
        _ => tracing::warn!(
            %target_id,
            outcome,
            reached,
            steps = steps.len(),
            elapsed_ms,
            error = error.unwrap_or_default(),
            "flow run did not pass"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::domain::FlowStep;
    use engine::{CdpEngine, FlowEngineConfig};

    #[tokio::test]
    async fn missing_engine_reports_error_not_panic() {
        let flow = FlowCheck {
            start_url: url::Url::parse("https://example.com/login").unwrap(),
            steps: vec![FlowStep::AssertUrl {
                contains: "/x".into(),
            }],
            timeout: Duration::from_secs(5),
            step_timeout: Duration::from_secs(1),
            verify_tls: true,
        };
        let r = execute_flow_check(Uuid::nil(), Uuid::nil(), &flow, None).await;
        assert_eq!(r.status, CheckStatus::Error);
        assert!(r.error.unwrap().contains("not configured"));
    }

    // End-to-end through the real engine + a spawned Lightpanda. Ignored: needs
    // the engine binary (path in FLOW_LIGHTPANDA_BIN) and outbound network.
    #[tokio::test]
    #[ignore]
    async fn real_login_flow_passes() {
        let Ok(bin) = std::env::var("FLOW_LIGHTPANDA_BIN") else {
            eprintln!("FLOW_LIGHTPANDA_BIN unset; skipping");
            return;
        };
        let engine = CdpEngine::new(FlowEngineConfig {
            binary: bin.into(),
            max_concurrency: 1,
            mem_limit_bytes: 0,
            block_private_networks: true,
            block_cidrs: "169.254.0.0/16,127.0.0.0/8".into(),
            v8_max_heap_mb: 0,
            max_response_bytes: 0,
            user_agent_suffix: String::new(),
        });
        let flow = FlowCheck {
            start_url: url::Url::parse("https://the-internet.herokuapp.com/login").unwrap(),
            steps: vec![
                FlowStep::Fill {
                    selector: "#username".into(),
                    value: "tomsmith".into(),
                },
                FlowStep::Fill {
                    selector: "#password".into(),
                    value: "SuperSecretPassword!".into(),
                },
                // The icon inside the button, which is what a recorder
                // captures. Only passes if the click retargets to the button.
                FlowStep::Click {
                    selector: "#login > button > i".into(),
                },
                FlowStep::AssertUrl {
                    contains: "/secure".into(),
                },
                FlowStep::AssertText {
                    selector: None,
                    contains: "secure area".into(),
                },
            ],
            timeout: Duration::from_secs(30),
            step_timeout: Duration::from_secs(10),
            verify_tls: true,
        };
        let (r, probe) =
            execute_flow_check_probe(Uuid::nil(), Uuid::nil(), &flow, Some(&engine)).await;
        assert_eq!(r.status, CheckStatus::Up, "error={:?}", r.error);
        assert_eq!(probe.steps.len(), flow.steps.len());
        assert!(
            probe.steps.iter().all(|s| s.outcome == StepOutcome::Passed),
            "a passing run must trace every step: {:?}",
            probe.steps
        );
        assert!(probe.evidence.is_none(), "a pass has nothing to explain");
    }

    // A budget too small to outlast the browser starting up must say so rather
    // than blaming the step that would have run first.
    #[tokio::test]
    #[ignore]
    async fn budget_spent_before_any_step_names_no_step() {
        let Ok(bin) = std::env::var("FLOW_LIGHTPANDA_BIN") else {
            eprintln!("FLOW_LIGHTPANDA_BIN unset; skipping");
            return;
        };
        let (addr, server) = serve_page("<html><body>never reached</body></html>").await;

        let flow = FlowCheck {
            start_url: url::Url::parse(&format!("http://{addr}/")).unwrap(),
            steps: vec![FlowStep::AssertText {
                selector: None,
                contains: "never reached".into(),
            }],
            timeout: Duration::from_millis(1),
            step_timeout: Duration::from_secs(5),
            verify_tls: false,
        };
        let (result, probe) = execute_flow_check_probe(
            Uuid::nil(),
            Uuid::nil(),
            &flow,
            Some(&sandbox_engine(&bin, false)),
        )
        .await;
        server.abort();

        assert_eq!(
            result.status,
            CheckStatus::Error,
            "error={:?}",
            result.error
        );
        assert_eq!(
            result.error.as_deref(),
            Some("run budget spent before the first step ran")
        );
        assert_eq!(
            probe.steps.iter().map(|s| s.outcome).collect::<Vec<_>>(),
            vec![StepOutcome::Skipped]
        );
    }

    // The budget, not the step timeout, must end a run that outlives it — and
    // the page has to survive that so the failure still explains itself.
    #[tokio::test]
    #[ignore]
    async fn spent_budget_names_the_step_and_keeps_the_page() {
        let Ok(bin) = std::env::var("FLOW_LIGHTPANDA_BIN") else {
            eprintln!("FLOW_LIGHTPANDA_BIN unset; skipping");
            return;
        };
        let (addr, server) =
            serve_page("<html><head><title>Stuck</title></head><body>waiting room</body></html>")
                .await;

        let flow = FlowCheck {
            start_url: url::Url::parse(&format!("http://{addr}/")).unwrap(),
            steps: vec![
                FlowStep::AssertText {
                    selector: None,
                    contains: "waiting room".into(),
                },
                FlowStep::WaitFor {
                    selector: "#never".into(),
                },
                FlowStep::AssertUrl {
                    contains: "/done".into(),
                },
            ],
            timeout: Duration::from_secs(6),
            step_timeout: Duration::from_secs(60),
            verify_tls: false,
        };
        let (result, probe) = execute_flow_check_probe(
            Uuid::nil(),
            Uuid::nil(),
            &flow,
            Some(&sandbox_engine(&bin, false)),
        )
        .await;
        server.abort();

        assert_eq!(
            result.status,
            CheckStatus::Error,
            "error={:?}",
            result.error
        );
        assert_eq!(
            result.error.as_deref(),
            Some("step 2/3 wait_for: run budget spent")
        );
        assert!(
            result.duration_ms < 11_000,
            "the backstop must not be what ended the run: {} ms",
            result.duration_ms
        );
        let outcomes: Vec<_> = probe.steps.iter().map(|s| s.outcome).collect();
        assert_eq!(
            outcomes,
            vec![
                StepOutcome::Passed,
                StepOutcome::Failed,
                StepOutcome::Skipped
            ]
        );
        let ev = probe
            .evidence
            .expect("a spent budget must still snapshot the page");
        assert_eq!(ev.title.as_deref(), Some("Stuck"));
    }

    /// Serves one fixed page on loopback until the returned task is aborted.
    async fn serve_page(body: &'static str) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 2048];
                let _ = sock.readable().await;
                let _ = sock.try_read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        (addr, task)
    }

    fn sandbox_engine(bin: &str, block: bool) -> CdpEngine {
        CdpEngine::new(FlowEngineConfig {
            binary: bin.into(),
            max_concurrency: 1,
            mem_limit_bytes: 0,
            block_private_networks: block,
            block_cidrs: if block {
                "127.0.0.0/8".into()
            } else {
                String::new()
            },
            v8_max_heap_mb: 0,
            max_response_bytes: 0,
            user_agent_suffix: String::new(),
        })
    }

    async fn run_loopback_flow(engine: &CdpEngine, url: &str) -> CheckResult {
        let flow = FlowCheck {
            start_url: url::Url::parse(url).unwrap(),
            steps: vec![FlowStep::AssertText {
                selector: None,
                contains: "flowmarker".into(),
            }],
            timeout: Duration::from_secs(15),
            step_timeout: Duration::from_secs(5),
            verify_tls: false,
        };
        execute_flow_check(Uuid::nil(), Uuid::nil(), &flow, Some(engine)).await
    }

    // A failing step must explain itself without a picture.
    #[tokio::test]
    #[ignore]
    async fn failed_step_collects_page_evidence() {
        use tokio::io::AsyncWriteExt;

        let Ok(bin) = std::env::var("FLOW_LIGHTPANDA_BIN") else {
            eprintln!("FLOW_LIGHTPANDA_BIN unset; skipping");
            return;
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 2048];
                let _ = sock.readable().await;
                let _ = sock.try_read(&mut buf);
                let head = String::from_utf8_lossy(&buf).to_string();
                let (code, body) = if head.starts_with("GET /session") {
                    ("404 Not Found", String::new())
                } else {
                    (
                        "200 OK",
                        "<html><head><title>Sign in</title></head><body>\
                         <p>Your credentials are invalid</p>\
                         <script>console.error('early error');fetch('/session');setTimeout(function(){console.error('token expired');fetch('/session?late=1');},400);</script>\
                         </body></html>"
                            .to_string(),
                    )
                };
                let resp = format!(
                    "HTTP/1.1 {code}\r\nContent-Length: {}\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let flow = FlowCheck {
            start_url: url::Url::parse(&format!("http://{addr}/login")).unwrap(),
            steps: vec![FlowStep::AssertText {
                selector: None,
                contains: "welcome back".into(),
            }],
            timeout: Duration::from_secs(20),
            step_timeout: Duration::from_secs(2),
            verify_tls: false,
        };
        let (result, probe) = execute_flow_check_probe(
            Uuid::nil(),
            Uuid::nil(),
            &flow,
            Some(&sandbox_engine(&bin, false)),
        )
        .await;
        server.abort();

        assert_eq!(result.status, CheckStatus::Down, "error={:?}", result.error);
        assert_eq!(probe.steps.len(), 1);
        assert_eq!(probe.steps[0].op, "assert_text");
        assert_eq!(probe.steps[0].outcome, StepOutcome::Failed);
        let ev = probe.evidence.expect("a failed step must carry evidence");
        assert_eq!(ev.title.as_deref(), Some("Sign in"));
        assert!(
            ev.final_url
                .as_deref()
                .is_some_and(|u| u.contains("/login")),
            "final_url={:?}",
            ev.final_url
        );
        assert!(
            ev.text_snippet
                .as_deref()
                .is_some_and(|t| t.contains("credentials are invalid")),
            "text_snippet={:?}",
            ev.text_snippet
        );
        assert_eq!(
            ev.console,
            vec![crate::domain::agent_wire::ConsoleLine {
                level: "error".into(),
                text: "token expired".into(),
            }],
            "one console.error must arrive exactly once"
        );
    }

    // Causal proof the egress sandbox works: the same loopback page the browser
    // reaches with blocking off is unreachable with it on. A private target that
    // merely refused the connection would fail either way; contrasting the two
    // runs pins the failure on the block, not the network.
    #[tokio::test]
    #[ignore]
    async fn egress_sandbox_blocks_loopback_but_not_public() {
        let Ok(bin) = std::env::var("FLOW_LIGHTPANDA_BIN") else {
            eprintln!("FLOW_LIGHTPANDA_BIN unset; skipping");
            return;
        };
        let (addr, server) = serve_page("<html><body>flowmarker</body></html>").await;
        let url = format!("http://{addr}/");

        let reached = run_loopback_flow(&sandbox_engine(&bin, false), &url).await;
        assert_eq!(
            reached.status,
            CheckStatus::Up,
            "loopback reachable without the block; error={:?}",
            reached.error
        );

        let blocked = run_loopback_flow(&sandbox_engine(&bin, true), &url).await;
        assert_ne!(
            blocked.status,
            CheckStatus::Up,
            "loopback must be unreachable with the egress sandbox on"
        );

        server.abort();
    }
}
