// Shared helpers for the JSON-API-backed forms (monitor form, notification
// channel form). The page-specific scripts keep their own buildBody/field
// logic; everything below is the common error-banner + escaping layer, loaded
// (deferred) before those scripts. Single source so a fix on the
// security-sensitive escaping path can't miss one form.

window.smEscapeHtml = function (s) {
    return String(s).replace(/[&<>"']/g, ch => ({
        "&": "&amp;",
        "<": "&lt;",
        ">": "&gt;",
        "\"": "&quot;",
        "'": "&#39;",
    }[ch]));
};

window.smClearFormErrors = function (banner) {
    banner.innerHTML = "";
    banner.classList.add("hidden");
    document.querySelectorAll("[aria-invalid]").forEach(el => el.removeAttribute("aria-invalid"));
};

window.smRenderClientError = function (banner, msg) {
    banner.textContent = msg;
    banner.classList.remove("hidden");
};

// The server's error message from a failed fetch Response, or `fallback`
// when the body isn't the standard error envelope.
window.smApiErrorMessage = async function (res, fallback) {
    try {
        const json = await res.json();
        if (json?.error?.message) return json.error.message;
    } catch { /* not JSON */ }
    return fallback;
};

// POST /api/v1/targets/{id}/check-now (best-effort). Returns the parsed
// body on success or null on any failure — callers handle null themselves.
window.smRunCheckNow = async function (id) {
    try {
        const r = await fetch(`/api/v1/targets/${id}/check-now`, {
            method: "POST",
            headers: {
                "Accept": "application/json",
                "X-Requested-With": "uptimepage",
            },
        });
        let body = null;
        try { body = await r.json(); } catch { /* empty body */ }
        return { ok: r.ok, status: r.status, body };
    } catch (err) {
        return { ok: false, status: 0, body: null, networkError: err };
    }
};

// Render a CheckResult-shaped object into an element using the
// .test-result* CSS classes. `extra.matched` (bool) adjusts the pill
// label; `extra.errorText` (str) is the error in plain words, shown in
// place of the result's own terse code; `extra.headers` (Vec) +
// `extra.body` (str) append HTTP-only detail sections. Returns nothing.
window.smRenderCheckResult = function (el, result, extra) {
    if (!el) return;
    extra = extra || {};
    const status = (result && result.status) || "unknown";
    const matched = extra.matched !== undefined ? !!extra.matched : status === "up";
    const klass = matched && status === "up"
        ? "test-result--ok"
        : status === "degraded"
            ? "test-result--warn"
            : "test-result--bad";
    el.className = `test-result ${klass}`;
    const pillLabel = matched ? status.toUpperCase()
        : `${status.toUpperCase()} — expectation failed`;
    const meta = [
        result.duration_ms != null ? `${result.duration_ms} ms` : null,
        result.response_code != null ? `HTTP ${result.response_code}` : null,
        result.response_size != null ? `${result.response_size} B` : null,
    ].filter(Boolean).join(" · ");
    let html = `
        <div class="flex flex-wrap items-center gap-2">
            <span class="test-result__pill">${window.smEscapeHtml(pillLabel)}</span>
            <span class="test-result__meta">${window.smEscapeHtml(meta)}</span>
        </div>
    `;
    if (extra.errorText) {
        html += `<div class="test-result__meta mt-1">${window.smEscapeHtml(extra.errorText)}</div>`;
    }
    const section = (label, text) =>
        `<details class="mt-2"><summary class="cursor-pointer text-xs">${window.smEscapeHtml(label)}</summary><pre class="test-result__body">${window.smEscapeHtml(text)}</pre></details>`;

    const headers = extra.headers || [];
    if (headers.length > 0) {
        html += section(
            `Response headers (${headers.length})`,
            headers.map(h => `${h.name}: ${h.value}`).join("\n"),
        );
    }
    if (extra.body) {
        html += section("Response body (first 1 KiB)", extra.body);
    }
    // Stands in for a screenshot: the flow engine has no renderer.
    const ev = extra.evidence;
    if (ev) {
        const where = [
            ev.final_url ? `URL:   ${ev.final_url}` : null,
            ev.title ? `Title: ${ev.title}` : null,
        ].filter(Boolean);
        if (ev.text_snippet) where.push("", ev.text_snippet);
        if (where.length > 0) html += section("Page when it failed", where.join("\n"));

        const lines = ev.console || [];
        if (lines.length > 0) {
            html += section(
                `Browser console (${lines.length})`,
                lines.map(l => `${l.level}: ${l.text}`).join("\n"),
            );
        }
    }
    if (extra.footnote) {
        html += `<div class="test-result__meta mt-1 italic">${window.smEscapeHtml(extra.footnote)}</div>`;
    }
    el.innerHTML = html;
    el.classList.remove("hidden");
};

