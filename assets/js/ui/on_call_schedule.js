// On-call schedule builder. Manages the dynamic layer rows and their ordered
// participant lists, and serialises them into a NewOnCallSchedule body for
// POST/PATCH /api/v1/on-call/schedules. Handoff times are typed as wall-clock
// time in the schedule's timezone and sent as the instant that names there.
// Shares the error-banner layer from api_form.js (loaded before this).
import { MAX_SECS, formatDuration, parseSpan } from "./_duration.js";
import { knowsZone, parseZoned } from "./_zoned.js";

(function () {
    const form = document.getElementById("schedule-form");
    if (!form) return;
    const layers = document.getElementById("layers");
    const layerTmpl = document.getElementById("layer-template");
    const participantTmpl = document.getElementById("participant-template");
    const addBtn = document.getElementById("add-layer");
    const tzInput = form.querySelector("[name=timezone]");

    // Daily and weekly take a count of periods; custom takes a duration.
    const PERIOD_SECS = { daily: 86400, weekly: 604800 };
    const UNIT_LABEL = { daily: "day(s)", weekly: "week(s)", custom: "e.g. 12h" };

    function lengthSecs(type, raw) {
        if (type === "custom") return parseSpan(raw);
        const t = String(raw ?? "").trim();
        const secs = /^\d+$/.test(t) ? parseInt(t, 10) * PERIOD_SECS[type] : null;
        return secs !== null && secs <= MAX_SECS ? secs : null;
    }

    function lengthText(type, secs) {
        if (secs == null || secs <= 0) return "";
        if (type === "custom") return formatDuration(secs);
        return secs % PERIOD_SECS[type] === 0 ? String(secs / PERIOD_SECS[type]) : "";
    }

    function zone() {
        return (tzInput.value || "").trim() || "UTC";
    }

    function syncZoneLabels() {
        const tz = zone();
        layers.querySelectorAll("[data-handoff-zone]").forEach((el) => {
            el.textContent = `(${tz})`;
        });
    }

    // The length is converted from what was last typed, not from the previous
    // type's rendering, so passing through a type it does not fit (arrowing
    // Daily → Weekly → Custom) does not lose it.
    function syncUnit(row) {
        const select = row.querySelector("[data-rotation-type]");
        const input = row.querySelector("[data-rotation-length]");
        if (input.dataset.secs === undefined) {
            input.dataset.secs = lengthSecs(select.value, input.value) ?? "";
        } else {
            input.value = lengthText(select.value, Number(input.dataset.secs) || null);
        }
        row.querySelector("[data-rotation-unit]").textContent = UNIT_LABEL[select.value] || "";
    }

    function syncParticipants(row) {
        const items = Array.from(row.querySelectorAll("[data-participant-list] > [data-participant]"));
        const taken = new Set(items.map((li) => li.dataset.participant));
        items.forEach((li, i) => {
            li.querySelector("[data-participant-pos]").textContent = `${i + 1}.`;
            li.querySelector("[data-move='-1']").disabled = i === 0;
            li.querySelector("[data-move='1']").disabled = i === items.length - 1;
        });
        for (const opt of row.querySelector("[data-add-participant]").options) {
            if (opt.value) opt.hidden = opt.disabled = taken.has(opt.value);
        }
    }

    function participantFrom(opt) {
        const li = participantTmpl.content.firstElementChild.cloneNode(true);
        const email = opt.dataset.email;
        li.dataset.participant = opt.value;
        li.querySelector("[data-participant-email]").textContent = email;
        li.querySelector("[data-participant-unreachable]").hidden = !opt.hasAttribute("data-unreachable");
        li.querySelector("[data-move='-1']").setAttribute("aria-label", `Move ${email} earlier`);
        li.querySelector("[data-move='1']").setAttribute("aria-label", `Move ${email} later`);
        li.querySelector("[data-remove-participant]").setAttribute("aria-label", `Remove ${email}`);
        return li;
    }

    function initRow(row) {
        syncUnit(row);
        syncParticipants(row);
        row.querySelector("[data-rotation-type]").addEventListener("change", () => syncUnit(row));
        const length = row.querySelector("[data-rotation-length]");
        length.addEventListener("input", () => {
            length.dataset.secs = lengthSecs(row.querySelector("[data-rotation-type]").value, length.value) ?? "";
        });
        // A button, not the select's `change`: some platforms fire `change` on
        // each arrow key, which would add every member passed over.
        const add = row.querySelector("[data-add-participant]");
        row.querySelector("[data-add-participant-go]").addEventListener("click", () => {
            const opt = add.selectedOptions[0];
            if (!opt || !opt.value) return;
            row.querySelector("[data-participant-list]").appendChild(participantFrom(opt));
            add.value = "";
            syncParticipants(row);
        });
    }

    function renumber() {
        layers.querySelectorAll("[data-layer-row]").forEach((row, i) => {
            const n = row.querySelector("[data-layer-num]");
            if (n) n.textContent = String(i + 1);
        });
    }

    // Only a zone the server lists, so the default can never be refused.
    const browserZone = Intl.DateTimeFormat().resolvedOptions().timeZone;
    const offered = document.querySelector(`#tz-list option[value="${CSS.escape(browserZone || "")}"]`);
    if (form.dataset.mode === "create" && tzInput.value === "UTC" && offered) {
        tzInput.value = browserZone;
    }
    tzInput.addEventListener("input", syncZoneLabels);
    layers.querySelectorAll("[data-layer-row]").forEach(initRow);
    syncZoneLabels();

    addBtn.addEventListener("click", () => {
        layers.appendChild(layerTmpl.content.firstElementChild.cloneNode(true));
        initRow(layers.lastElementChild);
        renumber();
        syncZoneLabels();
    });

    layers.addEventListener("click", (evt) => {
        const move = evt.target.closest("[data-move]");
        if (move) {
            const li = move.closest("[data-participant]");
            const sibling = move.dataset.move === "-1" ? li.previousElementSibling : li.nextElementSibling;
            if (sibling) {
                if (move.dataset.move === "-1") sibling.before(li);
                else sibling.after(li);
            }
            syncParticipants(li.closest("[data-layer-row]"));
            // At the end of the list this button is now disabled; keep the
            // keyboard on the row through its other move button.
            (move.disabled ? li.querySelector(`[data-move]:not([data-move="${move.dataset.move}"])`) : move).focus();
            return;
        }
        const drop = evt.target.closest("[data-remove-participant]");
        if (drop) {
            const row = drop.closest("[data-layer-row]");
            drop.closest("[data-participant]").remove();
            syncParticipants(row);
            return;
        }
        const rm = evt.target.closest("[data-remove-layer]");
        if (!rm) return;
        if (layers.querySelectorAll("[data-layer-row]").length <= 1) {
            renderClientError("A schedule needs at least one layer.");
            return;
        }
        rm.closest("[data-layer-row]").remove();
        renumber();
    });

    const submitBtn = form.querySelector("button[type=submit]");
    form.addEventListener("submit", async (evt) => {
        evt.preventDefault();
        if (submitBtn.disabled) return;
        clearErrors();
        const built = buildBody();
        if (built.error) { renderClientError(built.error); return; }
        const label = submitBtn.textContent;
        submitBtn.disabled = true;
        submitBtn.textContent = "Saving…";
        let navigating = false;
        try {
            let res;
            try {
                res = await fetch(form.dataset.action, {
                    method: form.dataset.method,
                    headers: {
                        "Content-Type": "application/json",
                        "Accept": "application/json",
                        "X-Requested-With": "uptimepage",
                    },
                    body: JSON.stringify(built.payload),
                });
            } catch (err) {
                renderClientError(`Network error: ${err.message || err}`);
                return;
            }
            if (res.ok) { navigating = true; window.location = "/settings/on-call"; return; }
            let body;
            try { body = await res.json(); }
            catch { renderClientError(`Request failed (${res.status})`); return; }
            renderApiError(body, res.status);
        } finally {
            if (!navigating) { submitBtn.disabled = false; submitBtn.textContent = label; }
        }
    });

    function buildBody() {
        const name = (form.querySelector("[name=name]").value || "").trim();
        if (!name) return { error: "Name is required." };
        const timezone = zone();
        const zoneKept = tzInput.value === tzInput.defaultValue;
        const rows = Array.from(layers.querySelectorAll("[data-layer-row]"));
        const out = [];
        for (let i = 0; i < rows.length; i++) {
            const row = rows[i];
            const type = row.querySelector("[data-rotation-type]").value;
            const secs = lengthSecs(type, row.querySelector("[data-rotation-length]").value);
            if (!(secs >= 1)) {
                const want = type === "custom" ? "a duration with a unit, such as 12h or 90m" : `a whole number of ${type === "weekly" ? "weeks" : "days"}`;
                return { error: `Layer ${i + 1}: write the rotation length as ${want}.` };
            }
            if (type === "custom" && secs < 3600) {
                return { error: `Layer ${i + 1}: a custom rotation length must be at least 1h.` };
            }
            const input = row.querySelector("[data-handoff]");
            if (!input.value) return { error: `Layer ${i + 1}: pick a first handoff time.` };
            // Untouched, the stored instant goes back as it came.
            let handoff = zoneKept && input.value === input.defaultValue && input.dataset.iso
                ? new Date(input.dataset.iso)
                : null;
            if (!handoff) {
                if (!knowsZone(timezone)) {
                    return { error: `This browser cannot read times in ${timezone}. Check the timezone name.` };
                }
                handoff = parseZoned(input.value, timezone);
            }
            if (!handoff || isNaN(handoff.getTime())) return { error: `Layer ${i + 1}: invalid handoff time.` };
            const participants = Array.from(row.querySelectorAll("[data-participant-list] > [data-participant]"))
                .map((li) => ({ user_id: li.dataset.participant }));
            if (participants.length === 0) {
                return { error: `Layer ${i + 1} needs at least one participant.` };
            }
            const layerName = (row.querySelector("[data-layer-name]").value || "").trim();
            out.push({
                name: layerName || null,
                rotation_type: type,
                rotation_length_secs: secs,
                handoff_at: handoff.toISOString(),
                layer_order: i,
                participants,
            });
        }
        return { payload: { name, timezone, layers: out } };
    }

    function clearErrors() { window.smClearFormErrors(document.getElementById("form-errors")); }
    function renderClientError(msg) { window.smRenderClientError(document.getElementById("form-errors"), msg); }
    function renderApiError(json, status) { window.smRenderApiError(document.getElementById("form-errors"), json, status); }
})();
