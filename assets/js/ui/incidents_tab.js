// Incidents sub-tab: row-click toggle + lazy-loaded timeline drawer.

(function () {
    const TIMELINE_FETCH_LIMIT = 50;
    const PAD_MS = 5_000;
    const TAIL_PAD_MS = 30_000;

    // Mirrors CheckStatus::is_bad: a degraded check keeps an incident open.
    const badEnough = (s) => s === "down" || s === "error" || s === "degraded";
    const goodEnough = (s) => s === "up";

    function fmtTime(iso) {
        const d = new Date(iso);
        return Number.isNaN(d.getTime()) ? iso : d.toLocaleString();
    }

    function pickTimeline(items, ongoing) {
        const asc = items.slice().reverse();
        const firstFailure = asc.find((it) => badEnough(it.status)) || null;
        let recovered = null;
        let lastFailure = null;
        if (firstFailure) {
            const firstIdx = asc.indexOf(firstFailure);
            for (let i = firstIdx + 1; i < asc.length; i++) {
                if (goodEnough(asc[i].status)) {
                    recovered = asc[i];
                    lastFailure = asc[i - 1];
                    break;
                }
            }
            if (!recovered) {
                lastFailure = asc.slice(firstIdx).reverse().find((it) => badEnough(it.status));
            }
        }
        if (ongoing) recovered = null;
        // Suppress duplicate row when lastFailure collapses onto firstFailure.
        if (lastFailure && firstFailure && lastFailure.timestamp === firstFailure.timestamp) {
            lastFailure = null;
        }
        return { firstFailure, lastFailure, recovered };
    }

    function el(tag, className, text) {
        const node = document.createElement(tag);
        node.className = className;
        if (text !== undefined) node.textContent = text;
        return node;
    }

    const note = (text) => el("span", "font-mono text-xs text-quiet", text);
    const failure = (text) => el("span", "flash-text flash-text--bad", text);

    function eventRow(label, ev) {
        if (!ev) return null;
        const meta = ev.response_code
            ? `HTTP ${ev.response_code}`
            : (ev.error ? ev.error : `${ev.duration_ms}ms`);
        const time = el("time", "font-mono text-xs tabular-nums", fmtTime(ev.timestamp));
        time.dateTime = ev.timestamp;
        const outcome = el("span", "min-w-0 text-body [overflow-wrap:anywhere]");
        outcome.append(el("span", `status-badge status-badge--${ev.status} mr-2`, ev.status), meta);
        const row = el("div", "grid grid-cols-1 gap-x-3 gap-y-0.5 py-1 sm:grid-cols-[7rem_11rem_1fr]");
        row.append(el("span", "font-mono text-xs text-quiet", label), time, outcome);
        return row;
    }

    async function loadTimeline(row, detail) {
        const body = detail.querySelector("[data-incident-detail-body]");
        if (!body) return;
        // `data-results-base` carries the per-surface results prefix
        // (`/api/v1/targets/{id}` operator-side, `/m/{token}` on a shared page),
        // so the same drawer works without leaking an operator id onto a share.
        const base = row.dataset.resultsBase;
        const fromIso = row.dataset.from;
        const toIso = row.dataset.to;
        const ongoing = row.dataset.ongoing === "true";

        const fromRaw = new Date(fromIso).getTime();
        const toRaw = toIso ? new Date(toIso).getTime() : Date.now();
        if (Number.isNaN(fromRaw) || Number.isNaN(toRaw)) {
            body.replaceChildren(failure("could not load timeline: invalid timestamp on incident row"));
            return;
        }
        const url = `${base}/results`
            + `?from=${encodeURIComponent(new Date(fromRaw - PAD_MS).toISOString())}`
            + `&to=${encodeURIComponent(new Date(toRaw + TAIL_PAD_MS).toISOString())}`
            + `&limit=${TIMELINE_FETCH_LIMIT}`;

        body.replaceChildren(note("# loading timeline…"));
        try {
            const r = await fetch(url, { headers: { "Accept": "application/json" } });
            if (!r.ok) throw new Error(`HTTP ${r.status}`);
            const json = await r.json();
            const items = Array.isArray(json.items) ? json.items : [];
            if (items.length === 0) {
                body.replaceChildren(note("# no checks are stored for this window"));
                return;
            }
            const tl = pickTimeline(items, ongoing);
            const rows = [
                eventRow("first failure", tl.firstFailure),
                eventRow("last failure", tl.lastFailure),
                eventRow("recovered", tl.recovered),
            ].filter(Boolean);
            body.replaceChildren(...(rows.length
                ? rows
                : [note("# no failing checks are stored for this window")]));
        } catch (err) {
            body.replaceChildren(failure(`could not load timeline: ${String(err.message || err)}`));
            delete detail.dataset.loaded;
        }
    }

    function toggleFor(incidentId, btn) {
        const detail = document.querySelector(
            `[data-incident-detail][data-for="${CSS.escape(incidentId)}"]`,
        );
        const row = document.querySelector(
            `[data-incident-row][data-incident-id="${CSS.escape(incidentId)}"]`,
        );
        if (!detail || !row) return;
        const isOpen = !detail.hasAttribute("hidden");
        if (isOpen) {
            detail.setAttribute("hidden", "");
            btn?.setAttribute("aria-expanded", "false");
            return;
        }
        detail.removeAttribute("hidden");
        btn?.setAttribute("aria-expanded", "true");
        if (detail.dataset.loaded !== "1") {
            detail.dataset.loaded = "1";
            loadTimeline(row, detail);
        }
    }

    document.body.addEventListener("click", (ev) => {
        const btn = ev.target.closest("[data-incident-expand]");
        if (btn) {
            ev.preventDefault();
            toggleFor(btn.dataset.incidentId, btn);
            return;
        }
        const row = ev.target.closest("[data-incident-row]");
        if (!row) return;
        if (ev.target.closest("a, button")) return;
        const id = row.dataset.incidentId;
        const sibling = document.querySelector(
            `[data-incident-expand][data-incident-id="${CSS.escape(id)}"]`,
        );
        toggleFor(id, sibling);
    });
})();
