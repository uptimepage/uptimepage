//! Runs a flow's step list against a live page. Every action is driven through
//! `Runtime.evaluate` JS rather than CDP input primitives: on the Lightpanda
//! engine, CDP key-typing does not populate `input.value` and a CDP click on a
//! submit button does not submit — but `el.value = ...` + dispatched events and
//! `el.click()` in page JS do both reliably.

use std::time::{Duration, Instant};

use chromiumoxide::Page;

use crate::domain::FlowStep;
use crate::domain::agent_wire::{StepOutcome, StepTrace};

/// Terminal result of running the step list.
pub enum RunResult {
    Passed,
    /// A step legitimately failed (selector missing, assertion false, wait
    /// timed out): the monitored flow is down. `step` is 0-based.
    Failed {
        step: usize,
        op: &'static str,
        reason: String,
    },
    /// The whole-run budget ran out at `step`, which either never started or
    /// was still waiting when the deadline arrived. Nothing was learned about
    /// the target, but the page is still alive to be snapshotted.
    Budget {
        step: usize,
        op: &'static str,
    },
    /// The engine/CDP transport broke: not a verdict on the target, an error.
    Engine(String),
    /// No browser slot freed within the run's queue limit; the target was
    /// never contacted.
    Busy(String),
}

/// A step that did not pass: a flow failure, or a transport break.
enum StepError {
    Failed(String),
    Engine(String),
}

pub async fn run_steps(
    page: &Page,
    steps: &[FlowStep],
    step_timeout: Duration,
    deadline: Instant,
) -> (RunResult, Vec<StepTrace>) {
    let mut trace: Vec<StepTrace> = steps
        .iter()
        .map(|s| StepTrace {
            op: step_op(s).to_string(),
            outcome: StepOutcome::Skipped,
            duration_ms: 0,
        })
        .collect();

    for (i, step) in steps.iter().enumerate() {
        let op = step_op(step);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return (RunResult::Budget { step: i, op }, trace);
        }
        // Clamping keeps a step's own wait from outliving the run it is inside,
        // so the budget is what ends the run rather than a bogus step failure.
        let clamped = remaining <= step_timeout;
        let started = Instant::now();
        let outcome = drive(page, step, step_timeout.min(remaining)).await;
        let took = millis(started.elapsed());
        match outcome {
            Ok(()) => {
                trace[i].outcome = StepOutcome::Passed;
                trace[i].duration_ms = took;
            }
            // A broken transport is no verdict on this step, so the trace keeps
            // none: the page never answered either way.
            Err(StepError::Engine(e)) => return (RunResult::Engine(e), trace),
            Err(StepError::Failed(reason)) => {
                trace[i].outcome = StepOutcome::Failed;
                trace[i].duration_ms = took;
                // Only a step that was still waiting can have been cut short by
                // the clamp. The rest answer in one round trip, so their failure
                // is about the page however little budget was left.
                let result = if waits(step) && clamped && Instant::now() >= deadline {
                    RunResult::Budget { step: i, op }
                } else {
                    RunResult::Failed {
                        step: i,
                        op,
                        reason,
                    }
                };
                return (result, trace);
            }
        }
    }
    (RunResult::Passed, trace)
}

fn millis(d: Duration) -> u32 {
    d.as_millis().min(u128::from(u32::MAX)) as u32
}

fn waits(step: &FlowStep) -> bool {
    !matches!(step, FlowStep::Fill { .. } | FlowStep::Click { .. })
}

fn step_op(step: &FlowStep) -> &'static str {
    match step {
        FlowStep::Goto { .. } => "goto",
        FlowStep::Click { .. } => "click",
        FlowStep::Fill { .. } => "fill",
        FlowStep::WaitFor { .. } => "wait_for",
        FlowStep::AssertText { .. } => "assert_text",
        FlowStep::AssertUrl { .. } => "assert_url",
    }
}

