//! CDP engine for flow monitors. Lightpanda exposes one global browser context
//! and one target per connection, so isolation is per-process: each run spawns a
//! fresh Lightpanda `serve`, drives one flow over CDP, then tears the process
//! down. A fresh process is a clean cookie jar, which is the isolation a shared
//! browser context would otherwise provide.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide::Browser;
use chromiumoxide::cdp::browser_protocol::network::EnableParams as NetworkEnable;
use chromiumoxide::cdp::js_protocol::runtime::EnableParams as RuntimeEnable;
use futures::StreamExt;
use tokio::process::{Child, Command};
use tokio::sync::Semaphore;

use super::evidence::EvidenceCollector;
use super::executor::{RunResult, run_steps};
use crate::domain::FlowCheck;
use crate::domain::agent_wire::{FlowEvidence, StepTrace};

/// Headroom over the flow's own budget for the outer backstop. The deadline
/// inside a run ends it with a trace and a live page to snapshot, so the
/// backstop should only ever fire for a CDP call that stopped returning to it.
pub const BACKSTOP_GRACE: Duration = Duration::from_secs(5);
/// Longest an interactive run waits for a browser slot before it reports the
/// engine busy, so a test never burns its wait queued behind scheduled flows.
pub const INTERACTIVE_QUEUE_LIMIT: Duration = Duration::from_secs(10);
const BUSY: &str = "every browser slot stayed busy with other flows";

/// A run the engine turned away for want of a free slot: the target was never
/// contacted.
pub fn is_busy(result: &crate::domain::CheckResult) -> bool {
    result.error.as_deref().is_some_and(|e| e.starts_with(BUSY))
}

/// Evidence is telemetry about a failure, so reading it must never cost the
/// verdict and trace already in hand. Sits inside the backstop grace, which it
/// spends after the run's own budget is gone.
const EVIDENCE_BUDGET: Duration = Duration::from_secs(3);

/// What one attempt produced: a verdict, the page state behind a failure, and
/// one entry per declared step.
type Attempt = (RunResult, Option<FlowEvidence>, Vec<StepTrace>);

pub struct FlowEngineConfig {
    pub binary: PathBuf,
    pub max_concurrency: usize,
    /// Per-run browser RSS ceiling in bytes; 0 disables the watchdog.
    pub mem_limit_bytes: u64,
    /// Block the browser's outbound requests to private/internal IPs after DNS
    /// resolution (runtime SSRF guard).
    pub block_private_networks: bool,
    /// Extra CIDRs to block, comma-separated (Lightpanda `--block-cidrs` form);
    /// empty to add none.
    pub block_cidrs: String,
    /// V8 heap cap per browser (MB); 0 = engine default.
    pub v8_max_heap_mb: u64,
    /// Per-response size cap in bytes; 0 = no limit.
    pub max_response_bytes: u64,
    /// User-Agent suffix for attribution; empty = none.
    pub user_agent_suffix: String,
}

/// Spawns and drives Lightpanda processes. Concurrency is bounded so a burst of
/// flow checks can't fork an unbounded number of browser processes.
pub struct CdpEngine {
    binary: PathBuf,
    sem: Arc<Semaphore>,
    mem_limit_bytes: u64,
    block_private_networks: bool,
    block_cidrs: String,
    v8_max_heap_mb: u64,
    max_response_bytes: u64,
    user_agent_suffix: String,
}

impl CdpEngine {
    pub fn new(cfg: FlowEngineConfig) -> Self {
        Self {
            binary: cfg.binary,
            sem: Arc::new(Semaphore::new(cfg.max_concurrency.max(1))),
            mem_limit_bytes: cfg.mem_limit_bytes,
            block_private_networks: cfg.block_private_networks,
            block_cidrs: cfg.block_cidrs,
            v8_max_heap_mb: cfg.v8_max_heap_mb,
            max_response_bytes: cfg.max_response_bytes,
            user_agent_suffix: cfg.user_agent_suffix,
        }
    }