// The advice every region that ran carries, or null. One region failing its
// own way, or passing, makes it a problem with the path there, so advice is
// shown only when they all returned the same one. A region whose request
// produced no result (rate limited, no probe) says nothing either way.
// `outcomes` = [{ result, advice }], where `advice` is the API's
// { source, code, text, docs_url? } or null.
window.smAgreedAdvice = function (outcomes) {
    const ran = outcomes.filter(o => o.result);
    const first = ran[0]?.advice;
    if (!first) return null;
    const same = (a) => a && a.source === first.source && a.code === first.code;
    return ran.every(o => same(o.advice)) ? first : null;
};

// Build a per-region test widget inside `el`: one row per region (label +
// "Running…" body). Returns { [regionId]: bodyEl } so the caller renders each
// region's outcome into its row with smRenderCheckResult / smRenderCheckError
// as the fan-out requests resolve. `regions` = [{ id, label }].
window.smRenderRegionTestRunning = function (el, regions) {
    if (!el) return {};
    if (!el.hasAttribute("role")) el.setAttribute("role", "status");
    el.className = "test-region-list sm:col-span-2";
    el.innerHTML = "";
    const rows = {};
    regions.forEach((r) => {
        const row = document.createElement("div");
        row.className = "test-region";
        const name = document.createElement("div");
        name.className = "test-region__name";
        name.textContent = r.label;
        const body = document.createElement("div");
        body.className = "test-result";
        body.textContent = "Running…";
        row.appendChild(name);
        row.appendChild(body);
        el.appendChild(row);
        rows[r.id] = body;
    });
    el.classList.remove("hidden");
    return rows;
};

window.smRenderCheckRunning = function (el) {
    if (!el) return;
    // Live-region backstop: the templates set role="status" so the first
    // update is announced; self-heal any container that forgot it.
    if (!el.hasAttribute("role")) el.setAttribute("role", "status");
    el.className = "test-result";
    el.textContent = "Running…";
    el.classList.remove("hidden");
};

window.smRenderCheckError = function (el, msg) {
    if (!el) return;
    el.className = "test-result test-result--bad";
    el.textContent = msg;
    el.classList.remove("hidden");
};

// `opts.onField(field)` → optional per-field affordance (focus/aria-invalid).
window.smRenderApiError = function (banner, json, status, opts) {
    opts = opts || {};
    const err = (json && json.error) || {};
    const code = err.code || `HTTP ${status}`;
    let message = err.message || "Request rejected.";
    if (err.code === "SSRF_BLOCKED") {
        message += ". To monitor internal services, run a monitor agent inside that network.";
    }
    banner.innerHTML = `<strong>${window.smEscapeHtml(code)}</strong>: ${window.smEscapeHtml(message)}`;
    if (err.field) {
        banner.insertAdjacentHTML("beforeend",
            ` <span class="text-xs text-state-bad">(field: ${window.smEscapeHtml(err.field)})</span>`);
    }
    banner.classList.remove("hidden");
    banner.scrollIntoView({ block: "center", behavior: "smooth" });
    if (err.field && opts.onField) opts.onField(err.field);
};