async fn drive(page: &Page, step: &FlowStep, step_timeout: Duration) -> Result<(), StepError> {
    match step {
        FlowStep::Goto { url } => {
            let nav = async {
                page.goto(url.as_str())
                    .await
                    .map_err(|e| StepError::Engine(format!("goto: {e}")))?;
                page.wait_for_navigation()
                    .await
                    .map_err(|e| StepError::Engine(format!("goto nav: {e}")))?;
                Ok(())
            };
            // A slow origin would otherwise run past the whole-run deadline and
            // cost the trace the backstop cannot preserve.
            match tokio::time::timeout(step_timeout, nav).await {
                Ok(r) => r,
                Err(_) => Err(StepError::Failed(format!("timed out loading {url}"))),
            }
        }
        FlowStep::Fill { selector, value } => {
            match eval_action(page, &js_fill(selector, value)).await? {
                s if s == "OK" => Ok(()),
                _ => Err(StepError::Failed(format!("selector not found: {selector}"))),
            }
        }
        FlowStep::Click { selector } => match eval_action(page, &js_click(selector)).await? {
            s if s == "OK" => Ok(()),
            _ => Err(StepError::Failed(format!("selector not found: {selector}"))),
        },
        FlowStep::WaitFor { selector } => {
            let js = js_present(selector);
            if poll(step_timeout, || eval_bool(page, &js)).await? {
                Ok(())
            } else {
                Err(StepError::Failed(format!(
                    "timed out waiting for {selector}"
                )))
            }
        }
        FlowStep::AssertText { selector, contains } => {
            let js = js_text(selector.as_deref());
            let want = contains.clone();
            let found = poll(step_timeout, || async {
                Ok(eval_string(page, &js).await?.contains(&want))
            })
            .await?;
            if found {
                Ok(())
            } else {
                Err(StepError::Failed(format!("text not found: {contains:?}")))
            }
        }
        FlowStep::AssertUrl { contains } => {
            let want = contains.clone();
            let found = poll(step_timeout, || async {
                let url = page
                    .url()
                    .await
                    .map_err(|e| StepError::Engine(format!("url: {e}")))?
                    .unwrap_or_default();
                Ok(url.contains(&want))
            })
            .await?;
            if found {
                Ok(())
            } else {
                Err(StepError::Failed(format!(
                    "url does not contain {contains:?}"
                )))
            }
        }
    }
}

const POLL_PAUSE: Duration = Duration::from_millis(100);

/// Poll `check` every 100ms until it is true or `deadline` elapses. A context
/// loss is not an answer: it neither ends the poll nor counts as `false`. If the
/// window closes while the page is still losing its context, that is what gets
/// reported — a probe that lost its footing is an error, not a down verdict.
async fn poll<F, Fut>(deadline: Duration, mut check: F) -> Result<bool, StepError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool, StepError>>,
{
    let start = Instant::now();
    // Held only until the page answers again, so a recovered navigation leaves
    // nothing behind and a page still churning at the deadline surfaces it.
    let mut lost: Option<String>;
    loop {
        lost = match check().await {
            Ok(true) => return Ok(true),
            Ok(false) => None,
            Err(StepError::Engine(e)) if is_context_loss(&e) => Some(e),
            Err(e) => return Err(e),
        };
        if start.elapsed() >= deadline {
            return match lost {
                Some(e) => Err(StepError::Engine(e)),
                None => Ok(false),
            };
        }
        tokio::time::sleep(POLL_PAUSE).await;
    }
}

/// CDP failures that mean the context the JS was about to run in went away,
/// which a navigation does routinely and which says nothing about the target.
/// Deliberately excludes "Inspected target navigated or closed": that one also
/// fires for a closed target, and retrying a dead page just burns the budget.
fn is_context_loss(msg: &str) -> bool {
    const MARKERS: [&str; 2] = [
        "Cannot find context with specified id",
        "Execution context was destroyed",
    ];
    MARKERS.iter().any(|m| msg.contains(m))
}

/// One retry, because a read firing right after a navigation can reach the page
/// mid-swap. Only ever wraps reads: a mutating step is not safe to repeat, and
/// re-running its JS against the document the mutation just produced would
/// report the selector as missing when the step in fact worked.
async fn retrying_context_loss<T, F, Fut>(mut run: F) -> Result<T, StepError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, StepError>>,
{
    match run().await {
        Err(StepError::Engine(e)) if is_context_loss(&e) => {
            tokio::time::sleep(POLL_PAUSE).await;
            run().await
        }
        other => other,
    }
}

/// Runs a mutating step's JS exactly once. A lost context stays an engine
/// error: the click may well have landed and taken the page with it.
async fn eval_action(page: &Page, js: &str) -> Result<String, StepError> {
    eval_string_once(page, js).await
}

async fn eval_string(page: &Page, js: &str) -> Result<String, StepError> {
    retrying_context_loss(|| eval_string_once(page, js)).await
}

async fn eval_bool(page: &Page, js: &str) -> Result<bool, StepError> {
    retrying_context_loss(|| eval_bool_once(page, js)).await
}

