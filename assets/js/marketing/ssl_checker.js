// Reads a host's TLS certificate through our own probe. Unlike the DNS tool
// this cannot run in the browser: fetch exposes no part of the peer chain, so
// the handshake happens server-side and only the parsed fields come back.
import { toolError, toolUsed } from "./_tool_event.js";

const TOOL = "ssl-checker";

// Matches the monitor's own thresholds, so the page and an alert agree about
// what "getting close" means.
const WARN_DAYS = 30;
const CRITICAL_DAYS = 14;

const form = document.getElementById("ssl-form");
const hostInput = document.getElementById("ssl-host");
const portInput = document.getElementById("ssl-port");
const out = document.getElementById("ssl-result");

if (form && hostInput && portInput && out) {
    form.addEventListener("submit", (e) => {
        e.preventDefault();
        run();
    });
}

// Strips a pasted scheme and path so "https://acme.com/pricing" still checks.
function cleanHost(raw) {
    let s = raw.trim().replace(/^[a-z][a-z0-9+.-]*:\/\//i, "");
    s = s.split("/")[0].split("?")[0].split("#")[0];
    return s.replace(/\.$/, "");
}

function el(tag, cls, text) {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined) n.textContent = text;
    return n;
}

function replace(node) {
    out.replaceChildren(node);
}

function message(text) {
    replace(el("p", "tool-dns__empty mk-mono", text));
}

async function run() {
    const host = cleanHost(hostInput.value);
    const port = portInput.value;

    if (!host || !host.includes(".")) {
        message("That does not look like a hostname.");
        toolError(TOOL, { reason: "malformed-host" });
        return;
    }

    message("Opening a handshake…");
    toolUsed(TOOL, { mode: port });

    const url = `${form.dataset.probe}?host=${encodeURIComponent(host)}&port=${encodeURIComponent(port)}`;
    let res;
    try {
        res = await fetch(url, { headers: { accept: "application/json" } });
    } catch {
        message("Could not reach the checker. Check your connection and try again.");
        toolError(TOOL, { reason: "probe-unreachable" });
        return;
    }

    // Parsed separately: a body that is not JSON means the server answered,
    // so saying it was unreachable would send the visitor after the wrong bug.
    let body = null;
    try {
        body = await res.json();
    } catch {
        body = null;
    }

    // A host that would not answer comes back 200, so the status line cannot
    // decide this.
    if (!body || body.ok !== true) {
        message(body?.error ?? "The check did not complete.");
        toolError(TOOL, { reason: failureReason(res, body) });
        return;
    }

    replace(render(body));
}

function failureReason(res, body) {
    if (!res.ok) return `status-${res.status}`;
    if (!body) return "malformed-body";
    return "host-unreachable";
}

function verdictClass(r) {
    if (r.expired) return "tool-ssl__verdict--dead";
    if (r.days_remaining <= CRITICAL_DAYS) return "tool-ssl__verdict--critical";
    if (r.days_remaining <= WARN_DAYS) return "tool-ssl__verdict--warn";
    return "tool-ssl__verdict--ok";
}

// The day count truncates toward zero, so a certificate that died six hours
// ago arrives as 0 and only the server's `expired` flag can tell that from a
// certificate with hours left.
function verdictText(r) {
    const days = Math.abs(r.days_remaining);
    const unit = days === 1 ? "day" : "days";
    if (r.expired) return days === 0 ? "expired today" : `expired ${days} ${unit} ago`;
    if (r.days_remaining === 0) return "expires today";
    return `${days} ${unit} left`;
}

function day(iso) {
    // Date only: an expiry hour is never the thing anyone acts on.
    return iso.slice(0, 10);
}

// Only what a person would act on. Everything else the certificate carries is
// noise on a page whose job is to answer one question.
function facts(r) {
    const rows = [
        ["issued to", r.subject_common_name ?? "—"],
        ["issued by", r.issuer_organization ?? r.issuer_common_name ?? "—"],
        ["valid", `${day(r.not_before)} → ${day(r.not_after)}`],
        ["covers", r.san_dns_names.length ? r.san_dns_names.join(", ") : "no SAN names"],
        ["chain", `${r.chain_len} certificate${r.chain_len === 1 ? "" : "s"} sent`],
        ["handshake", `${r.handshake_ms} ms from ${r.resolved_ip}`],
    ];

    const list = el("dl", "tool-ssl__facts");
    for (const [label, value] of rows) {
        list.append(el("dt", "tool-metric__label", label));
        list.append(el("dd", "tool-ssl__value mk-mono", value));
    }
    return list;
}

// Each of these is a live failure for some client even while the dates are
// fine, which is exactly the case a date-only check misses.
function warnings(r) {
    const found = [];
    if (!r.name_matches) {
        found.push(
            `The certificate does not cover ${r.host}. Every client rejects it for this name.`,
        );
    }
    if (r.self_signed) {
        found.push("Self-signed: no authority vouches for it, so public clients refuse it.");
    }
    if (r.chain_len === 1) {
        found.push(
            "Only the leaf was sent. Browsers often paper over a missing intermediate from cache while fresh clients and server-to-server calls fail.",
        );
    }
    return found;
}

function render(r) {
    const frag = document.createDocumentFragment();

    const head = el("p", "tool-dns__head mk-mono");
    head.append(el("span", "tool-dns__q", `${r.host}:${r.port}`));
    frag.append(head);

    const verdict = el("p", `tool-ssl__verdict mk-mono ${verdictClass(r)}`);
    verdict.append(el("span", "tool-ssl__days", verdictText(r)));
    const when = r.expired ? "expired" : "expires";
    verdict.append(el("span", "tool-ssl__until", `${when} ${day(r.not_after)}`));
    frag.append(verdict);

    frag.append(facts(r));

    for (const text of warnings(r)) {
        frag.append(el("p", "tool-dns__warn mk-mono", text));
    }

    const cta = el("a", "mk-cta mk-cta--primary tool-dns__cta", "monitor this certificate");
    cta.href = `/start?kind=tls_cert&url=${encodeURIComponent(r.host)}`;
    cta.dataset.umamiEvent = "signup-start";
    cta.dataset.umamiEventPosition = "tool-ssl-result";
    frag.append(cta);

    if (r.port === 443) {
        const next = el("p", "tool-dns__note");
        const link = el("a", "mk-link", "Check this website’s HTTPS and security headers");
        link.href = `/tools/website-security-checker?url=${encodeURIComponent(`https://${r.host}/`)}`;
        next.append(link);
        frag.append(next);
    }

    return frag;
}
