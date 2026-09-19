//! Collects what a failing flow left behind. The engine has no renderer, so
//! the console and the live DOM stand in for a screenshot.

use std::sync::{Arc, Mutex};

use chromiumoxide::Page;
use chromiumoxide::cdp::js_protocol::runtime::EventConsoleApiCalled;
use futures::StreamExt;

use crate::domain::agent_wire::{ConsoleLine, FlowEvidence};
use crate::security::redaction::scrub_url;

/// Caps, so a page looping console errors can't return an unbounded payload.
const MAX_CONSOLE: usize = 20;
const MAX_LINE_CHARS: usize = 300;
const MAX_URL_CHARS: usize = 300;
const MAX_SNIPPET_CHARS: usize = 800;
/// Cut in the page, not here: a text-heavy document would otherwise cross CDP
/// whole to yield a few hundred characters.
const WIRE_SNIPPET_CHARS: usize = MAX_SNIPPET_CHARS * 2;

#[derive(Default)]
struct Buffer {
    console: Vec<ConsoleLine>,
}

/// Drains CDP events into a bounded buffer for the life of one flow run.
pub struct EvidenceCollector {
    buffer: Arc<Mutex<Buffer>>,
    pumps: Vec<tokio::task::JoinHandle<()>>,
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Reads `expr` with the page collapsing then cutting it, so the cut is spent on
/// text rather than on the blank runs a stalled page is mostly made of. A
/// non-string stays null rather than becoming the word "undefined".
fn js_slice(expr: &str, max: usize) -> String {
    format!(
        "(function(){{const v={expr};\
         return typeof v==='string'?v.replace(/\\s+/g,' ').slice(0,{max}):null;}})()"
    )
}

/// Render an argument the way the console would print it, cut before the join
/// so one logged blob cannot build a large intermediate.
fn render_arg(arg: &chromiumoxide::cdp::js_protocol::runtime::RemoteObject) -> String {
    match arg.value.as_ref() {
        Some(serde_json::Value::String(s)) => truncate(s, MAX_LINE_CHARS),
        Some(v) => truncate(&v.to_string(), MAX_LINE_CHARS),
        None => truncate(&arg.description.clone().unwrap_or_default(), MAX_LINE_CHARS),
    }
}

impl EvidenceCollector {
    /// Console events are session-scoped, so they arrive on the page rather
    /// than the browser.
    pub async fn attach(page: &Page) -> Self {
        let buffer = Arc::new(Mutex::new(Buffer::default()));
        let mut pumps = Vec::new();

        if let Ok(mut stream) = page.event_listener::<EventConsoleApiCalled>().await {
            let buf = buffer.clone();
            pumps.push(tokio::spawn(async move {
                while let Some(ev) = stream.next().await {
                    let text = ev.args.iter().map(render_arg).collect::<Vec<_>>().join(" ");
                    let line = ConsoleLine {
                        level: format!("{:?}", ev.r#type).to_lowercase(),
                        text: truncate(&text, MAX_LINE_CHARS),
                    };
                    let mut b = buf.lock().unwrap();
                    // One console call can be delivered more than once.
                    let repeat = b
                        .console
                        .last()
                        .is_some_and(|p| p.level == line.level && p.text == line.text);
                    if !repeat && b.console.len() < MAX_CONSOLE {
                        b.console.push(line);
                    }
                }
            }));
        }

        Self { buffer, pumps }
    }

    /// Page reads are best-effort: a dead page must not turn a clean Down
    /// verdict into an engine error.
    pub async fn finish(self, page: &Page) -> FlowEvidence {
        let final_url = page.url().await.ok().flatten();
        let title = eval_string(page, &js_slice("document.title", MAX_LINE_CHARS)).await;
        let text_snippet = eval_string(
            page,
            &js_slice(
                "document.body?document.body.innerText:null",
                WIRE_SNIPPET_CHARS,
            ),
        )
        .await
        .map(|s| {
            truncate(
                s.split_whitespace().collect::<Vec<_>>().join(" ").as_str(),
                MAX_SNIPPET_CHARS,
            )
        })
        .filter(|s| !s.is_empty());

        for pump in &self.pumps {
            pump.abort();
        }
        let mut buffer = self.buffer.lock().unwrap();

        FlowEvidence {
            // Only the URL is scrubbed by key here. `title`, `text_snippet` and
            // the console are free text with no such structure: they carry
            // whatever the page said, and lose only the org's own secrets, in
            // `scrub_flow_evidence` on the way to storage.
            final_url: final_url.map(|u| truncate(&scrub_url(&u), MAX_URL_CHARS)),
            title: title
                .map(|t| truncate(&t, MAX_LINE_CHARS))
                .filter(|t| !t.is_empty()),
            text_snippet,
            console: std::mem::take(&mut buffer.console),
        }
    }
}

async fn eval_string(page: &Page, js: &str) -> Option<String> {
    page.evaluate(js)
        .await
        .ok()?
        .into_value::<Option<String>>()
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_marks_what_it_cut_and_counts_chars_not_bytes() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 3), "abc…");
        // Multi-byte input must not be sliced mid-character.
        assert_eq!(truncate("héllo wörld", 5), "héllo…");
    }

    #[test]
    fn render_arg_prefers_the_string_value_then_the_description() {
        use chromiumoxide::cdp::js_protocol::runtime::{RemoteObject, RemoteObjectType};
        let mut o = RemoteObject::new(RemoteObjectType::String);
        o.value = Some(serde_json::Value::String("boom".into()));
        assert_eq!(render_arg(&o), "boom");

        let mut n = RemoteObject::new(RemoteObjectType::Number);
        n.value = Some(serde_json::json!(404));
        assert_eq!(render_arg(&n), "404");

        let mut d = RemoteObject::new(RemoteObjectType::Object);
        d.description = Some("TypeError: x is not a function".into());
        assert_eq!(render_arg(&d), "TypeError: x is not a function");
    }

    #[test]
    fn render_arg_cuts_a_large_value_before_it_is_joined() {
        use chromiumoxide::cdp::js_protocol::runtime::{RemoteObject, RemoteObjectType};
        let mut o = RemoteObject::new(RemoteObjectType::String);
        o.value = Some(serde_json::Value::String("x".repeat(50_000)));
        assert_eq!(render_arg(&o).chars().count(), MAX_LINE_CHARS + 1);
    }

    #[test]
    fn page_reads_are_cut_in_the_page() {
        assert_eq!(
            js_slice("document.title", 300),
            "(function(){const v=document.title;\
             return typeof v==='string'?v.replace(/\\s+/g,' ').slice(0,300):null;})()"
        );
    }
}