    /// Elapsed excludes queue wait. Steps must already be variable-resolved.
    /// With a `queue_limit`, a run that finds no free slot in time reports the
    /// engine busy instead of waiting on.
    pub async fn run(
        &self,
        flow: &FlowCheck,
        queue_limit: Option<Duration>,
    ) -> (RunResult, Option<FlowEvidence>, Vec<StepTrace>, u32) {
        let acquired = match queue_limit {
            Some(limit) => match tokio::time::timeout(limit, self.sem.acquire()).await {
                Ok(acquired) => acquired,
                Err(_) => {
                    return (
                        RunResult::Busy(format!(
                            "{BUSY} for {} s; try again shortly",
                            limit.as_secs()
                        )),
                        None,
                        Vec::new(),
                        0,
                    );
                }
            },
            None => self.sem.acquire().await,
        };
        let _permit = match acquired {
            Ok(p) => p,
            Err(_) => {
                return (
                    RunResult::Engine("flow engine shut down".into()),
                    None,
                    Vec::new(),
                    0,
                );
            }
        };
        // Clock and deadline both start only once a slot is held, so time spent
        // queued for a permit is never charged against the flow's own budget.
        let started = Instant::now();
        let deadline = started + flow.timeout;
        let (result, evidence, steps) = match tokio::time::timeout(
            flow.timeout + BACKSTOP_GRACE,
            self.run_once(flow, deadline),
        )
        .await
        {
            Ok(attempt) => attempt,
            Err(_) => (
                RunResult::Engine(format!(
                    "engine stopped responding after its {} ms budget",
                    flow.timeout.as_millis()
                )),
                None,
                Vec::new(),
            ),
        };
        let elapsed = started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
        (result, evidence, steps, elapsed)
    }

    /// One flow attempt, retried on a fresh port when the engine dies during
    /// startup — a freed ephemeral port can be taken before Lightpanda binds it.
    /// Retries share the run's deadline, so a slow start is charged to the run.
    async fn run_once(&self, flow: &FlowCheck, deadline: Instant) -> Attempt {
        let mut last = String::new();
        for _ in 0..3 {
            let port = match free_port() {
                Ok(p) => p,
                Err(e) => return engine_error(format!("no free port: {e}")),
            };
            let mut child = match self.spawn(port, flow.verify_tls) {
                Ok(c) => c,
                Err(e) => return engine_error(format!("spawn lightpanda: {e}")),
            };
            // A heavy page can't OOM the node: if RSS crosses the ceiling the
            // watchdog wins the race and the process is torn down.
            let attempt = match (child.id(), self.mem_limit_bytes) {
                (Some(pid), limit) if limit > 0 => tokio::select! {
                    r = drive(&mut child, port, flow, deadline) => r,
                    _ = watch_rss(pid, limit) => engine_error(format!(
                        "engine exceeded {} MB memory limit",
                        limit / (1024 * 1024)
                    )),
                },
                _ => drive(&mut child, port, flow, deadline).await,
            };
            let _ = child.start_kill();
            let _ = child.wait().await;
            if let RunResult::Engine(e) = &attempt.0
                && let Some(startup) = e.strip_prefix(STARTUP_FAILED)
            {
                last = startup.to_string();
                continue;
            }
            return attempt;
        }
        engine_error(format!("engine did not start after retries: {last}"))
    }

    fn spawn(&self, port: u16, verify_tls: bool) -> std::io::Result<Child> {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("serve")
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string());
        if !verify_tls {
            cmd.arg("--insecure-disable-tls-host-verification");
        }
        // Runtime egress sandbox: the engine re-checks every request's resolved
        // IP, so a redirect or JS fetch to an internal address is blocked even
        // though the save-time URL check only saw the public start URL.
        if self.block_private_networks {
            cmd.arg("--block-private-networks");
        }
        let cidrs = self.block_cidrs.trim();
        if !cidrs.is_empty() {
            cmd.arg("--block-cidrs").arg(cidrs);
        }
        // Optional resource + attribution belts (default off).
        if self.v8_max_heap_mb > 0 {
            cmd.arg("--v8-max-heap-mb")
                .arg(self.v8_max_heap_mb.to_string());
        }
        if self.max_response_bytes > 0 {
            cmd.arg("--http-max-response-size")
                .arg(self.max_response_bytes.to_string());
        }
        let ua = self.user_agent_suffix.trim();
        if !ua.is_empty() {
            cmd.arg("--user-agent-suffix").arg(ua);
        }
        cmd.stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
    }
}

/// Marks an error as a startup miss (the engine never came up), so the caller
/// retries on a fresh port instead of reporting a false verdict on the target.
const STARTUP_FAILED: &str = "engine did not start: ";

fn engine_error(reason: String) -> Attempt {
    (RunResult::Engine(reason), None, Vec::new())
}

