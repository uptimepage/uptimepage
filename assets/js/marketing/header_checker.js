// Walks a URL through our own probe and prints the chain it took. The browser
// cannot do this itself: fetch follows redirects opaquely and hides both the
// intermediate hops and the response headers behind CORS.
import { chain, statusClass } from "./_redirect_chain.js";
import { toolError, toolUsed } from "./_tool_event.js";

const TOOL = "header-checker";

// Marked in the full list rather than pulled above it: each of these explains
// the status the walk just returned, or the hop that produced it, and lifting
// them out would print the interesting ones twice.
const NOTABLE = [
    "server",
    "location",
    "content-type",
    "cache-control",
    "retry-after",
    "strict-transport-security",
];

const form = document.getElementById("hdr-form");
const input = document.getElementById("hdr-url");
const out = document.getElementById("hdr-result");

if (form && input && out) {
    form.addEventListener("submit", (e) => {
        e.preventDefault();
        run();
    });
}

function el(tag, cls, text) {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined) n.textContent = text;
    return n;
}

function message(text) {
    out.replaceChildren(el("p", "tool-dns__empty mk-mono", text));
}

async function run() {
    const raw = input.value.trim();
    if (!raw || !raw.includes(".")) {
        message("That does not look like a URL.");
        toolError(TOOL, { reason: "malformed-url" });
        return;
    }

    message("Walking the chain…");
    toolUsed(TOOL);

    const url = `${form.dataset.probe}?url=${encodeURIComponent(raw)}`;
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

    out.replaceChildren(render(body));
}

function failureReason(res, body) {
    if (!res.ok) return `status-${res.status}`;
    if (!body) return "malformed-body";
    return "host-unreachable";
}

// A redirect is neither good nor bad on its own, so it stays neutral until the
// walk ends somewhere. Only the final code is coloured.
function host(url) {
    try {
        return new URL(url).host;
    } catch {
        return "";
    }
}

// Each of these is something the final status code alone does not say.
function notices(r) {
    const found = [];
    if (r.redirect_loop) {
        found.push(
            "The chain came back to a URL it had already requested. This is the loop a browser reports as ERR_TOO_MANY_REDIRECTS.",
        );
    }
    if (r.hop_limit_hit) {
        found.push(
            "Still redirecting when the hop limit ran out. A monitor following this chain records a failure, not the page at the end of it.",
        );
    }
    const first = host(r.url);
    const last = host(r.final_url);
    if (first && last && first !== last) {
        found.push(
            `The chain ends on ${last}, not ${first}. Cookies and authorization headers are dropped when a redirect crosses hosts.`,
        );
    }
    if (r.final_url.startsWith("http://")) {
        found.push("The chain ends on plain HTTP. Anything sent to this URL travels unencrypted.");
    }
    if (r.hops.length > 2) {
        found.push(
            `${r.hops.length} requests before an answer. Every hop is a fresh connection, and on HTTPS a fresh handshake.`,
        );
    }
    return found;
}

function headerRows(r) {
    const frag = document.createDocumentFragment();
    if (!r.headers.length) return frag;

    const count = r.headers.length + (r.headers_truncated ? " (the first of more)" : "");
    frag.append(el("p", "tool-hdr__count mk-mono", `${count} response headers`));

    const list = el("dl", "tool-ssl__facts tool-hdr__keys");
    for (const [name, value] of r.headers) {
        const notable = NOTABLE.includes(name.toLowerCase());
        list.append(
            el("dt", `tool-metric__label${notable ? " tool-hdr__key--notable" : ""}`, name),
        );
        list.append(el("dd", "tool-ssl__value mk-mono", value));
    }
    frag.append(list);
    return frag;
}

function render(r) {
    const frag = document.createDocumentFragment();

    const head = el("p", "tool-dns__head mk-mono");
    head.append(el("span", "tool-dns__q", r.url));
    frag.append(head);

    const verdict = el("p", "tool-hdr__verdict mk-mono");
    verdict.append(
        el("span", `tool-hdr__status ${statusClass(r.final_status)}`, String(r.final_status)),
    );
    const hops = r.hops.length === 1 ? "1 request" : `${r.hops.length} requests`;
    verdict.append(el("span", "tool-hdr__final", `${hops} · ${r.total_ms} ms total`));
    frag.append(verdict);

    frag.append(chain(r));
    frag.append(headerRows(r));

    for (const text of notices(r)) {
        frag.append(el("p", "tool-dns__warn mk-mono", text));
    }

    const cta = el("a", "mk-cta mk-cta--primary tool-dns__cta", "monitor this URL");
    cta.href = `/start?kind=http&url=${encodeURIComponent(r.url)}`;
    cta.dataset.umamiEvent = "signup-start";
    cta.dataset.umamiEventPosition = "tool-header-result";
    frag.append(cta);

    return frag;
}
