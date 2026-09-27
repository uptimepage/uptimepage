// On-call schedule builder. Manages the dynamic layer rows, their hours and
// ordered participant lists, and serialises them into a NewOnCallSchedule
// body for POST/PATCH /api/v1/on-call/schedules. Handoff times are typed as
// wall-clock time in the schedule's timezone and sent as the instant that
// names there. Shares the error-banner layer from api_form.js (loaded before
// this).
import { MAX_SECS, formatDuration, parseSpan } from "./_duration.js";
import { knowsZone, parseZoned, todayIn } from "./_zoned.js";

(function () {
    const form = document.getElementById("schedule-form");
    if (!form) return;
    const layers = document.getElementById("layers");
    const layerTmpl = document.getElementById("layer-template");
    const participantTmpl = document.getElementById("participant-template");
    const windowTmpl = document.getElementById("window-template");
    // Every member, to rebuild a picker from as people join and leave its
    // rotation.
    const memberOptions = Array.from(layerTmpl.content.querySelector("[data-add-participant]").options);
    const addBtn = document.getElementById("add-layer");
    const tzInput = form.querySelector("[name=timezone]");
    const rail = form.querySelector("[data-layer-rail]");

    // Daily and weekly take a count of periods; custom takes a duration.
    const PERIOD_SECS = { daily: 86400, weekly: 604800 };
    const UNIT_LABEL = { daily: "days", weekly: "weeks", custom: "" };
    const DAYS = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

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
            el.textContent = `# ${tz} time`;
        });
    }

    function rows() {
        return Array.from(layers.querySelectorAll("[data-layer-row]"));
    }

    function rotationType(row) {
        return row.querySelector("[data-rotation-type]:checked")?.value || "daily";
    }

    // The length is converted from what was last typed, not from the previous
    // type's rendering, so passing through a type it does not fit (arrowing
    // daily → weekly → custom) does not lose it.
    function syncUnit(row) {
        const type = rotationType(row);
        const input = row.querySelector("[data-rotation-length]");
        if (input.dataset.secs === undefined) {
            input.dataset.secs = lengthSecs(type, input.value) ?? "";
        } else {
            input.value = lengthText(type, Number(input.dataset.secs) || null);
        }
        input.placeholder = type === "custom" ? "12h" : "1";
        row.querySelector("[data-rotation-unit]").textContent = UNIT_LABEL[type];
    }

    // `mon`, `tue`… as runs: Mon–Fri, Sat, Sun.
    function daysText(days) {
        const at = days.map((d) => DAYS.indexOf(d)).sort((a, b) => a - b);
        if (at.length === 7) return "every day";
        const runs = [];
        for (const i of at) {
            const last = runs[runs.length - 1];
            if (last && last[1] === i - 1) last[1] = i;
            else runs.push([i, i]);
        }
        const name = (i) => DAYS[i][0].toUpperCase() + DAYS[i].slice(1);
        return runs
            .map(([a, b]) => (a === b ? name(a) : b === a + 1 ? `${name(a)}, ${name(b)}` : `${name(a)}–${name(b)}`))
            .join(", ");
    }

    function hoursText(row) {
        const windows = Array.from(row.querySelectorAll("[data-window-list] > [data-window]"));
        if (windows.length === 0) return "at all hours";
        return windows.map((li) => {
            const days = Array.from(li.querySelectorAll("[data-window-day]:checked")).map((c) => c.value);
            const from = li.querySelector("[data-window-from]").value;
            const to = li.querySelector("[data-window-to]").value;
            return `${days.length ? daysText(days) : "no days"} ${from}–${to}`;
        }).join("; ");
    }

    function rotationText(row) {
        const type = rotationType(row);
        const secs = lengthSecs(type, row.querySelector("[data-rotation-length]").value);
        if (!secs) return type;
        if (type === "custom") return `every ${formatDuration(secs)}`;
        const n = secs / PERIOD_SECS[type];
        return n === 1 ? type : `every ${n} ${UNIT_LABEL[type]}`;
    }

    const DAY_MINUTES = 24 * 60;
    const WEEK_MINUTES = 7 * DAY_MINUTES;

    function minutesOf(hhmm) {
        const m = /^(\d{2}):(\d{2})/.exec(hhmm || "");
        return m ? +m[1] * 60 + +m[2] : null;
    }

    // The minutes of the week, Monday 00:00 first, a layer is on call on the
    // wall clock, as the API counts them; every one when it has no hours, and
    // null while one of its hours still lacks a day or a time.
    function weekMinutes(row) {
        const windows = Array.from(row.querySelectorAll("[data-window-list] > [data-window]"));
        const on = new Uint8Array(WEEK_MINUTES).fill(windows.length === 0 ? 1 : 0);
        for (const li of windows) {
            const from = minutesOf(li.querySelector("[data-window-from]").value);
            const to = minutesOf(li.querySelector("[data-window-to]").value);
            if (from === null || to === null || !li.querySelector("[data-window-day]:checked")) return null;
            // A whole day when the ends meet, past midnight when `to` comes first.
            const len = ((to + DAY_MINUTES - from - 1) % DAY_MINUTES) + 1;
            for (const day of li.querySelectorAll("[data-window-day]:checked")) {
                const start = DAYS.indexOf(day.value) * DAY_MINUTES + from;
                for (let m = start; m < start + len; m++) on[m % WEEK_MINUTES] = 1;
            }
        }
        return on;
    }

    // Why each layer would never page, or null: the layers before it are on
    // call at all of its hours. The API refuses such a schedule.
    function shadows() {
        const covered = new Uint8Array(WEEK_MINUTES);
        const byHours = new Uint8Array(WEEK_MINUTES);
        let allHours = null;
        return rows().map((row, i) => {
            const mine = weekMinutes(row);
            if (!mine) return null;
            if (mine.every((m, k) => !m || covered[k])) {
                if (allHours !== null && !mine.every((m, k) => !m || byHours[k])) {
                    return `# never pages: layer ${allHours + 1} is on call at all hours; add hours to layer ${allHours + 1}, or remove this layer`;
                }
                return covered.every(Boolean)
                    ? "# never pages: the layers before it are on call at all hours; remove it"
                    : "# never pages: the layers before it are on call at all its hours; change its hours, or remove it";
            }
            const hasHours = row.querySelector("[data-window]") !== null;
            if (!hasHours) allHours = i;
            mine.forEach((m, k) => {
                if (!m) return;
                covered[k] = 1;
                if (hasHours) byHours[k] = 1;
            });
            return null;
        });
    }

    // The rail restates the layers in the order they are asked, and a layer
    // that could never page says why before the save does.
    function syncRail() {
        const why = shadows();
        rows().forEach((row, i) => {
            const note = row.querySelector("[data-never-pages]");
            if (note.textContent !== (why[i] || "")) note.textContent = why[i] || "";
            note.hidden = !why[i];
        });
        rail.replaceChildren(...rows().map((row, i) => {
            const card = document.createElement("a");
            card.className = "check-type-card";
            card.href = `#${row.id}`;
            const label = row.querySelector("[data-layer-name]").value.trim();
            const name = document.createElement("span");
            name.className = "check-type-card__name";
            name.textContent = label ? `${i + 1} · ${label}` : `layer ${i + 1}`;
            const desc = document.createElement("span");
            desc.className = "check-type-card__desc";
            desc.textContent = why[i] ? "never pages" : `${hoursText(row)} · ${rotationText(row)}`;
            card.append(name, desc);
            return card;
        }));
    }

    function participantsOf(row) {
        return Array.from(row.querySelectorAll("[data-participant-list] > [data-participant]"));
    }

    function syncParticipants(row) {
        const items = participantsOf(row);
        items.forEach((li, i) => {
            li.querySelector("[data-participant-pos]").textContent = `${i + 1}.`;
            li.querySelector("[data-move='-1']").disabled = i === 0;
            li.querySelector("[data-move='1']").disabled = i === items.length - 1;
        });
    }

    // The picker offers only members not already in the rotation.
    function syncPicker(row) {
        const taken = new Set(participantsOf(row).map((li) => li.dataset.participant));
        row.querySelector("[data-add-participant]").replaceChildren(
            ...memberOptions.filter((o) => !taken.has(o.value)).map((o) => o.cloneNode(true)),
        );
    }

    function syncWindows(row) {
        row.querySelector("[data-windows-none]").hidden = row.querySelector("[data-window]") !== null;
    }

    // Next Monday, 09:00 in the schedule's zone, as a datetime-local value.
    function nextMonday() {
        const now = new Date();
        const [y, m, d] = knowsZone(zone()) ? todayIn(zone()) : [now.getFullYear(), now.getMonth(), now.getDate()];
        const date = new Date(Date.UTC(y, m, d));
        date.setUTCDate(date.getUTCDate() + ((8 - date.getUTCDay()) % 7 || 7));
        return `${date.toISOString().slice(0, 10)}T09:00`;
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

    let rowSeq = 0;
    function initRow(row) {
        // One radio group per layer, so arrow keys stay within it.
        const group = `rotation-${++rowSeq}`;
        row.querySelectorAll("[data-rotation-type]").forEach((r) => { r.name = group; });
        syncUnit(row);
        syncParticipants(row);
        syncPicker(row);
        syncWindows(row);
        const handoff = row.querySelector("[data-handoff]");
        if (!handoff.value) handoff.value = nextMonday();
        row.querySelectorAll("[data-rotation-type]").forEach((r) => r.addEventListener("change", () => syncUnit(row)));
        const length = row.querySelector("[data-rotation-length]");
        length.addEventListener("input", () => {
            length.dataset.secs = lengthSecs(rotationType(row), length.value) ?? "";
        });
        // The picker is a combobox, which reports a pick only once it is
        // made, never while arrowing through the list.
        const add = row.querySelector("[data-add-participant]");
        add.addEventListener("change", () => {
            const opt = add.selectedOptions[0];
            if (!opt || !opt.value) return;
            row.querySelector("[data-participant-list]").appendChild(participantFrom(opt));
            syncParticipants(row);
            syncPicker(row);
        });
    }

    function renumber() {
        const all = rows();
        all.forEach((row, i) => {
            row.id = `layer-${i + 1}`;
            row.querySelector("[data-layer-num]").textContent = String(i + 1);
            const up = row.querySelector("[data-move-layer='-1']");
            const down = row.querySelector("[data-move-layer='1']");
            up.disabled = i === 0;
            down.disabled = i === all.length - 1;
            up.setAttribute("aria-label", `Move layer ${i + 1} earlier`);
            down.setAttribute("aria-label", `Move layer ${i + 1} later`);
        });
        syncRail();
    }

    // Layers are asked in the order listed. One is dragged by its handle,
    // drawn as the whole card, and one not dropped on the list goes back
    // where it was.
    let dragging = null;
    let home = null;
    let dropped = false;
    const elementOf = (node) => (node instanceof Element ? node : node.parentElement);
    layers.addEventListener("dragstart", (evt) => {
        const handle = elementOf(evt.target)?.closest("[data-layer-drag]");
        if (!handle) return;
        dragging = handle.closest("[data-layer-row]");
        home = dragging.nextElementSibling;
        dropped = false;
        const box = dragging.getBoundingClientRect();
        evt.dataTransfer.setDragImage(dragging, evt.clientX - box.left, evt.clientY - box.top);
        evt.dataTransfer.effectAllowed = "move";
        // Firefox starts no drag that carries no data.
        evt.dataTransfer.setData("text/plain", "");
        // After the drag image is taken, so it keeps full strength.
        const row = dragging;
        requestAnimationFrame(() => row.classList.add("opacity-50"));
    });
    layers.addEventListener("dragover", (evt) => {
        if (!dragging) return;
        evt.preventDefault();
        evt.dataTransfer.dropEffect = "move";
        const over = elementOf(evt.target)?.closest("[data-layer-row]");
        if (!over || over === dragging) return;
        const box = over.getBoundingClientRect();
        const before = evt.clientY > box.top + box.height / 2 ? over.nextElementSibling : over;
        if (before !== dragging && before !== dragging.nextElementSibling) {
            layers.insertBefore(dragging, before);
        }
    });
    layers.addEventListener("drop", (evt) => {
        if (!dragging) return;
        evt.preventDefault();
        dropped = true;
    });
    layers.addEventListener("dragend", () => {
        if (!dragging) return;
        if (!dropped) layers.insertBefore(dragging, home);
        dragging.classList.remove("opacity-50");
        dragging = home = null;
        renumber();
    });

    // Only a zone the server lists, so the default can never be refused.
    const storedZone = Array.from(tzInput.options).find((o) => o.defaultSelected)?.value ?? tzInput.value;
    const browserZone = Intl.DateTimeFormat().resolvedOptions().timeZone;
    const offered = Array.from(tzInput.options).some((o) => o.value === browserZone);
    if (form.dataset.mode === "create" && storedZone === "UTC" && offered) {
        tzInput.value = browserZone;
        tzInput.dispatchEvent(new Event("change", { bubbles: true }));
    }
    tzInput.addEventListener("change", syncZoneLabels);
    rows().forEach(initRow);
    renumber();
    syncZoneLabels();
    layers.addEventListener("input", syncRail);
    layers.addEventListener("change", syncRail);
    // Not from an open picker, whose own Enter commits the choice first, nor
    // from the calendar, which saves overrides on its own.
    form.addEventListener("keydown", (evt) => {
        const picking = evt.target.closest(".sm-combobox")?.querySelector('[aria-expanded="true"]');
        if ((evt.metaKey || evt.ctrlKey) && evt.key === "Enter" && !picking && !evt.target.closest("[data-overrides]")) {
            evt.preventDefault();
            form.requestSubmit();
        }
    });

    addBtn.addEventListener("click", () => {
        layers.appendChild(layerTmpl.content.firstElementChild.cloneNode(true));
        initRow(layers.lastElementChild);
        window.smInitComboboxes?.();
        renumber();
        syncZoneLabels();
    });

    layers.addEventListener("click", (evt) => {
        const moveLayer = evt.target.closest("[data-move-layer]");
        if (moveLayer) {
            const way = moveLayer.dataset.moveLayer;
            const row = moveLayer.closest("[data-layer-row]");
            if (way === "-1") row.previousElementSibling?.before(row);
            else row.nextElementSibling?.after(row);
            renumber();
            // At either end this button is now disabled; keep the keyboard
            // on the layer through its other move button.
            (moveLayer.disabled ? row.querySelector(`[data-move-layer]:not([data-move-layer="${way}"])`) : moveLayer).focus();
            return;
        }
        const addWindow = evt.target.closest("[data-add-window]");
        if (addWindow) {
            const row = addWindow.closest("[data-layer-row]");
            row.querySelector("[data-window-list]").appendChild(windowTmpl.content.firstElementChild.cloneNode(true));
            syncWindows(row);
            syncRail();
            return;
        }
        const dropWindow = evt.target.closest("[data-remove-window]");
        if (dropWindow) {
            const row = dropWindow.closest("[data-layer-row]");
            dropWindow.closest("[data-window]").remove();
            syncWindows(row);
            syncRail();
            return;
        }
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
            syncPicker(row);
            return;
        }
        const rm = evt.target.closest("[data-remove-layer]");
        if (!rm) return;
        if (rows().length <= 1) {
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
        submitBtn.textContent = "saving…";
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
        const zoneKept = zone() === storedZone;
        const all = rows();
        const out = [];
        for (let i = 0; i < all.length; i++) {
            const row = all[i];
            const type = rotationType(row);
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
            const windows = [];
            for (const li of row.querySelectorAll("[data-window-list] > [data-window]")) {
                const days = Array.from(li.querySelectorAll("[data-window-day]:checked")).map((c) => c.value);
                const from = li.querySelector("[data-window-from]").value;
                const to = li.querySelector("[data-window-to]").value;
                if (days.length === 0) return { error: `Layer ${i + 1}: pick at least one day for each of its hours.` };
                if (!from || !to) return { error: `Layer ${i + 1}: give each of its hours a start and an end time.` };
                windows.push({ days, from, to });
            }
            const layerName = (row.querySelector("[data-layer-name]").value || "").trim();
            out.push({
                name: layerName || null,
                rotation_type: type,
                rotation_length_secs: secs,
                handoff_at: handoff.toISOString(),
                layer_order: i,
                windows,
                participants,
            });
        }
        return { payload: { name, timezone, layers: out } };
    }

    function clearErrors() { window.smClearFormErrors(document.getElementById("form-errors")); }
    function renderClientError(msg) { window.smRenderClientError(document.getElementById("form-errors"), msg); }
    function renderApiError(json, status) { window.smRenderApiError(document.getElementById("form-errors"), json, status); }
})();