async fn eval_string_once(page: &Page, js: &str) -> Result<String, StepError> {
    let v = page
        .evaluate(js)
        .await
        .map_err(|e| StepError::Engine(format!("evaluate: {e}")))?;
    Ok(v.into_value::<Option<String>>()
        .map_err(|e| StepError::Engine(format!("evaluate decode: {e}")))?
        .unwrap_or_default())
}

async fn eval_bool_once(page: &Page, js: &str) -> Result<bool, StepError> {
    let v = page
        .evaluate(js)
        .await
        .map_err(|e| StepError::Engine(format!("evaluate: {e}")))?;
    Ok(v.into_value::<Option<bool>>()
        .map_err(|e| StepError::Engine(format!("evaluate decode: {e}")))?
        .unwrap_or(false))
}

// JS builders. Selector and value are JSON-encoded into the source so a hostile
// selector or credential can never break out of its string literal.
fn js_fill(selector: &str, value: &str) -> String {
    format!(
        "(function(){{const e=document.querySelector({s});if(!e)return 'NOT_FOUND';\
         e.value={v};e.dispatchEvent(new Event('input',{{bubbles:true}}));\
         e.dispatchEvent(new Event('change',{{bubbles:true}}));return 'OK';}})()",
        s = enc(selector),
        v = enc(value),
    )
}

/// Elements the browser gives an activation behaviour to. `click()` runs that
/// behaviour only for the element it is called on, so clicking the `<i>` inside
/// a submit button fires an event that bubbles but submits nothing. A recorder
/// captures the deepest element under the cursor, which is usually that icon,
/// so retarget to the ancestor a real mouse click would have activated. The
/// ancestor always wins when one matches, so a handler bound to an inner
/// element nested inside a link or button does not get the click.
const CLICK_TARGETS: &str = "button, a, input[type=\"submit\"], input[type=\"button\"], input[type=\"reset\"], summary, [role=\"button\"]";

fn js_click(selector: &str) -> String {
    format!(
        "(function(){{const e=document.querySelector({s});if(!e)return 'NOT_FOUND';\
         (e.closest({t})||e).click();return 'OK';}})()",
        s = enc(selector),
        t = enc(CLICK_TARGETS),
    )
}

fn js_present(selector: &str) -> String {
    format!("!!document.querySelector({s})", s = enc(selector))
}

fn js_text(selector: Option<&str>) -> String {
    match selector {
        Some(s) => format!(
            "(function(){{const e=document.querySelector({s});return e?e.textContent:null;}})()",
            s = enc(s)
        ),
        None => "document.body?document.body.textContent:null".to_string(),
    }
}

