import { chain } from "./_redirect_chain.js";
import { toolError, toolUsed } from "./_tool_event.js";
import { assess, targetURLs, validReport } from "./_security_checks.js";

const TOOL = "security-checker";
const form = document.getElementById("security-form");
const input = document.getElementById("security-url");
const button = document.getElementById("security-submit");
const status = document.getElementById("security-status");
const out = document.getElementById("security-result");
let running = false;
const labels = { fail: "fail", warning: "warning", unknown: "not checked", pass: "pass" };

if (form && input && button && status && out) {
    // Prefill the handoff from the SSL checker, but never run a scan on a GET.
    const prefill = new URLSearchParams(window.location.search).get("url");
    if (prefill) {
        try { input.value = targetURLs(prefill).https; } catch { /* Ignore invalid handoffs. */ }
    }
    form.addEventListener("submit", e => { e.preventDefault(); run(); });
}

function el(tag, cls, text) {
    const node = document.createElement(tag);
    if (cls) node.className = cls;
    if (text !== undefined) node.textContent = text;
    return node;
}

async function probe(url, kind) {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 16000);
    try {
        const response = await fetch(url, { headers: { accept: "application/json" }, signal: controller.signal });
        let body;
        try { body = await response.json(); }
        catch { return { error: "The checker returned an unreadable response. Try again shortly." }; }
        if (!response.ok || body?.ok !== true) {
            toolError(TOOL, { reason: response.ok ? "probe-incomplete" : `status-${response.status}` });
            return { error: typeof body?.error === "string" ? body.error : "This check did not complete. Please try again." };
        }
        if (!validReport(kind, body)) return { error: "The checker returned incomplete data. Try again shortly." };
        return { report: body };
    } catch (error) {
        toolError(TOOL, { reason: error.name === "AbortError" ? "timeout" : "probe-unreachable" });
        return { error: error.name === "AbortError" ? "The check timed out. Try again shortly." : "Could not reach the checker. Check your connection and try again." };
    } finally { clearTimeout(timeout); }
}

async function run() {
    if (running) return;
    let target;
    try { target = targetURLs(input.value); }
    catch (error) {
        status.textContent = error.message;
        out.replaceChildren();
        input.setAttribute("aria-invalid", "true");
        input.focus();
        toolError(TOOL, { reason: "invalid-url" });
        return;
    }
    running = true;
    button.disabled = true;
    input.setAttribute("aria-invalid", "false");
    form.setAttribute("aria-busy", "true");
    out.replaceChildren();
    status.textContent = "reading the certificate and following https…";
    toolUsed(TOOL); // Never send the submitted domain, path or query to analytics.
    try {
        const [ssl, https] = await Promise.all([
            probe(`${form.dataset.sslProbe}?host=${encodeURIComponent(target.host)}&port=443`, "ssl"),
            probe(`${form.dataset.headerProbe}?url=${encodeURIComponent(target.https)}`, "headers"),
        ]);
        // Reuse the same header limiter, sequentially. Long chains can consume
        // its remaining budget; that produces an explicit incomplete check.
        status.textContent = "following http…";
        const http = await probe(`${form.dataset.headerProbe}?url=${encodeURIComponent(target.http)}`, "headers");
        const checks = assess(target, ssl, https, http);
        render(target, checks, https.report, http.report);
        status.textContent = "report ready";
    } catch {
        status.textContent = "The report could not be completed. Try again.";
        toolError(TOOL, { reason: "report-error" });
    } finally {
        running = false;
        button.disabled = false;
        form.setAttribute("aria-busy", "false");
    }
}

// One line a person can act on before reading anything else: what failed,
// or what is left to look at.
function headline(counts, total) {
    if (counts.fail) return [`${counts.fail} failed`, "down"];
    if (counts.warning) return [`${counts.warning} to review`, "warn"];
    if (counts.unknown) return [`${counts.unknown} not checked`, "quiet"];
    return [`all ${total} passed`, "ok"];
}

