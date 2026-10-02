// Detail-page wiring: Run-check-now button, Enable/Disable toggle, and a
// one-shot nudge that resolves a just-created monitor's first status.
//
// The KPI cards + Recent results table live inside `#detail-live-kpi`, an
// htmx-polled partial that re-renders every 60s (or on demand via the
// `sm:refresh-live` body event). We don't re-render those regions
// client-side — the server template is the single source of truth.

(function () {
    function refreshLive() {
        // Tell the #detail-live-kpi partial to refresh now. Safe no-op
        // if htmx isn't on the page or the region isn't mounted yet.
        if (window.htmx && document.getElementById("detail-live-kpi")) {
            window.htmx.trigger("body", "sm:refresh-live");
        }
    }

    // Run check now: persist a fresh check, render the verbose result
    // pill, then ask the live partial to pick up the new row + uptime.
    const btn = document.querySelector("[data-detail-test-now]");
    const resultEl = document.querySelector("[data-detail-test-result]");
    if (btn && resultEl) {
        btn.addEventListener("click", async () => {
            const id = btn.dataset.targetId;
            if (!id) return;
            btn.disabled = true;
            window.smRenderCheckRunning(resultEl);
            try {
                const r = await window.smRunCheckNow(id);
                if (!r.ok) {
                    const code = (r.body && r.body.error && r.body.error.code)
                        || (r.networkError ? "network" : `HTTP ${r.status}`);
                    const message = (r.body && r.body.error && r.body.error.message)
                        || (r.networkError ? String(r.networkError.message || r.networkError)
                                           : "Check rejected.");
                    window.smRenderCheckError(resultEl, `${code}: ${message}`);
                    return;
                }
                window.smRenderCheckResult(resultEl, r.body || {}, {
                    footnote: "Metrics and charts update automatically.",
                });
                refreshLive();
            } finally {
                btn.disabled = false;
            }
        });
    }

    // The badge is server-rendered on a 60s poll, but a heartbeat crosses its
    // due time on its own schedule. Promote up→late from the clock; never the
    // other way, so the server stays the only thing that can clear it.
    function markLateWhenDue() {
        const panel = document.querySelector("[data-hb-due]");
        const badge = document.getElementById("detail-status-badge");
        if (!panel || !badge || !badge.classList.contains("status-badge--up")) return;
        // Absent means zero grace: no late window, and down is the server's call.
        if (!panel.dataset.hbDown) return;
        const due = Date.parse(panel.dataset.hbDue);
        const down = Date.parse(panel.dataset.hbDown);
        const now = Date.now();
        if (!(now > due && now <= down)) return;
        badge.classList.replace("status-badge--up", "status-badge--late");
        badge.textContent = "late";
        panel.classList.replace("border-[color:var(--theme-line)]", "border-[color:var(--theme-state-warn-line)]");
        const note = panel.querySelector("[data-hb-late-note]");
        if (note) note.hidden = false;
    }
    markLateWhenDue();
    setInterval(markLateWhenDue, 5000);
    document.body.addEventListener("htmx:afterSwap", markLateWhenDue);

    // Region filter: full-page nav (so the chart modules re-init) preserving
    // the current range. Empty value clears the filter back to all regions.
    const regionSel = document.querySelector("[data-region-filter]");
    if (regionSel) {
        regionSel.addEventListener("change", () => {
            const url = new URL(location.href);
            if (regionSel.value) url.searchParams.set("region", regionSel.value);
            else url.searchParams.delete("region");
            location.assign(url.pathname + url.search);
        });
    }

    // Widens the region link's hit target to its whole row; the anchor stays the
    // only source of the URL. Modified clicks and text selection fall through.
    for (const row of document.querySelectorAll("[data-region-row]")) {
        row.addEventListener("click", (e) => {
            if (e.target.closest("a") || e.metaKey || e.ctrlKey || e.shiftKey) return;
            if (window.getSelection()?.toString()) return;
            row.querySelector("a")?.click();
        });
    }

    // Enable/Disable toggle.
    const toggleBtn = document.querySelector('[data-action="toggle-enabled"]');
    const toggleErr = document.querySelector("[data-detail-toggle-error]");
    if (toggleBtn) {
        toggleBtn.addEventListener("click", async () => {
            const id = toggleBtn.dataset.targetId;
            const current = toggleBtn.dataset.current === "true";
            if (!id) return;
            toggleBtn.disabled = true;
            if (toggleErr) toggleErr.classList.add("hidden");
            try {
                const r = await fetch(`/api/v1/targets/${id}`, {
                    method: "PATCH",
                    headers: {
                        "Content-Type": "application/json",
                        "Accept": "application/json",
                        "X-Requested-With": "uptimepage",
                    },
                    body: JSON.stringify({ enabled: !current }),
                });
                if (r.ok) {
                    window.location.reload();
                    return;
                }
                let body = null;
                try { body = await r.json(); } catch { /* empty */ }
                const err = (body && body.error) || {};
                const msg = `${err.code || `HTTP ${r.status}`}: ${err.message || "Toggle failed."}`;
                if (toggleErr) {
                    toggleErr.textContent = msg;
                    toggleErr.classList.remove("hidden");
                }
            } catch (err) {
                const msg = `network: ${String(err.message || err)}`;
                if (toggleErr) {
                    toggleErr.textContent = msg;
                    toggleErr.classList.remove("hidden");
                }
            } finally {
                toggleBtn.disabled = false;
            }
        });
    }

    async function callAndReload(path, method, body) {
        try {
            const r = await fetch(path, {
                method,
                headers: {
                    "Content-Type": "application/json",
                    "Accept": "application/json",
                    "X-Requested-With": "uptimepage",
                },
                body: body ? JSON.stringify(body) : undefined,
            });
            if (r.ok) {
                window.location.reload();
                return true;
            }
            let msg = `HTTP ${r.status}`;
            try {
                const b = await r.json();
                if (b && b.error && b.error.message) msg = b.error.message;
            } catch { /* empty */ }
            window.smToast?.({ message: msg });
        } catch (err) {
            window.smToast?.({ message: `network: ${String(err.message || err)}` });
        }
        return false;
    }

    // Each press is a whole statement: the state plus the note as typed.
    const manualPanel = document.querySelector("[data-manual-state]");
    if (manualPanel) {
        const buttons = manualPanel.querySelectorAll("[data-manual-set]");
        buttons.forEach((b) => {
            b.addEventListener("click", async () => {
                const note = manualPanel.querySelector("[data-manual-note]")?.value.trim() || null;
                buttons.forEach((x) => { x.disabled = true; });
                const done = await callAndReload(
                    `/api/v1/targets/${manualPanel.dataset.targetId}/state`, "PUT",
                    { status: b.dataset.manualSet, note });
                if (!done) buttons.forEach((x) => { x.disabled = false; });
            });
        });
    }

    const hbRotateBtn = document.querySelector("[data-hb-rotate]");
    if (hbRotateBtn) {
        hbRotateBtn.addEventListener("click", async () => {
            const id = hbRotateBtn.dataset.targetId;
            if (!id) return;
            // One overlap slot: rotating again drops the token already in it.
            const ok = await window.smConfirm({
                title: "Rotate ping URL?",
                body: document.querySelector("[data-hb-revoke-prev]")
                    ? "An overlap is already open. Rotating again revokes the " +
                      "previous URL immediately rather than in 24 hours, so " +
                      "anything still calling it stops counting now."
                    : "A new URL is minted immediately. The old one keeps " +
                      "working for 24 hours so you can update the job first; " +
                      "this card will show when it was last used.",
                confirmLabel: "Rotate",
            });
            if (!ok) return;
            // Stays disabled on success so the pending reload can't double-fire.
            hbRotateBtn.disabled = true;
            const done = await callAndReload(`/api/v1/targets/${id}/heartbeat/rotate`, "POST",
                { revoke_previous_immediately: false });
            if (!done) hbRotateBtn.disabled = false;
        });
    }

    const hbRevokePrevBtn = document.querySelector("[data-hb-revoke-prev]");
    if (hbRevokePrevBtn) {
        hbRevokePrevBtn.addEventListener("click", async () => {
            const id = hbRevokePrevBtn.dataset.targetId;
            if (!id) return;
            const ok = await window.smConfirm({
                title: "Revoke the old URL now?",
                body: "Anything still pinging the old URL stops counting " +
                    "immediately and will read as down once the period and " +
                    "grace run out.",
                confirmLabel: "Revoke",
                danger: true,
            });
            if (!ok) return;
            hbRevokePrevBtn.disabled = true;
            const done = await callAndReload(`/api/v1/targets/${id}/heartbeat/previous`, "DELETE");
            if (!done) hbRevokePrevBtn.disabled = false;
        });
    }

    // Result-row timing expansion: delegated from document so it works for
    // server-rendered rows in the ribbon drill drawer and the share table.
    document.addEventListener("click", (ev) => {
        const row = ev.target.closest("[data-result-row]");
        if (!row) return;
        const detail = row.nextElementSibling;
        if (!detail || !detail.hasAttribute("data-result-detail")) return;
        const open = detail.classList.toggle("hidden");
        row.setAttribute("aria-expanded", String(!open));
    });

    // Header ⋯ overflow menu: native <details> stays open on outside click, so
    // dismiss it on any click outside or Escape.
    const closeHdrMenus = (except) => {
        document.querySelectorAll("details.hdr-menu[open]").forEach((d) => {
            if (d !== except) d.removeAttribute("open");
        });
    };
    document.addEventListener("click", (ev) => {
        closeHdrMenus(ev.target.closest("details.hdr-menu"));
    });
    document.addEventListener("keydown", (ev) => {
        if (ev.key === "Escape") closeHdrMenus(null);
    });

    // A just-created monitor has no result until its first check lands.
    // Poll until it does so "checking…" resolves without waiting for the
    // 60s cadence, then stop — no steady-state extra requests.
    const lastCheck = document.querySelector("[data-last-check]");
    if (lastCheck && lastCheck.dataset.enabled === "true" && !lastCheck.dataset.lastAt) {
        const nudge = setInterval(() => {
            if (document.visibilityState === "visible") refreshLive();
        }, 5000);
        document.body.addEventListener("htmx:afterSettle", (ev) => {
            const target = ev.detail && ev.detail.target;
            if (target && target.id === "detail-live-kpi" && target.dataset.newestTs) {
                clearInterval(nudge);
            }
        });
    }
})();
