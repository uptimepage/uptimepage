// Schedule calendar on the on-call edit page. The server renders each month:
// who is on call each day, overrides highlighted, and the overrides still to
// come. Click a start day, then an end day, then pick who covers; the override
// is POSTed to /api/v1/on-call/schedules/{id}/overrides and the month reloads.
// Every day carries its own window as instants, so nothing here reads zones,
// and the API refuses an override the same person already covers.

(function () {
    const root = document.querySelector("[data-overrides]");
    if (!root) return;
    const scheduleId = root.getAttribute("data-schedule-id");
    const box = root.querySelector("#on-call-calendar");
    const hint = root.querySelector("[data-cal-hint]");
    const picker = root.querySelector("[data-cal-picker]");
    const result = root.querySelector("[data-cal-result]");
    const memberSelect = root.querySelector("[data-override-members]");
    const HINT = hint.textContent;
    const SELECTED = ["ring-2", "ring-inset", "ring-[color:var(--theme-accent)]"];

    // Indexes into the rendered days; `to` is null until the second click.
    let sel = null;
    // A change in flight holds the month, its selection and picker until it
    // lands, so its reload is the one that runs.
    let busy = false;

    let flashTimer;
    function flash(msg, ok) {
        clearTimeout(flashTimer);
        result.textContent = msg;
        result.className = "flash-text text-xs " + (ok ? "flash-text--ok" : "flash-text--bad");
        if (ok) flashTimer = setTimeout(() => { result.textContent = ""; }, 4000);
    }

    function days() {
        return Array.from(box.querySelectorAll("[data-day]"));
    }

    function range() {
        const to = sel.to ?? sel.from;
        return [Math.min(sel.from, to), Math.max(sel.from, to)];
    }

    function paint() {
        const [lo, hi] = sel ? range() : [-1, -1];
        days().forEach((cell, i) => {
            SELECTED.forEach((c) => cell.classList.toggle(c, i >= lo && i <= hi));
        });
    }

    function clearSelection() {
        sel = null;
        picker.hidden = true;
        picker.textContent = "";
        hint.textContent = HINT;
        paint();
    }

    function reload() {
        const month = box.querySelector("[data-cal]")?.getAttribute("data-month") || "";
        window.htmx.ajax(
            "GET",
            `/web/partials/settings/on-call/${scheduleId}/calendar?month=${encodeURIComponent(month)}`,
            { source: box, target: box, swap: "innerHTML" },
        ).catch(() => { /* flashed by the htmx error events; an abort is a newer request */ });
    }

    // The API's reason for a refusal, or `fallback`.
    async function reason(res, fallback) {
        try {
            const body = await res.json();
            return body?.error?.message || fallback;
        } catch {
            return fallback;
        }
    }

    box.addEventListener("htmx:beforeRequest", (evt) => {
        if (busy) evt.preventDefault();
    });
    box.addEventListener("htmx:afterSwap", clearSelection);
    ["htmx:responseError", "htmx:sendError"].forEach((name) => {
        box.addEventListener(name, () => flash("✗ could not load the calendar, reload the page", false));
    });

    box.addEventListener("keydown", (evt) => {
        if ((evt.key === "Enter" || evt.key === " ") && evt.target.matches("[data-day][tabindex]")) {
            evt.preventDefault();
            evt.target.click();
        }
    });

    box.addEventListener("click", (evt) => {
        const remove = evt.target.closest("[data-override-remove]");
        if (remove) {
            if (busy) flash("✗ another change is still saving, try again", false);
            else removeOverride(remove);
            return;
        }
        const cell = evt.target.closest("[data-day]");
        if (busy || !cell || cell.hasAttribute("data-past")) return;
        const i = days().indexOf(cell);
        if (!sel || sel.to !== null) {
            clearSelection();
            sel = { from: i, to: null };
            hint.textContent = `Start ${cell.getAttribute("data-label")}, now click the end day.`;
            paint();
            return;
        }
        sel.to = i;
        paint();
        showPicker();
    });

    function showPicker() {
        picker.textContent = "";
        if (!memberSelect || memberSelect.options.length === 0) {
            flash("✗ no members to assign", false);
            return;
        }
        const cells = days();
        const [lo, hi] = range();
        const label = document.createElement("span");
        label.className = "text-sm text-muted";
        const first = cells[lo].getAttribute("data-label");
        const last = cells[hi].getAttribute("data-label");
        label.textContent = `Cover ${lo === hi ? first : `${first}–${last}`}:`;
        const select = memberSelect.cloneNode(true);
        select.className = "field";
        const assign = document.createElement("button");
        assign.type = "button";
        assign.className = "sticker-btn sticker-btn--primary px-3 py-1 text-sm";
        assign.textContent = "assign";
        const cancel = document.createElement("button");
        cancel.type = "button";
        cancel.className = "btn-ghost px-3 py-1 text-sm";
        cancel.textContent = "cancel";
        assign.addEventListener("click", async () => {
            if (busy) return;
            const opt = select.options[select.selectedIndex];
            busy = assign.disabled = cancel.disabled = true;
            const added = await assignOverride(
                cells[lo].getAttribute("data-start"),
                cells[hi].getAttribute("data-end"),
                opt.value,
            );
            busy = false;
            if (added) {
                clearSelection();
                flash("✓ override added", true);
                reload();
            } else {
                assign.disabled = cancel.disabled = false;
            }
        });
        cancel.addEventListener("click", clearSelection);
        picker.append(label, select, assign, cancel);
        picker.hidden = false;
    }

    async function removeOverride(button) {
        const id = button.getAttribute("data-override-remove");
        const email = button.getAttribute("data-email");
        const ok = await window.smConfirm({
            title: "Remove override?",
            body: `${email} stops covering from now; any time already covered stays.`,
            confirmLabel: "remove",
            danger: true,
        });
        if (!ok) return;
        busy = true;
        let msg;
        try {
            const res = await fetch(`/api/v1/on-call/schedules/${scheduleId}/overrides/${id}`, {
                method: "DELETE",
                headers: { "X-Requested-With": "uptimepage" },
            });
            msg = res.ok || res.status === 404 ? null : "✗ " + await reason(res, "remove failed");
        } catch {
            msg = "✗ network error";
        }
        busy = false;
        if (msg) {
            flash(msg, false);
            return;
        }
        reload();
        flash("✓ removed", true);
    }

    // Whether the override was stored; a refusal is flashed.
    async function assignOverride(startsAt, endsAt, userId) {
        try {
            const res = await fetch(`/api/v1/on-call/schedules/${scheduleId}/overrides`, {
                method: "POST",
                headers: {
                    "Content-Type": "application/json",
                    "Accept": "application/json",
                    "X-Requested-With": "uptimepage",
                },
                body: JSON.stringify({ user_id: userId, starts_at: startsAt, ends_at: endsAt }),
            });
            if (res.status === 201) return true;
            flash("✗ " + await reason(res, "add failed"), false);
        } catch {
            flash("✗ network error", false);
        }
        return false;
    }
})();