function plural(n, word) {
    return `${n} ${word}${n === 1 ? "" : "s"}`;
}

function verdict(target, checks, https) {
    const counts = Object.fromEntries(Object.keys(labels).map(key => [key, checks.filter(c => c.status === key).length]));
    const [text, tone] = headline(counts, checks.length);
    const block = el("div", `tool-security__verdict mk-mono tool-security__verdict--${tone}`);
    block.append(el("p", "tool-security__headline", text));
    block.append(el("p", "tool-security__counts",
        `${plural(checks.length, "check")} · ${counts.fail} failed · ${plural(counts.warning, "warning")} · ${counts.unknown} not checked · ${counts.pass} passed`));
    if (https && https.final_url !== target.https) {
        block.append(el("p", "tool-security__counts", `headers read from ${https.final_url}`));
    }
    return block;
}

function findings(checks) {
    const list = el("ol", "tool-security__findings");
    for (const check of checks) {
        const item = el("li", `tool-security__finding tool-security__finding--${check.status}`);
        item.append(el("span", "tool-security__badge mk-mono", labels[check.status]));
        item.append(el("h3", "tool-security__title", check.title));
        item.append(el("p", "tool-security__evidence mk-mono", check.evidence));
        item.append(el("p", "tool-security__advice mk-body", check.advice));
        list.append(item);
    }
    return list;
}

function chains(https, http) {
    const frag = document.createDocumentFragment();
    for (const [label, report] of [["https request chain", https], ["http request chain", http]]) {
        if (!report) continue;
        const details = el("details", "tool-security__chain");
        details.append(el("summary", "mk-mono", `${label} · ${plural(report.hops.length, "request")} · ${report.total_ms} ms`));
        details.append(chain(report));
        frag.append(details);
    }
    return frag;
}

// Plain text for a ticket or a message to whoever runs the server.
function plainText(target, checks) {
    const lines = [`website security check · ${target.host}`, `${new Date().toISOString()} · ${location.origin}${location.pathname}`, ""];
    for (const check of checks) {
        lines.push(`[${labels[check.status]}] ${check.title}`, `  ${check.evidence}`, `  ${check.advice}`, "");
    }
    return lines.join("\n");
}

function copyButton(target, checks) {
    const copy = el("button", "mk-cta mk-cta--ghost", "copy report");
    copy.type = "button";
    copy.addEventListener("click", async () => {
        window.umami?.track("tool-copy", { tool: TOOL });
        try {
            await navigator.clipboard.writeText(plainText(target, checks));
            copy.textContent = "copied";
        } catch {
            copy.textContent = "press ctrl+c";
        }
        setTimeout(() => { copy.textContent = "copy report"; }, 1600);
    });
    return copy;
}

function render(target, checks, https, http) {
    const fragment = document.createDocumentFragment();
    const head = el("p", "tool-dns__head mk-mono");
    head.append(el("span", "tool-dns__q", target.host));
    fragment.append(head, verdict(target, checks, https), findings(checks), chains(https, http));
    fragment.append(el("p", "tool-dns__note", "A pass covers the stated check only. Malware, application vulnerabilities, mixed content, cookies and signed-in pages were not tested."));

    const actions = el("div", "tool-security__actions");
    // Only offer a handoff when at least one probe returned usable evidence.
    if (checks.some(c => c.status !== "unknown")) {
        for (const [text, kind, value, cls] of [
            ["monitor this website", "http", target.https, "mk-cta--primary"],
            ["monitor its certificate", "tls_cert", target.host, "mk-cta--ghost"],
        ]) {
            const link = el("a", `mk-cta ${cls}`, text);
            link.href = `/start?kind=${kind}&url=${encodeURIComponent(value)}`;
            link.dataset.umamiEvent = "signup-start";
            link.dataset.umamiEventPosition = "tool-security-result";
            actions.append(link);
        }
        actions.append(copyButton(target, checks));
    }
    fragment.append(actions);
    out.replaceChildren(fragment);
}