fn enc(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_lost() -> StepError {
        StepError::Engine("evaluate: Error -32000: Cannot find context with specified id".into())
    }

    #[tokio::test]
    async fn poll_rides_out_a_context_loss_and_still_passes() {
        let mut calls = 0;
        let got = poll(Duration::from_secs(5), || {
            calls += 1;
            async move {
                match calls {
                    1 | 2 => Err(ctx_lost()),
                    _ => Ok(true),
                }
            }
        })
        .await;
        assert!(
            matches!(got, Ok(true)),
            "a navigation mid-poll is not a verdict"
        );
    }

    #[tokio::test]
    async fn poll_reports_not_present_when_the_page_kept_answering() {
        let got = poll(Duration::from_millis(150), || async { Ok(false) }).await;
        assert!(
            matches!(got, Ok(false)),
            "a page that answers cleanly gets a verdict"
        );
    }

    #[tokio::test]
    async fn poll_surfaces_a_context_loss_the_page_never_recovered_from() {
        let got = poll(Duration::from_millis(150), || async { Err(ctx_lost()) }).await;
        assert!(
            matches!(got, Err(StepError::Engine(_))),
            "never answering is an engine error, not a down verdict"
        );
    }

    #[tokio::test]
    async fn poll_does_not_call_a_dying_page_absent() {
        // Answered once, then the context goes and never comes back. Reporting
        // Down here would alert on the probe's own footing, not the target.
        let mut calls = 0;
        let got = poll(Duration::from_millis(150), || {
            calls += 1;
            async move {
                if calls == 1 {
                    Ok(false)
                } else {
                    Err(ctx_lost())
                }
            }
        })
        .await;
        assert!(
            matches!(got, Err(StepError::Engine(_))),
            "a loss still standing at the deadline outranks an earlier absence"
        );
    }

    #[tokio::test]
    async fn poll_forgets_a_loss_the_page_recovered_from() {
        let mut calls = 0;
        let got = poll(Duration::from_millis(250), || {
            calls += 1;
            async move {
                if calls == 1 {
                    Err(ctx_lost())
                } else {
                    Ok(false)
                }
            }
        })
        .await;
        assert!(
            matches!(got, Ok(false)),
            "a recovered navigation must not colour the verdict"
        );
    }

    #[tokio::test]
    async fn a_closed_target_is_not_treated_as_transient() {
        // Chromium uses this wording for a closed target too, so it must fail
        // fast rather than spin out the whole step budget.
        let got = poll(Duration::from_secs(30), || async {
            Err(StepError::Engine(
                "evaluate: Inspected target navigated or closed".into(),
            ))
        })
        .await;
        assert!(matches!(got, Err(StepError::Engine(_))));
    }

    #[tokio::test]
    async fn poll_still_fails_fast_on_an_unrelated_engine_error() {
        let got = poll(Duration::from_secs(5), || async {
            Err(StepError::Engine("url: transport closed".into()))
        })
        .await;
        assert!(matches!(got, Err(StepError::Engine(_))));
    }

    #[tokio::test]
    async fn a_read_retries_one_context_loss() {
        let mut calls = 0;
        let got: Result<u8, StepError> = retrying_context_loss(|| {
            calls += 1;
            async move { if calls == 1 { Err(ctx_lost()) } else { Ok(7) } }
        })
        .await;
        assert!(matches!(got, Ok(7)));
    }

    #[test]
    fn mutating_steps_are_driven_by_the_no_retry_eval() {
        // Guards the fix, not the wiring: repeating a click after the context it
        // navigated away from reads as a missing selector, which the engine maps
        // to Down. Only reads may be retried. Whitespace is collapsed so
        // rustfmt cannot break the check by wrapping a call site, and the
        // needles are built at run time so this test cannot satisfy itself.
        let src: String = include_str!("executor.rs")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        for builder in ["js_fill(", "js_click("] {
            let wanted = format!("{}(page,&{builder}", "eval_action");
            let forbidden = format!("{}(page,&{builder}", "eval_string");
            assert!(src.contains(&wanted), "{builder} must use eval_action");
            assert!(
                !src.contains(&forbidden),
                "{builder} must not use the retrying eval"
            );
        }
    }

    #[test]
    fn context_loss_is_matched_narrowly() {
        assert!(is_context_loss(
            "evaluate: Error -32000: Cannot find context with specified id"
        ));
        assert!(is_context_loss("Execution context was destroyed"));
        assert!(!is_context_loss(
            "evaluate: Error -32000: Object reference chain is too long"
        ));
        assert!(!is_context_loss("goto nav: timeout"));
    }

    #[test]
    fn fill_encodes_selector_and_value_safely() {
        // A value containing quotes/backslashes must stay inside its literal.
        let js = js_fill("#pw", "a\"b\\c</script>");
        assert!(js.contains(r##""#pw""##));
        assert!(js.contains(r#""a\"b\\c</script>""#));
        assert!(js.contains("dispatchEvent(new Event('input'"));
    }

    #[test]
    fn click_and_present_and_text_target_the_selector() {
        assert!(js_click("#go").contains(r##"querySelector("#go")"##));
        assert!(js_present("#x").starts_with("!!document.querySelector("));
        assert!(js_text(Some(".flash")).contains(r#"querySelector(".flash")"#));
        assert_eq!(
            js_text(None),
            "document.body?document.body.textContent:null"
        );
    }

    #[test]
    fn click_retargets_to_the_activatable_ancestor() {
        let js = js_click("#login > button > i");
        assert!(js.contains("closest("), "click must retarget: {js}");
        assert!(js.contains("button, a, input[type=\\\"submit\\\"]"));
        // Falls back to the element itself, so a plain button still clicks
        // itself and a handler-only div is unaffected.
        assert!(
            js.contains("||e).click()"),
            "must fall back to the element: {js}"
        );
    }

    #[test]
    fn selector_quote_is_escaped_exactly() {
        // Exact output proves a quote in the selector is escaped, so it stays
        // one string literal and cannot break out into executable code.
        assert_eq!(js_present("#a"), "!!document.querySelector(\"#a\")");
        assert_eq!(js_present("a\"b"), "!!document.querySelector(\"a\\\"b\")");
    }
}
