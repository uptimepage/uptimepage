// Maintenance window form: local-time pickers that submit UTC instants, a
// length shortcut, a searchable monitor list, and a fetch submit against the
// JSON API. Errors go through the shared api_form.js banner.
(() => {
    const form = document.getElementById("maintenance-form");
    if (!form) return;

    const MAX_DAYS = 30;
    const MINUTE = 60000;
    const banner = document.getElementById("form-errors");
    const startInput = form.elements.starts_at;
    const endInput = form.elements.ends_at;
    const submitBtn = form.querySelector("button[type=submit]");
    const isEdit = form.dataset.mode === "edit";
    const presets = [...form.querySelectorAll("[data-minutes]")];
    const rows = [...form.querySelectorAll("[data-monitor-row]")];
    const search = form.querySelector("[data-monitor-search]");
    const noMatch = form.querySelector("[data-no-match]");
    const countEl = form.querySelector("[data-selected-count]");
    const unpublishedNote = form.querySelector("[data-unpublished-note]");

    const pad = (n) => String(n).padStart(2, "0");
    const toInput = (d) =>
        `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
    // A datetime-local value has no offset, so Date reads it as local time.
    const fromInput = (el) => (el.value ? new Date(el.value) : null);
    const validDate = (d) => d instanceof Date && !Number.isNaN(d.getTime());

    function nextQuarterHour() {
        const d = new Date();
        d.setMinutes(Math.ceil((d.getMinutes() + 1) / 15) * 15, 0, 0);
        return d;
    }

    function seed(el, fallback) {
        const utc = el.dataset.utc;
        el.value = toInput(utc ? new Date(utc) : fallback);
    }

    function syncPresets() {
        const start = fromInput(startInput);
        const end = fromInput(endInput);
        const minutes = validDate(start) && validDate(end) ? Math.round((end - start) / MINUTE) : null;
        presets.forEach((b) =>
            b.classList.toggle("sm-rail__seg--active", Number(b.dataset.minutes) === minutes));
    }

    const first = nextQuarterHour();
    seed(startInput, first);
    seed(endInput, new Date(first.getTime() + 60 * MINUTE));
    form.querySelector("[data-tz-name]").textContent =
        Intl.DateTimeFormat().resolvedOptions().timeZone;
    syncPresets();

    presets.forEach((b) => b.addEventListener("click", () => {
        let start = fromInput(startInput);
        if (!validDate(start)) {
            start = nextQuarterHour();
            startInput.value = toInput(start);
        }
        endInput.value = toInput(new Date(start.getTime() + Number(b.dataset.minutes) * MINUTE));
        syncPresets();
    }));
    startInput.addEventListener("input", syncPresets);
    endInput.addEventListener("input", syncPresets);

    function refreshList() {
        const q = search ? search.value.trim().toLowerCase() : "";
        let shown = 0;
        rows.forEach((r) => {
            const hit = !q || r.dataset.name.includes(q);
            r.classList.toggle("hidden", !hit);
            if (hit) shown += 1;
        });
        if (noMatch) noMatch.classList.toggle("hidden", shown > 0);
    }

    function refreshSelection() {
        const picked = rows.filter((r) => r.querySelector("input").checked);
        if (countEl) countEl.textContent = `${picked.length} selected`;
        if (!unpublishedNote) return;
        const off = picked.filter((r) => r.dataset.published !== "true").length;
        unpublishedNote.textContent = off
            ? `# ${off} of the selected monitors ${off === 1 ? "is" : "are"} not on a status page, so the window does not show publicly for ${off === 1 ? "it" : "them"}`
            : "";
        unpublishedNote.classList.toggle("hidden", off === 0);
    }

    if (search) {
        search.addEventListener("input", refreshList);
        search.addEventListener("keydown", (evt) => {
            if (evt.key === "Enter") evt.preventDefault();
        });
    }
    rows.forEach((r) => r.querySelector("input").addEventListener("change", refreshSelection));
    refreshSelection();

    const pickedIds = () =>
        rows
            .map((r) => r.querySelector("input"))
            .filter((box) => box.checked)
            .map((box) => box.value);
    const loaded = {
        title: form.elements.title.value.trim(),
        description: form.elements.description.value.trim(),
        start: startInput.value,
        end: endInput.value,
        components: [...pickedIds()].sort().join(","),
        suppress: form.elements.suppress_alerts.checked,
    };

    function flag(field) {
        const el = form.querySelector(`[name="${field}"]`);
        if (!el) return;
        el.setAttribute("aria-invalid", "true");
        el.focus();
    }

    function timeError(endChanged, start, end) {
        if (!validDate(start)) return { error: "Pick a start time.", field: "starts_at" };
        if (!validDate(end)) return { error: "Pick an end time.", field: "ends_at" };
        if (end <= start) return { error: "The end must be after the start.", field: "ends_at" };
        if (end - start > MAX_DAYS * 24 * 60 * MINUTE) {
            return { error: `A window can last at most ${MAX_DAYS} days.`, field: "ends_at" };
        }
        if (endChanged && end <= new Date()) {
            return { error: "The end is in the past. A window must end in the future.", field: "ends_at" };
        }
        return null;
    }

    function buildBody() {
        const title = form.elements.title.value.trim();
        if (!title) return { error: "Give the window a title.", field: "title" };
        const description = form.elements.description.value.trim();
        const components = pickedIds();
        const suppress = form.elements.suppress_alerts.checked;
        // An edit sends only what changed, so the audit trail lists real edits
        // and an untouched time is not rewritten at minute precision. A create
        // sends everything it has.
        const changed = {
            title: !isEdit || title !== loaded.title,
            description: isEdit ? description !== loaded.description : description !== "",
            start: !isEdit || startInput.value !== loaded.start,
            end: !isEdit || endInput.value !== loaded.end,
            components: !isEdit || [...components].sort().join(",") !== loaded.components,
            suppress: !isEdit || suppress !== loaded.suppress,
        };
        const start = fromInput(startInput);
        const end = fromInput(endInput);
        if (changed.start || changed.end) {
            const failure = timeError(changed.end, start, end);
            if (failure) return failure;
        }
        const payload = {};
        if (changed.title) payload.title = title;
        if (changed.description) payload.description = description;
        if (changed.start) payload.starts_at = start.toISOString();
        if (changed.end) payload.ends_at = end.toISOString();
        if (changed.components) payload.component_ids = components;
        if (changed.suppress) payload.suppress_alerts = suppress;
        return Object.keys(payload).length ? { payload } : { unchanged: true };
    }

    form.addEventListener("submit", async (evt) => {
        evt.preventDefault();
        if (submitBtn.disabled) return;
        window.smClearFormErrors(banner);
        const built = buildBody();
        if (built.unchanged) {
            window.location = "/maintenance";
            return;
        }
        if (built.error) {
            window.smRenderClientError(banner, built.error);
            flag(built.field);
            return;
        }

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
                window.smRenderClientError(banner, `Network error: ${err.message || err}`);
                return;
            }
            if (res.ok) {
                navigating = true;
                window.location = "/maintenance";
                return;
            }
            let json = null;
            try { json = await res.json(); } catch { /* not JSON */ }
            window.smRenderApiError(banner, json, res.status, {
                onField: flag,
            });
        } finally {
            if (!navigating) {
                submitBtn.disabled = false;
                submitBtn.textContent = label;
            }
        }
    });
})();
