import { toolError, toolUsed } from "./_tool_event.js";
import { assess, targetURLs, validReport } from "./_security_checks.js";

const TOOL = "security-checker";
const form = document.getElementById("security-form");
const input = document.getElementById("security-url");
const button = document.getElementById("security-submit");
const status = document.getElementById("security-status");
const out = document.getElementById("security-result");
let running = false;
const labels = { fail: "Fail", warning: "Warning", unknown: "Not checked", pass: "Pass" };

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
    button.textContent = "Checking…";
    input.setAttribute("aria-invalid", "false");
    form.setAttribute("aria-busy", "true");
    out.replaceChildren();
    status.textContent = "Checking the certificate and HTTPS response…";
    toolUsed(TOOL); // Never send the submitted domain, path or query to analytics.
    try {
        const [ssl, https] = await Promise.all([
            probe(`${form.dataset.sslProbe}?host=${encodeURIComponent(target.host)}&port=443`, "ssl"),
            probe(`${form.dataset.headerProbe}?url=${encodeURIComponent(target.https)}`, "headers"),
        ]);
        // Reuse the same header limiter, sequentially. Long chains can consume
        // its remaining budget; that produces an explicit incomplete check.
        status.textContent = "Checking whether HTTP redirects to HTTPS…";
        const http = await probe(`${form.dataset.headerProbe}?url=${encodeURIComponent(target.http)}`, "headers");
        const checks = assess(target, ssl, https, http);
        render(target, checks, https.report, http.report);
        const counts = Object.fromEntries(Object.keys(labels).map(key => [key, checks.filter(c => c.status === key).length]));
        status.textContent = `${checks.length} checks: ${counts.fail} failed, ${counts.warning} warnings, ${counts.unknown} not checked, ${counts.pass} passed.`;
    } catch {
        status.textContent = "The report could not be completed. Try again.";
        toolError(TOOL, { reason: "report-error" });
    } finally {
        running = false;
        button.disabled = false;
        button.textContent = "Check website";
        form.setAttribute("aria-busy", "false");
    }
}

function render(target, checks, https, http) {
    const fragment = document.createDocumentFragment();
    fragment.append(el("h2", "mk-h2", "Your security configuration report"));
    fragment.append(el("p", "tool-security__scope mk-mono", `Certificate: ${target.host}:443`));
    if (https) fragment.append(el("p", "tool-security__scope mk-mono", `Final HTTPS-request response: ${https.final_url}`));
    fragment.append(el("p", "tool-security__scope mk-mono", `Checked ${new Date().toLocaleString()}. One location; no login or browser rendering.`));

    const list = el("ol", "tool-security__findings");
    for (const check of checks) {
        const item = el("li", `tool-security__finding tool-security__finding--${check.status}`);
        const head = el("div", "tool-security__heading");
        head.append(el("span", "tool-security__badge mk-mono", labels[check.status]));
        head.append(el("h3", "tool-security__title", check.title));
        item.append(head, el("p", "tool-security__evidence mk-mono", check.evidence), el("p", "mk-body", check.advice));
        list.append(item);
    }
    fragment.append(list);
    for (const [label, report] of [["HTTPS request chain", https], ["HTTP request chain", http]]) {
        if (!report) continue;
        const details = el("details", "mk-faq");
        details.append(el("summary", "", label));
        const hops = el("ol", "tool-security__chain mk-mono");
        for (const hop of report.hops) hops.append(el("li", "", `${hop.status} · ${hop.url}`));
        details.append(hops);
        fragment.append(details);
    }
    fragment.append(el("p", "tool-security__scope", "Pass means only that the stated check passed. Malware, application vulnerabilities, mixed content, cookies and authenticated pages were not tested."));
    // Only offer a handoff when at least one probe returned usable evidence.
    if (checks.some(c => c.status !== "unknown")) {
        const actions = el("div", "tool-security__actions");
        for (const [text, kind, value] of [["Monitor certificate expiry", "tls_cert", target.host], ["Monitor website uptime", "http", target.https]]) {
            const link = el("a", "mk-cta mk-cta--primary", text);
            link.href = `/start?kind=${kind}&url=${encodeURIComponent(value)}`;
            link.dataset.umamiEvent = "signup-start";
            link.dataset.umamiEventPosition = "tool-security-result";
            actions.append(link);
        }
        fragment.append(actions);
    }
    out.replaceChildren(fragment);
}