async fn drive(child: &mut Child, port: u16, flow: &FlowCheck, deadline: Instant) -> Attempt {
    let ws = format!("ws://127.0.0.1:{port}/");
    let (browser, mut handler) = match connect_with_retry(child, &ws).await {
        Ok(pair) => pair,
        Err(e) => return engine_error(format!("{STARTUP_FAILED}{e}")),
    };
    let pump = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let outcome = async {
        // Listeners would catch the first byte if we opened `about:blank` and
        // navigated after, but this engine never resolves that call.
        let page = browser
            .new_page(flow.start_url.as_str())
            .await
            .map_err(|e| format!("open page: {e}"))?;
        let _ = page.execute(RuntimeEnable::default()).await;
        let _ = page.execute(NetworkEnable::default()).await;
        let collector = EvidenceCollector::attach(&page).await;
        // The first load is inside the run's budget like everything else. On
        // expiry the step loop below is what reports it, so a slow origin ends
        // the run on its deadline rather than on the backstop.
        if let Ok(Err(e)) =
            tokio::time::timeout_at(deadline.into(), page.wait_for_navigation()).await
        {
            return Err(format!("initial navigation: {e}"));
        }

        let (result, steps) = run_steps(&page, &flow.steps, flow.step_timeout, deadline).await;
        // A pass has nothing to explain; after an engine break the page state
        // is not trustworthy. A spent budget leaves both a live page and the
        // question of what it was still waiting for.
        let evidence = match &result {
            RunResult::Failed { .. } | RunResult::Budget { .. } => {
                tokio::time::timeout(EVIDENCE_BUDGET, collector.finish(&page))
                    .await
                    .ok()
            }
            _ => None,
        };
        Ok::<Attempt, String>((result, evidence, steps))
    }
    .await;

    pump.abort();
    match outcome {
        Ok(attempt) => attempt,
        Err(e) => engine_error(e),
    }
}

/// Poll the engine's RSS via `ps` (portable, cheap at this cadence); returns once
/// it exceeds `limit`. A vanished process reads as 0 and keeps the loop harmless.
async fn watch_rss(pid: u32, limit_bytes: u64) {
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if rss_bytes(pid).await > limit_bytes {
            return;
        }
    }
}

async fn rss_bytes(pid: u32) -> u64 {
    #[cfg(target_os = "linux")]
    {
        // Resident pages from /proc (field 2 of statm), ×4 KiB. No fork, so the
        // watchdog can't be silently defeated by `ps` failing under the memory
        // pressure it exists to catch. A vanished pid reads as 0 (harmless).
        match tokio::fs::read_to_string(format!("/proc/{pid}/statm")).await {
            Ok(s) => s
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
                .saturating_mul(4096),
            Err(_) => 0,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        match Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .await
        {
            Ok(out) => String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse::<u64>()
                .unwrap_or(0)
                .saturating_mul(1024),
            Err(_) => 0,
        }
    }
}

/// Ask the OS for a free loopback port, then release it for Lightpanda to bind.
fn free_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// Lightpanda opens its CDP socket a moment after spawn; retry the connect for
/// up to ~5s before giving up.
async fn connect_with_retry(
    child: &mut Child,
    ws: &str,
) -> Result<(Browser, chromiumoxide::Handler), String> {
    let mut last = String::new();
    for _ in 0..50 {
        // Stop early if the engine already exited (e.g. lost the port race), so
        // the caller retries on a fresh port instead of waiting out the loop.
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("lightpanda exited on startup ({status})"));
        }
        match Browser::connect(ws).await {
            Ok(pair) => return Ok(pair),
            Err(e) => {
                last = e.to_string();
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    Err(format!("CDP connect failed after retries: {last}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn an_interactive_run_reports_busy_when_no_slot_frees() {
        let engine = CdpEngine::new(FlowEngineConfig {
            binary: "lightpanda".into(),
            max_concurrency: 1,
            mem_limit_bytes: 0,
            block_private_networks: true,
            block_cidrs: String::new(),
            v8_max_heap_mb: 0,
            max_response_bytes: 0,
            user_agent_suffix: String::new(),
        });
        let _scheduled = engine.sem.acquire().await.unwrap();
        let flow = FlowCheck {
            start_url: url::Url::parse("https://example.com/").unwrap(),
            steps: Vec::new(),
            timeout: Duration::from_secs(30),
            step_timeout: Duration::from_secs(5),
            verify_tls: true,
        };
        let (run, _, _, elapsed) = engine.run(&flow, Some(INTERACTIVE_QUEUE_LIMIT)).await;
        assert!(matches!(run, RunResult::Busy(_)));
        assert_eq!(elapsed, 0);
    }
}
