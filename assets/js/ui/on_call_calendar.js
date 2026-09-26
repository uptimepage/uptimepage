// Overrides calendar on the on-call schedule edit page. Click a start day, then
// an end day, then pick who covers — the override is POSTed to
// /api/v1/on-call/schedules/{id}/overrides. Existing overrides render as bars;
// click one to delete it. Days run midnight to midnight in the schedule's
// timezone, the zone its handoffs are set in.
import { knowsZone, zonedInstant, zonedToday } from "./_zoned.js";

(function () {
    const root = document.querySelector("[data-overrides]");
    if (!root) return;
    const scheduleId = root.getAttribute("data-schedule-id");
    const stored = root.getAttribute("data-timezone") || "UTC";
    const tz = knowsZone(stored) ? stored : "UTC";
    const zoneNote = root.querySelector("[data-cal-zone-note]");
    if (zoneNote && tz !== stored) {
        zoneNote.textContent = `This browser does not know ${stored}, so the days below run in UTC.`;
        zoneNote.hidden = false;
    }
    const grid = root.querySelector("[data-cal-grid]");
    const title = root.querySelector("[data-cal-title]");
    const hint = root.querySelector("[data-cal-hint]");
    const picker = root.querySelector("[data-cal-picker]");
    const result = root.querySelector("[data-cal-result]");
    const memberSelect = root.querySelector("[data-override-members]");

    // Seed from the hidden list the server rendered.
    const overrides = Array.from(root.querySelectorAll("[data-override-seed] li")).map((li) => ({
        id: li.getAttribute("data-override-id"),
        userId: li.getAttribute("data-user-id"),
        email: li.getAttribute("data-email"),
        start: new Date(li.getAttribute("data-start")),
        end: new Date(li.getAttribute("data-end")),
    }));

    const today = zonedToday(tz);
    let viewYear = today.year;
    let viewMonth = today.month;
    // Selection is two clicks: selStart set on the first, selEnd on the second.
    let selStart = null;
    let selEnd = null;

    const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

    let flashTimer;
    function flash(msg, ok) {
        if (!result) return;
        clearTimeout(flashTimer);
        result.textContent = msg;
        result.className = "flash-text text-xs " + (ok ? "flash-text--ok" : "flash-text--bad");
        if (ok) flashTimer = setTimeout(() => { result.textContent = ""; }, 4000);
    }
    function setHint(msg) { if (hint) hint.textContent = msg; }

    function dayStart(y, m, d) { return zonedInstant(tz, y, m, d); }

    // An override covers a calendar day when its [start,end) window overlaps the
    // day's [00:00, next 00:00) window.
    function coversDay(ov, s, e) {
        return ov.start < e && ov.end > s;
    }

    function inSelRange(d) {
        if (selStart === null || selEnd === null) return false;
        return d >= Math.min(selStart, selEnd) && d <= Math.max(selStart, selEnd);
    }

    function clearSelection() {
        selStart = null;
        selEnd = null;
        hidePicker();
        setHint("Click a start day, then an end day, then choose who covers.");
        render();
    }

    function render() {
        title.textContent = `${MONTHS[viewMonth]} ${viewYear}`;
        grid.textContent = "";
        ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].forEach((h) => {
            const head = document.createElement("div");
            head.className = "bg-[color:var(--theme-surface-sunk)] px-1 py-1 text-center text-quiet";
            head.textContent = h;
            grid.appendChild(head);
        });
        // Plain calendar arithmetic, the same in every zone.
        const lead = (new Date(Date.UTC(viewYear, viewMonth, 1)).getUTCDay() + 6) % 7; // Monday-first column for the 1st.
        const daysInMonth = new Date(Date.UTC(viewYear, viewMonth + 1, 0)).getUTCDate();
        for (let i = 0; i < lead; i++) {
            const blank = document.createElement("div");
            blank.className = "bg-[color:var(--theme-surface)] min-h-16";
            grid.appendChild(blank);
        }
        // Each midnight once: day d runs from bounds[d - 1] to bounds[d].
        const bounds = Array.from({ length: daysInMonth + 1 }, (_, i) => dayStart(viewYear, viewMonth, i + 1));
        for (let d = 1; d <= daysInMonth; d++) {
            const cell = document.createElement("div");
            cell.setAttribute("data-day", String(d));
            const selected = inSelRange(d);
            cell.className =
                "min-h-16 cursor-pointer select-none bg-[color:var(--theme-surface)] p-1 align-top " +
                (selected ? "ring-2 ring-inset ring-[color:var(--theme-accent)]" : "");
            const num = document.createElement("div");
            num.className = "text-right text-quiet";
            num.textContent = String(d);
            cell.appendChild(num);
            overrides.filter((ov) => coversDay(ov, bounds[d - 1], bounds[d])).forEach((ov) => {
                const bar = document.createElement("button");
                bar.type = "button";
                bar.setAttribute("data-override-bar", ov.id);
                bar.className = "mt-0.5 block w-full truncate rounded bg-[color:var(--theme-accent-soft)] px-1 text-left text-[color:var(--theme-accent)]";
                bar.textContent = ov.email;
                bar.title = "Remove this override";
                cell.appendChild(bar);
            });
            grid.appendChild(cell);
        }
    }

    function dayFrom(evt) {
        const cell = evt.target.closest("[data-day]");
        return cell ? parseInt(cell.getAttribute("data-day"), 10) : null;
    }

    grid.addEventListener("click", async (evt) => {
        // Deleting an existing override takes precedence over day selection.
        const bar = evt.target.closest("[data-override-bar]");
        if (bar) { await removeOverride(bar.getAttribute("data-override-bar")); return; }

        const d = dayFrom(evt);
        if (d === null) return;
        if (selStart === null) {
            // First click: range start.
            selStart = d;
            selEnd = d;
            setHint(`Start ${MONTHS[viewMonth]} ${d} — now click the end day.`);
            render();
        } else if (selEnd !== null && picker.classList.contains("hidden") === false) {
            // A click while the picker is open starts a fresh selection.
            clearSelection();
            selStart = d;
            selEnd = d;
            setHint(`Start ${MONTHS[viewMonth]} ${d} — now click the end day.`);
            render();
        } else {
            // Second click: range end → open the assignee picker.
            selEnd = d;
            render();
            showPicker();
        }
    });

    function hidePicker() {
        picker.classList.add("hidden");
        picker.textContent = "";
    }

    function showPicker() {
        picker.textContent = "";
        if (!memberSelect || memberSelect.options.length === 0) {
            flash("✗ no members to assign", false);
            return;
        }
        const lo = Math.min(selStart, selEnd);
        const hi = Math.max(selStart, selEnd);
        const label = document.createElement("span");
        label.className = "text-sm text-muted";
        label.textContent = `Cover ${MONTHS[viewMonth]} ${lo}–${hi}:`;
        const select = memberSelect.cloneNode(true);
        select.classList.remove("hidden");
        select.className = "field";
        const assign = document.createElement("button");
        assign.type = "button";
        assign.className = "sticker-btn sticker-btn--primary px-3 py-1 text-sm";
        assign.textContent = "Assign";
        const cancel = document.createElement("button");
        cancel.type = "button";
        cancel.className = "btn-ghost px-3 py-1 text-sm";
        cancel.textContent = "Cancel";
        assign.addEventListener("click", () => {
            const opt = select.options[select.selectedIndex];
            assignOverride(lo, hi, opt.value, opt.dataset.email || opt.textContent);
        });
        cancel.addEventListener("click", clearSelection);
        picker.append(label, select, assign, cancel);
        picker.classList.remove("hidden");
    }

    async function removeOverride(id) {
        const ov = overrides.find((o) => o.id === id);
        const ok = await window.smConfirm({
            title: "Remove override?",
            body: ov ? `Remove ${ov.email} from this period?` : "Remove this override?",
            confirmLabel: "remove",
            danger: true,
        });
        if (!ok) return;
        try {
            const res = await fetch(`/api/v1/on-call/schedules/${scheduleId}/overrides/${id}`, {
                method: "DELETE",
                headers: { "X-Requested-With": "uptimepage" },
            });
            if (res.ok || res.status === 404) {
                const i = overrides.findIndex((o) => o.id === id);
                if (i >= 0) overrides.splice(i, 1);
                render();
                flash("✓ removed", true);
            } else {
                flash("✗ remove failed", false);
            }
        } catch {
            flash("✗ network error", false);
        }
    }

    async function assignOverride(loDay, hiDay, userId, email) {
        const starts = dayStart(viewYear, viewMonth, loDay);
        const ends = dayStart(viewYear, viewMonth, hiDay + 1); // end is exclusive
        // Guard the common solo/small-team mistake: the same person already
        // covering an overlapping window. The rotation resolver would dedupe a
        // duplicate, but a redundant override is just clutter — reject it.
        const clash = overrides.some(
            (o) => o.userId === userId && o.start < ends && o.end > starts,
        );
        if (clash) {
            flash(`✗ ${email} already covers part of that range`, false);
            return;
        }
        try {
            const res = await fetch(`/api/v1/on-call/schedules/${scheduleId}/overrides`, {
                method: "POST",
                headers: {
                    "Content-Type": "application/json",
                    "Accept": "application/json",
                    "X-Requested-With": "uptimepage",
                },
                body: JSON.stringify({
                    user_id: userId,
                    starts_at: starts.toISOString(),
                    ends_at: ends.toISOString(),
                }),
            });
            if (res.status === 201) {
                const body = await res.json();
                overrides.push({
                    id: body.id,
                    userId: body.user_id,
                    email,
                    start: new Date(body.starts_at),
                    end: new Date(body.ends_at),
                });
                clearSelection();
                flash("✓ override added", true);
            } else {
                let msg = "add failed";
                try { const b = await res.json(); if (b && b.error && b.error.message) msg = b.error.message; } catch { /* */ }
                flash("✗ " + msg, false);
            }
        } catch {
            flash("✗ network error", false);
        }
    }

    root.querySelector("[data-cal-prev]").addEventListener("click", () => {
        viewMonth -= 1;
        if (viewMonth < 0) { viewMonth = 11; viewYear -= 1; }
        clearSelection();
    });
    root.querySelector("[data-cal-next]").addEventListener("click", () => {
        viewMonth += 1;
        if (viewMonth > 11) { viewMonth = 0; viewYear += 1; }
        clearSelection();
    });

    render();
})();
