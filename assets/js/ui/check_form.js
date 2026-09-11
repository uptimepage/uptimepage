// Monitor create/edit form. Handles:
//   - protocol panel swap driven by the type rail (edit renders it locked)
//   - Smart expected-status input ("200" / "200-299" / "200, 201, 204")
//   - JSON submission via fetch (no htmx for JSON-API forms)
//   - "Test now" → POST /api/v1/targets/test, renders verbose response inline
//   - Cmd/Ctrl+Enter to submit
// Headers are gathered via window.smCollectHeaders (header_rows.js).
// Tags are gathered via window.smCollectTags (tag_chip_input.js).

(function () {
    const form = document.getElementById("check-form");
    if (!form) return;

    // Reads back what web::views::exact_duration renders, so the two must
    // agree on the grammar. `""` is 0, so an empty optional field is off.
    const DURATION_UNITS = { s: 1, m: 60, h: 3600, d: 86400 };

    function parseDuration(raw) {
        const t = String(raw ?? "").trim().toLowerCase();
        if (t === "") return 0;
        const m = t.match(/^(\d+)\s*([smhd])?$/);
        if (!m) return null;
        return parseInt(m[1], 10) * (DURATION_UNITS[m[2]] || 1);
    }

    const DURATION_HELP = (label, lo, hi) =>
        `${label} must be between ${lo} and ${hi}. Use a number of seconds, or a unit: 90s, 15m, 2h, 30d.`;

    // Per-kind interval floors, mirrored from the API via data-kind-floors —
    // the slow/fast rail split and defaults all derive from them.
    let KIND_MIN_INTERVAL = {};
    try {
        KIND_MIN_INTERVAL = JSON.parse(form.dataset.kindFloors || "{}");
    } catch { /* validation falls back to the plan floor */ }
    const kindFloor = (kind) => KIND_MIN_INTERVAL[kind] || 10;
    const isSlowKind = (kind) => kindFloor(kind) >= 3600;

    // Sits above the floors: what to offer, and what to open on.
    let KIND_INTERVALS = {};
    try {
        KIND_INTERVALS = JSON.parse(form.dataset.kindIntervals || "{}");
    } catch { /* falls back to the floors */ }
    const pickerFloor = (kind) => KIND_INTERVALS[kind]?.min || kindFloor(kind);
    const suggested = (kind) => KIND_INTERVALS[kind]?.default;
    // Floors reach twelve hours, and "43200 seconds" reads as nothing.
    const floorLabel = (s) =>
        s % 3600 === 0 ? `${s / 3600}h` : s % 60 === 0 ? `${s / 60}m` : `${s}s`;

    // Remember each rail's last cadence so switching kinds back restores it
    // (checking a radio in one rail auto-unchecks the other's — shared name).
    form.querySelectorAll("[data-interval-rail]").forEach((rail) => {
        const cur = rail.querySelector("input[name='interval_s']:checked");
        if (cur) rail.dataset.last = cur.value;
    });

    function applyKindIntervalDefaults(kind) {
        const want = isSlowKind(kind) ? "slow" : "fast";
        const floor = pickerFloor(kind);
        let active = null;
        form.querySelectorAll("[data-interval-rail]").forEach((rail) => {
            const on = rail.dataset.intervalRail === want;
            rail.hidden = !on;
            if (on) active = rail;
        });
        if (!active) return;
        // A cadence this monitor already runs stays pickable, or opening its
        // form would silently re-point it at a preset.
        const stored = form.dataset.interval;
        const keeps = (o) => o.value === stored && Number(o.value) >= kindFloor(kind);
        let firstValid = null;
        active.querySelectorAll("input[name='interval_s']").forEach((o) => {
            const below = Number(o.value) < floor && !keeps(o);
            o.disabled = below;
            const seg = o.closest(".sm-rail__seg");
            if (seg) seg.classList.toggle("hidden", below);
            if (!below && !firstValid) firstValid = o;
        });
        const wanted = active.querySelector(`input[name='interval_s'][value='${suggested(kind)}']`);
        // Kinds sharing a rail would otherwise inherit the previous kind's pick.
        const inherited = form.dataset.mode === "create" && !form.dataset.intervalTouched;
        if (inherited && wanted && !wanted.disabled) {
            wanted.checked = true;
            return;
        }
        const cur = active.querySelector("input[name='interval_s']:checked");
        if (cur && !cur.disabled) return;
        const last = active.querySelector(`input[name='interval_s'][value='${active.dataset.last}']`);
        const def = [last, wanted, firstValid].find((o) => o && !o.disabled);
        if (def) def.checked = true;
    }

    // A heartbeat's cadence rail is hidden, so the form owns the value. A new
    // monitor derives one; an existing one keeps what it has, lowered only if
    // it is coarser than the window it judges, which the API refuses.
    function heartbeatInterval(data, minInterval) {
        const period = parseDuration(data.get("heartbeat_period_s"));
        const grace = parseDuration(data.get("heartbeat_grace_s"));
        if (period === null || grace === null || period === 0) {
            return parseInt(form.dataset.interval, 10) || minInterval;
        }
        const window = period + grace;
        const stored = parseInt(form.dataset.interval, 10);
        const base = form.dataset.mode === "create" || !Number.isInteger(stored)
            ? Math.min(300, Math.max(60, Math.ceil(window / 10)))
            : stored;
        return Math.max(minInterval, Math.min(base, window));
    }

    // Heartbeat is passive: no test-now, no cadence to pick (fixed floor), no
    // probe regions.
    function applyPassiveKind(kind) {
        const passive = kind === "heartbeat";
        const schedule = form.querySelector("[data-schedule-section]");
        if (schedule) schedule.hidden = passive;
        const testBtn = form.querySelector("[data-test-now]");
        if (testBtn) testBtn.hidden = passive;
        const regions = document.querySelector("[data-monitor-regions]");
        if (regions) regions.hidden = passive;
    }

    // A flow only runs where an engine exists, so on the flow kind the picker
    // disables the regions that can't run it (their checked state is left alone;
    // the server clamps on save). Test-now and the region save both skip disabled
    // boxes, so a flow never fans out to a region that would fail.
    function applyFlowRegions(kind) {
        const flow = kind === "flow";
        let flowRegions = 0;
        document.querySelectorAll("[data-region-checkbox]").forEach((cb) => {
            const capable = cb.dataset.flowCapable === "true";
            if (capable) flowRegions++;
            const off = flow && !capable;
            cb.disabled = off;
            const label = cb.closest("label");
            if (label) {
                label.classList.toggle("scope-token--off", off);
                label.title = off ? "No flow engine in this region" : "";
            }
        });
        const note = document.querySelector("[data-flow-region-note]");
        if (note) note.classList.toggle("hidden", !flow);
        // Region quorum only decides anything with 2+ regions probing. A flow
        // monitor that can reach only one flow-capable region has no quorum to
        // pick, so the threshold clamps to 1 whatever is chosen — disable it.
        const policy = document.querySelector("[data-region-policy]");
        if (policy) policy.disabled = flow && flowRegions < 2;
    }

    // The kind's own value field, not the method/record-type select before it.
    function focusFirstField(kind) {
        const panel = form.querySelector(`fieldset[data-variant='${kind}']`);
        panel?.querySelector(
            "input:not([type='hidden']):not([type='checkbox']):not([type='radio']), textarea")?.focus();
    }

    form.addEventListener("change", (evt) => {
        if (evt.target.name === "interval_s") {
            const rail = evt.target.closest("[data-interval-rail]");
            if (rail) rail.dataset.last = evt.target.value;
            form.dataset.intervalTouched = "1";
            return;
        }
        if (evt.target.name !== "check_type") return;
        const want = evt.target.value;
        document.querySelectorAll("[data-variant]").forEach(el => {
            el.classList.toggle("hidden", el.dataset.variant !== want);
        });
        applyKindIntervalDefaults(want);
        applyPassiveKind(want);
        applyFlowRegions(want);
        // So the next keystroke after a rail switch is already the useful one.
        focusFirstField(want);
    });

    const initialKind = currentCheckType();
    applyKindIntervalDefaults(initialKind);
    applyPassiveKind(initialKind);
    applyFlowRegions(initialKind);

    // Expected-status quick presets: a segmented rail that fills the text
    // field (the source of truth). A class preset writes its range; "custom"
    // jumps to the field. Typing anything off-preset lights "custom".
    const statusInput = form.querySelector("[data-status-input]");
    const statusPresets = form.querySelector("[data-status-presets]");
    if (statusInput && statusPresets) {
        const presetRadios = [...statusPresets.querySelectorAll("[data-status-preset]")];
        const norm = (v) => (v || "").replace(/\s+/g, "");
        const syncRailFromField = () => {
            const val = norm(statusInput.value);
            const match = presetRadios.find(r => r.value !== "custom" && norm(r.value) === val)
                || presetRadios.find(r => r.value === "custom");
            presetRadios.forEach(r => { r.checked = r === match; });
        };
        statusPresets.addEventListener("change", (evt) => {
            const r = evt.target.closest("[data-status-preset]");
            if (!r) return;
            if (r.value === "custom") {
                statusInput.focus();
                statusInput.select();
            } else {
                statusInput.value = r.value;
            }
        });
        statusInput.addEventListener("input", syncRailFromField);
        syncRailFromField();
    }

    form.addEventListener("click", (evt) => {
        const testBtn = evt.target.closest("[data-test-now]");
        if (testBtn) handleTestNow(testBtn);
        const applyPeriod = evt.target.closest("[data-apply-period]");
        if (applyPeriod) {
            const field = form.querySelector("[name='heartbeat_period_s']");
            if (field) {
                setDuration(field, applyPeriod.dataset.applyPeriod);
                field.focus();
            }
            applyPeriod.closest("[data-cadence-hint]")?.remove();
        }
    });

    // `input` clears a stuck aria-invalid, `change` marks the form dirty.
    function setDuration(field, value) {
        field.value = value;
        field.dispatchEvent(new Event("input", { bubbles: true }));
        field.dispatchEvent(new Event("change", { bubbles: true }));
    }

    function syncPresetRail(picker) {
        const input = picker.querySelector("[data-duration-input]");
        if (!input) return;
        const typed = parseDuration(input.value);
        picker.querySelectorAll("[data-duration-preset]").forEach((radio) => {
            const preset = parseDuration(radio.value);
            radio.checked = typed !== null && preset === typed;
        });
    }

    form.addEventListener("change", (evt) => {
        const preset = evt.target.closest("[data-duration-preset]");
        if (!preset) return;
        const input = preset.closest("[data-duration-picker]")?.querySelector("[data-duration-input]");
        if (input) setDuration(input, preset.value);
    });

    form.addEventListener("input", (evt) => {
        const input = evt.target.closest("[data-duration-input]");
        if (input) syncPresetRail(input.closest("[data-duration-picker]"));
    });

    form.addEventListener("keydown", (evt) => {
        if ((evt.metaKey || evt.ctrlKey) && evt.key === "Enter") {
            evt.preventDefault();
            form.requestSubmit();
        }
    });

    // Flag a bad numeric value on blur; only that field's own input clears it,
    // so editing one field never wipes another's mark.
    form.addEventListener("focusout", (evt) => {
        const el = evt.target;
        if (el.tagName !== "INPUT" || el.type !== "number") return;
        const raw = el.value.trim();
        if (raw === "") {
            if (el.validity && el.validity.badInput) el.setAttribute("aria-invalid", "true");
            return;
        }
        const n = parseInt(raw, 10);
        const min = el.min === "" ? -Infinity : Number(el.min);
        const max = el.max === "" ? Infinity : Number(el.max);
        const ok = Number.isInteger(n) && String(n) === raw && n >= min && n <= max;
        if (!ok) el.setAttribute("aria-invalid", "true");
    });
    form.addEventListener("input", (evt) => {
        if (evt.target.type === "number") evt.target.removeAttribute("aria-invalid");
    });

    // ARIA radiogroup keyboard nav on the check-type cards. Labels are
    // tabindex-0 as a fallback for browsers that won't focus sr-only radios.
    const checkTypeCards = form.querySelector(".check-type-cards");
    if (checkTypeCards) {
        checkTypeCards.querySelectorAll("[data-check-card]").forEach(label => {
            if (!label.hasAttribute("tabindex")) label.setAttribute("tabindex", "0");
        });

        form.addEventListener("keydown", (evt) => {
            if (evt.key !== "ArrowLeft" && evt.key !== "ArrowRight"
                && evt.key !== "ArrowUp" && evt.key !== "ArrowDown") return;
            const active = document.activeElement;
            if (!active || !checkTypeCards.contains(active)) return;
            if (active.tagName === "INPUT" && active.type !== "radio") return;
            const radios = Array.from(checkTypeCards.querySelectorAll("input[name='check_type']"));
            if (radios.length === 0) return;
            const checkedIdx = radios.findIndex(r => r.checked);
            const delta = (evt.key === "ArrowLeft" || evt.key === "ArrowUp") ? -1 : 1;
            const next = checkedIdx < 0
                ? 0
                : (checkedIdx + delta + radios.length) % radios.length;
            evt.preventDefault();
            radios[next].checked = true;
            radios[next].dispatchEvent(new Event("change", { bubbles: true }));
            (radios[next].closest("[data-check-card]") || radios[next]).focus();
        });

        // Space/Enter selects the focused card.
        checkTypeCards.addEventListener("keydown", (evt) => {
            if (evt.key !== " " && evt.key !== "Enter") return;
            const label = evt.target.closest("[data-check-card]");
            if (!label) return;
            const radio = label.querySelector("input[name='check_type']");
            if (!radio || radio.checked) return;
            evt.preventDefault();
            radio.checked = true;
            radio.dispatchEvent(new Event("change", { bubbles: true }));
        });
    }

    // Auto-fill Name from the primary input until the user types into Name.
    const nameInput = form.querySelector("[name='name']");
    if (nameInput) {
        nameInput.addEventListener("input", () => {
            nameInput.dataset.userTouched = nameInput.value.trim() ? "1" : "";
        });
        const AUTOFILL_SOURCES = [
            ["http_url", urlToName],
            ["tcp_host", v => v.trim()],
            ["ping_host", v => v.trim()],
            ["dns_domain", v => v.trim()],
            ["tls_host", v => v.trim()],
            ["domain_expiry_domain", v => v.trim()],
        ];
        for (const [name, derive] of AUTOFILL_SOURCES) {
            const src = form.querySelector(`[name='${name}']`);
            if (!src) continue;
            src.addEventListener("input", () => {
                if (nameInput.dataset.userTouched) return;
                const derived = derive(src.value);
                if (derived) nameInput.value = derived;
            });
        }
    }

    // Treat a bare host as https:// — one definition shared by URL validation and name autofill.
    const SCHEME_RE = /^[a-z][a-z0-9+.-]*:\/\//i;
    const ensureScheme = (raw) => (SCHEME_RE.test(raw) ? raw : `https://${raw}`);

    // Fail a malformed URL locally; the server stays authoritative for scheme allow-listing and SSRF.
    function normalizeHttpUrl(raw) {
        const trimmed = (raw || "").trim();
        if (!trimmed) {
            return { error: "URL is required — enter the endpoint to monitor.", field: "check.url" };
        }
        const candidate = ensureScheme(trimmed);
        let u;
        try { u = new URL(candidate); }
        catch { return { error: "That doesn’t look like a valid URL.", field: "check.url" }; }
        if (u.protocol !== "http:" && u.protocol !== "https:") {
            return { error: "URL must start with http:// or https://.", field: "check.url" };
        }
        return { url: candidate, prepended: candidate !== trimmed };
    }

    function urlToName(raw) {
        if (!raw) return "";
        try {
            const u = new URL(ensureScheme(raw));
            return u.hostname.replace(/^www\./i, "");
        } catch {
            return "";
        }
    }

    // The title input auto-grows with field-sizing where available; mirror its
    // width manually for engines that lack it so the name never sits in a wide box.
    if (nameInput && !(window.CSS && CSS.supports && CSS.supports("field-sizing", "content"))) {
        const cs = getComputedStyle(nameInput);
        const mirror = document.createElement("span");
        mirror.setAttribute("aria-hidden", "true");
        mirror.style.cssText = "position:absolute;visibility:hidden;white-space:pre;";
        for (const p of ["fontFamily", "fontSize", "fontWeight", "letterSpacing"]) mirror.style[p] = cs[p];
        nameInput.insertAdjacentElement("afterend", mirror);
        const autosize = () => {
            mirror.textContent = nameInput.value || nameInput.placeholder || "";
            const cap = nameInput.parentElement.clientWidth || 9999;
            nameInput.style.width = `${Math.min(mirror.offsetWidth + 6, cap)}px`;
        };
        nameInput.addEventListener("input", autosize);
        autosize();
    }

    // Unsaved-changes guard: warn before leaving a dirtied form, but not while
    // the save itself is navigating away.
    let formDirty = false;
    let submitting = false;
    form.addEventListener("input", () => {
        formDirty = true;
        // Drop a shown validation error once the user starts correcting it.
        if (errorsShown()) clearErrors();
    });
    form.addEventListener("change", () => { formDirty = true; });
    // In-form links (cancel, back) are intentional exits — don't guard them.
    form.querySelectorAll("a[href]").forEach((a) => {
        a.addEventListener("click", () => { formDirty = false; });
    });
    window.addEventListener("beforeunload", (evt) => {
        if (formDirty && !submitting) { evt.preventDefault(); evt.returnValue = ""; }
    });

    form.addEventListener("submit", async (evt) => {
        evt.preventDefault();
        clearErrors();
        const built = buildBody();
        if (built.error) {
            renderClientError(built.error);
            if (built.field) markFieldInvalid(built.field);
            return;
        }
        // Detection threshold rides the payload when the region fieldset is present.
        // Symbolic values (any/majority/all) go as-is; a number becomes {count: n}.
        const regionRoot = document.querySelector("[data-monitor-regions]");
        let regions = [];
        if (regionRoot) {
            const sel = regionRoot.querySelector("[data-region-policy]");
            if (sel && !sel.disabled) {
                const n = parseInt(sel.value, 10);
                built.payload.region_policy = Number.isInteger(n) ? { count: n } : sel.value;
            }
            if (currentCheckType() !== "heartbeat") {
                regions = [...regionRoot.querySelectorAll("[data-region-checkbox]:checked")]
                    .filter((c) => !c.disabled)
                    .map((c) => c.value);
            }
        }
        // On create the set rides the body, so an unrunnable one refuses the
        // whole request instead of leaving a monitor on the default coverage.
        if (regions.length && form.dataset.mode === "create") built.payload.regions = regions;

        // SubmitEvent.submitter is the actual clicked button (null for
        // form.requestSubmit() / Cmd+Enter, which we treat as primary save).
        const submitter = evt.submitter;

        const submitBtns = [...form.querySelectorAll("button[type='submit']")];
        const restoreLabel = submitter ? submitter.innerHTML : null;
        const setSubmitting = (on) => {
            submitting = on;
            submitBtns.forEach((b) => { b.disabled = on; });
            if (!submitter) return;
            if (on) {
                submitter.setAttribute("aria-busy", "true");
                submitter.innerHTML = form.dataset.mode === "create" ? "creating…" : "saving…";
            } else {
                submitter.removeAttribute("aria-busy");
                if (restoreLabel != null) submitter.innerHTML = restoreLabel;
            }
        };
        setSubmitting(true);

        let res;
        try {
            res = await fetch(form.dataset.action, {
                method: form.dataset.method,
                headers: jsonHeaders(),
                body: JSON.stringify(built.payload),
            });
        } catch (err) {
            setSubmitting(false);
            renderClientError(`Network error: ${err.message || err}`);
            return;
        }

        if (res.ok) {
            let id = null;
            try {
                const json = await res.json();
                id = json.id;
            } catch { /* PATCH may return 200 with body; create returns 201. */ }
            if (!id && form.dataset.mode === "edit") {
                const parts = form.dataset.action.split("/");
                id = parts[parts.length - 1];
            }
            if (regions.length && id && form.dataset.mode === "edit") {
                let put;
                try {
                    put = await fetch(`/api/v1/targets/${id}/regions`, {
                        method: "PUT",
                        headers: jsonHeaders(),
                        body: JSON.stringify({ regions }),
                    });
                } catch (err) {
                    setSubmitting(false);
                    renderClientError(`Saved, but the regions did not apply: ${err.message || err}`);
                    return;
                }
                if (!put.ok) {
                    setSubmitting(false);
                    let body;
                    try { body = await put.json(); }
                    catch { renderClientError(`Saved, but the regions did not apply (${put.status})`); return; }
                    renderApiError(body, put.status);
                    return;
                }
            }
            window.location = id ? `/targets/${id}` : "/targets";
            return;
        }

        setSubmitting(false);
        let body;
        try { body = await res.json(); }
        catch { renderClientError(`Request failed (${res.status})`); return; }
        renderApiError(body, res.status);
    });

    function jsonHeaders() {
        return {
            "Content-Type": "application/json",
            "Accept": "application/json",
            "X-Requested-With": "uptimepage",
        };
    }

    // Edit carries the locked kind in a hidden input — nothing to check there.
    function currentCheckType() {
        const el = form.querySelector("input[name='check_type']:checked")
            || form.querySelector("input[name='check_type'][type='hidden']");
        return el ? el.value : "http";
    }

    function buildCheck() {
        const data = new FormData(form);
        const checkType = currentCheckType();
        const numField = (name, min, max, label, field) => {
            const raw = (data.get(name) || "").trim();
            const n = parseInt(raw, 10);
            if (!Number.isInteger(n) || String(n) !== raw || n < min || n > max) {
                return { error: `${label} must be a whole number between ${min} and ${max}.`, field };
            }
            return { value: n };
        };

        if (checkType === "http") {
            const norm = normalizeHttpUrl(data.get("http_url"));
            if (norm.error) return { error: norm.error, field: norm.field };
            if (norm.prepended) {
                const urlInput = form.querySelector("[name='http_url']");
                if (urlInput) {
                    urlInput.value = norm.url;
                    urlInput.dispatchEvent(new Event("input", { bubbles: true }));
                }
            }
            const headers = (window.smCollectHeaders && window.smCollectHeaders()) || {};
            const expectedRaw = (data.get("expected_status_input") || "").trim();
            const expected = parseExpectedStatus(expectedRaw);
            if (expected.error) return { error: expected.error };
            const timeout = numField("http_timeout_ms", 100, 60000, "Timeout", "check.timeout");
            if (timeout.error) return timeout;
            // Optional: keep the default when blank, validate only an entered value.
            let maxRedirects = 5;
            if ((data.get("http_max_redirects") || "").trim() !== "") {
                const parsed = numField("http_max_redirects", 0, 10, "Max redirects", "check.max_redirects");
                if (parsed.error) return parsed;
                maxRedirects = parsed.value;
            }

            const check = {
                type: "http",
                url: norm.url,
                method: data.get("http_method") || "GET",
                timeout: timeout.value,
                follow_redirects: data.get("http_follow_redirects") === "on",
                max_redirects: maxRedirects,
                expected_status: expected.value,
                expected_body_contains: blankToNull(data.get("http_expected_body_contains")),
                headers,
                body: blankToNull(data.get("http_body")),
                verify_tls: data.get("http_verify_tls") === "on",
            };
            return { check };
        }
        if (checkType === "tcp") {
            const port = numField("tcp_port", 1, 65535, "Port", "check.port");
            if (port.error) return port;
            const timeout = numField("tcp_timeout_ms", 100, 60000, "Timeout", "check.timeout");
            if (timeout.error) return timeout;
            return {
                check: {
                    type: "tcp",
                    host: data.get("tcp_host"),
                    port: port.value,
                    timeout: timeout.value,
                },
            };
        }
        if (checkType === "ping") {
            const timeout = numField("ping_timeout_ms", 100, 60000, "Timeout", "check.timeout");
            if (timeout.error) return timeout;
            return {
                check: {
                    type: "ping",
                    host: data.get("ping_host"),
                    timeout: timeout.value,
                },
            };
        }
        if (checkType === "heartbeat") {
            const period = parseDuration(data.get("heartbeat_period_s"));
            if (period === null || period < 60 || period > 2592000) {
                return { error: DURATION_HELP("Expect a ping every", "60s", "30d"), field: "check.period" };
            }
            const grace = parseDuration(data.get("heartbeat_grace_s"));
            if (grace === null || grace > 2592000) {
                return { error: DURATION_HELP("Grace", "0", "30d"), field: "check.grace" };
            }
            const maxRuntime = parseDuration(data.get("heartbeat_max_runtime_s"));
            if (maxRuntime === null) {
                return { error: DURATION_HELP("Max run time", "60s", "30d"), field: "check.max_runtime" };
            }
            if (maxRuntime && (maxRuntime < 60 || maxRuntime > 2592000)) {
                return { error: DURATION_HELP("Max run time", "60s", "30d"), field: "check.max_runtime" };
            }
            return {
                check: {
                    type: "heartbeat",
                    period: period * 1000,
                    grace: grace * 1000,
                    max_runtime: maxRuntime ? maxRuntime * 1000 : null,
                },
            };
        }
        if (checkType === "dns") {
            const timeout = numField("dns_timeout_ms", 100, 60000, "Timeout", "check.timeout");
            if (timeout.error) return timeout;
            return {
                check: {
                    type: "dns",
                    domain: data.get("dns_domain"),
                    record_type: data.get("dns_record_type"),
                    resolver: blankToNull(data.get("dns_resolver")),
                    expected_contains: blankToNull(data.get("dns_expected_contains")),
                    timeout: timeout.value,
                },
            };
        }
        if (checkType === "tls_cert") {
            const warn = numField("tls_warn_days", 1, 365, "Warn days", "check.warn_days");
            if (warn.error) return warn;
            const crit = numField("tls_critical_days", 1, 365, "Critical days", "check.critical_days");
            if (crit.error) return crit;
            if (crit.value >= warn.value) {
                return { error: "Critical days must be less than warn days.", field: "check.warn_days" };
            }
            const port = numField("tls_port", 1, 65535, "Port", "check.port");
            if (port.error) return port;
            const timeout = numField("tls_timeout_ms", 100, 60000, "Timeout", "check.timeout");
            if (timeout.error) return timeout;
            return {
                check: {
                    type: "tls_cert",
                    host: data.get("tls_host"),
                    port: port.value,
                    server_name: blankToNull(data.get("tls_server_name")),
                    warn_days: warn.value,
                    critical_days: crit.value,
                    timeout: timeout.value,
                },
            };
        }
        if (checkType === "domain_expiry") {
            const warn = numField("domain_expiry_warn_days", 1, 365, "Warn days", "check.warn_days");
            if (warn.error) return warn;
            const crit = numField("domain_expiry_critical_days", 1, 365, "Critical days", "check.critical_days");
            if (crit.error) return crit;
            if (crit.value >= warn.value) {
                return { error: "Critical days must be less than warn days.", field: "check.warn_days" };
            }
            const timeout = numField("domain_expiry_timeout_ms", 100, 60000, "Timeout", "check.timeout");
            if (timeout.error) return timeout;
            return {
                check: {
                    type: "domain_expiry",
                    domain: data.get("domain_expiry_domain"),
                    warn_days: warn.value,
                    critical_days: crit.value,
                    timeout: timeout.value,
                },
            };
        }
        if (checkType === "flow") {
            const startUrl = (data.get("flow_start_url") || "").trim();
            if (startUrl === "") {
                return { error: "Start URL is required.", field: "check.start_url" };
            }
            const timeout = numField("flow_timeout_s", 1, 120, "Timeout", "check.timeout");
            if (timeout.error) return timeout;
            const stepTimeout = numField("flow_step_timeout_s", 1, 60, "Step timeout", "check.step_timeout");
            if (stepTimeout.error) return stepTimeout;
            const steps = window.smCollectFlowSteps ? window.smCollectFlowSteps() : [];
            if (steps.length === 0) {
                return { error: "Add at least one step.", field: "check.steps" };
            }
            return {
                check: {
                    type: "flow",
                    start_url: startUrl,
                    steps,
                    timeout: timeout.value * 1000,
                    step_timeout: stepTimeout.value * 1000,
                    verify_tls: data.get("flow_verify_tls") === "on",
                },
            };
        }
        return { error: `Unknown check type: ${checkType}` };
    }

    function buildBody() {
        const data = new FormData(form);
        const name = (data.get("name") || "").trim();
        if (!name) {
            return { error: "Name is required — label this monitor so you can find it later.", field: "name" };
        }
        const built = buildCheck();
        if (built.error) return { error: built.error, field: built.field };
        const check = built.check;

        const tags = (window.smCollectTags && window.smCollectTags()) || [];

        const alerts = [];
        for (const row of form.querySelectorAll("[data-channel-row]")) {
            const cb = row.querySelector("[data-channel-select]");
            if (!cb || !cb.checked) continue;
            alerts.push({ channel_id: cb.value });
        }

        const confEl = form.querySelector("input[name='alert_confirmations']:checked");
        const confirmations = confEl ? parseInt(confEl.value, 10) : 2;
        if (!Number.isInteger(confirmations) || confirmations < 1) {
            return { error: "Open incident after must be a whole number of failed checks (≥ 1)." };
        }
        const recoveryEl = form.querySelector("[data-notify-recovery]");
        const renotifyEl = form.querySelector("[data-renotify-secs] input:checked");

        const planMin = Number(form.dataset.minInterval) || 60;
        const kind = data.get("check_type") || "http";
        const minInterval = Math.max(planMin, kindFloor(kind));
        const interval = kind === "heartbeat"
            ? heartbeatInterval(data, minInterval)
            : parseInt(data.get("interval_s"), 10);
        if (!Number.isInteger(interval) || interval < minInterval) {
            return { error: `Check interval must be at least ${floorLabel(minInterval)}.` };
        }

        const groupRaw = (data.get("group_name") || "").trim();
        const ownerRaw = (data.get("owner_user_id") || "").trim();
        const payload = {
            name,
            interval,
            enabled: data.get("enabled") === "on",
            tags,
            check,
            alerts,
            alert_confirmations: confirmations,
        };
        // Absent with zero channels: create takes the server defaults,
        // edit (partial PATCH) keeps the stored values.
        if (recoveryEl) payload.notify_recovery = recoveryEl.checked;
        if (renotifyEl) payload.renotify_interval_secs = parseInt(renotifyEl.value, 10) || 0;
        payload.group_name = groupRaw === "" ? null : groupRaw;
        payload.owner_user_id = ownerRaw === "" ? null : ownerRaw;
        return { payload };
    }

    // "200" → Exact; "200-299" → Range; "200, 201, 204" → OneOf.
    function parseExpectedStatus(raw) {
        if (raw.length === 0) {
            return { error: "Expected status is required (e.g. 200 or 200-299)." };
        }
        let m;
        if ((m = /^(\d{3})$/.exec(raw))) {
            return { value: { kind: "exact", value: parseInt(m[1], 10) } };
        }
        if ((m = /^(\d{3})\s*-\s*(\d{3})$/.exec(raw))) {
            const min = parseInt(m[1], 10);
            const max = parseInt(m[2], 10);
            if (max < min) {
                return { error: "Expected status range max must be ≥ min." };
            }
            return { value: { kind: "range", value: { min, max } } };
        }
        if (/^[\d,\s]+$/.test(raw)) {
            const arr = raw
                .split(",")
                .map(s => s.trim())
                .filter(Boolean)
                .map(s => parseInt(s, 10));
            if (arr.length === 0 || arr.some(n => !Number.isInteger(n) || n < 100 || n > 599)) {
                return { error: "Expected status list must be HTTP codes (100–599)." };
            }
            if (arr.length === 1) {
                return { value: { kind: "exact", value: arr[0] } };
            }
            return { value: { kind: "one_of", value: arr } };
        }
        return {
            error: "Expected status must be like 200, 200-299, or 200, 201, 204.",
        };
    }

    // Run one test (optionally pinned to `region`) and render the outcome into
    // `resultEl` via the shared .test-result renderers.
    // `state` folds the expectation match into the status: ok / warn / bad.
    async function runTest(resultEl, check, region) {
        const failed = () => ({ result: null, steps: [], state: "bad" });
        let res;
        try {
            res = await fetch("/api/v1/targets/test", {
                method: "POST",
                headers: jsonHeaders(),
                body: JSON.stringify(region ? { check, region } : { check }),
            });
        } catch (err) {
            window.smRenderCheckError(resultEl, `Network error: ${err.message || err}`);
            return failed();
        }
        if (res.status === 429) {
            const retry = res.headers.get("retry-after");
            window.smRenderCheckError(resultEl, retry
                ? `Rate limited — retry in ${retry}s`
                : "Rate limited — slow down");
            return failed();
        }
        // Read once as text, then try to parse: error responses are not always
        // our JSON envelope. A body-deserialize rejection (e.g. a missing
        // required field) comes back as axum's plain-text 422 — surface that
        // reason instead of a generic "rejected".
        const raw = await res.text();
        let body = null;
        try { body = raw ? JSON.parse(raw) : null; } catch { body = null; }
        if (!res.ok) {
            const code = (body && body.error && body.error.code) || `HTTP ${res.status}`;
            const detail = (body && body.error && body.error.message)
                || (raw && raw.trim())
                || "Test rejected.";
            const message = detail.length > 300 ? `${detail.slice(0, 300)}…` : detail;
            window.smRenderCheckError(resultEl, `${code}: ${message}`);
            return failed();
        }
        if (!body) {
            window.smRenderCheckError(resultEl, "Bad JSON in test response.");
            return failed();
        }
        const result = body.result || {};
        window.smRenderCheckResult(resultEl, result, {
            matched: body.matched_expectations,
            headers: body.response_headers_preview || [],
            body: body.response_body_snippet || null,
            evidence: body.flow_evidence || null,
        });
        const matched = body.matched_expectations !== false;
        const state = !matched || result.status === "down"
            ? "bad"
            : result.status === "degraded" ? "warn" : "ok";
        return { result, steps: body.flow_steps || [], state };
    }

    const sheet = form.querySelector("[data-test-sheet]");
    const sheetBody = form.querySelector("#test-sheet-body");
    const sheetVerdict = form.querySelector("[data-test-verdict]");
    const sheetExpand = form.querySelector("[data-test-expand]");

    function expandSheet(on) {
        if (!sheetBody) return;
        sheetBody.hidden = !on;
        sheetExpand.setAttribute("aria-expanded", String(on));
        sheetExpand.textContent = on ? "details ▴" : "details ▾";
    }

    function showSheet(state, label, expand) {
        if (!sheet) return;
        delete sheet.dataset.leaving;
        sheet.hidden = false;
        sheetVerdict.className = `test-result test-sheet__verdict${state ? ` test-result--${state}` : ""}`;
        sheetVerdict.textContent = label;
        if (expand !== undefined) expandSheet(expand);
    }

    // Focus returns to the button, or a keyboard dismiss drops it to the
    // document. Reduced motion has no animation end to wait for.
    function hideSheet() {
        form.querySelector("[data-test-now]")?.focus();
        if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
            sheet.hidden = true;
            return;
        }
        sheet.dataset.leaving = "1";
        sheet.addEventListener("animationend", () => {
            sheet.hidden = true;
            delete sheet.dataset.leaving;
        }, { once: true });
    }

    if (sheet) {
        sheetExpand.addEventListener("click", () => expandSheet(sheetBody.hidden));
        form.querySelector("[data-test-dismiss]").addEventListener("click", hideSheet);
    }

    function summarize(outcomes, regions) {
        const state = outcomes.some(o => o.state === "bad")
            ? "bad"
            : outcomes.some(o => o.state === "warn") ? "warn" : "ok";
        if (regions.length === 0) {
            const r = outcomes[0].result;
            if (!r) return { state, label: "test failed" };
            const bits = [
                r.status ? r.status.toUpperCase() : "UNKNOWN",
                r.duration_ms != null ? `${r.duration_ms} ms` : null,
                r.response_code != null ? `HTTP ${r.response_code}` : null,
            ].filter(Boolean);
            return { state, label: bits.join(" · ") };
        }
        const failed = regions.filter((_, i) => outcomes[i].state === "bad").map(r => r.label);
        const up = outcomes.length - failed.length;
        const label = `${up}/${outcomes.length} up`;
        return { state, label: failed.length ? `${label} · ${failed.join(", ")} failed` : label };
    }

    async function handleTestNow(btn) {
        const resultEl = document.querySelector("[data-test-result]");
        clearErrors();
        const built = buildCheck();
        if (built.error) {
            renderClientError(built.error);
            if (built.field) markFieldInvalid(built.field);
            return;
        }
        // Fan out to every region selected on the form; the agent in each region
        // runs the probe.
        const allBoxes = [...form.querySelectorAll("[data-region-checkbox]")];
        const regions = allBoxes
            .filter((c) => c.checked && !c.disabled)
            .map((c) => ({ id: c.value, label: (c.closest("label")?.textContent || c.value).trim() }));
        // Selector shown but every region deselected → an explicit "no regions"
        // choice; don't silently probe the default region.
        if (allBoxes.length > 0 && regions.length === 0) {
            window.smRenderCheckError(resultEl, "Select at least one region to test.");
            showSheet("bad", "no regions selected", true);
            return;
        }
        btn.disabled = true;
        btn.setAttribute("aria-busy", "true");
        const btnLabel = btn.innerHTML;
        btn.innerHTML = "testing…";
        showSheet("", "testing…", false);
        // Per-step playback only makes sense for a single verdict; a multi-region
        // fan-out keeps its per-region banners.
        const isFlow = built.check.type === "flow";
        const paintFlow = isFlow && regions.length <= 1;
        if (paintFlow) window.smFlowTestStart?.();
        else if (isFlow) window.smFlowTestReset?.();
        try {
            if (regions.length === 0) {
                // No region selector (single-region plan) → test the default region.
                window.smRenderCheckRunning(resultEl);
                const outcome = await runTest(resultEl, built.check, null);
                if (paintFlow) window.smFlowTestResult?.(outcome.result, outcome.steps);
                const { state, label } = summarize([outcome], regions);
                // A clean pass needs no reading; anything else opens itself.
                showSheet(state, label, state !== "ok");
                return;
            }
            const rows = window.smRenderRegionTestRunning(resultEl, regions);
            const outcomes = await Promise.all(regions.map((r) => runTest(rows[r.id], built.check, r.id)));
            if (paintFlow) window.smFlowTestResult?.(outcomes[0].result, outcomes[0].steps);
            const { state, label } = summarize(outcomes, regions);
            showSheet(state, label, state !== "ok");
        } finally {
            btn.disabled = false;
            btn.removeAttribute("aria-busy");
            btn.innerHTML = btnLabel;
        }
    }

    function blankToNull(v) { return v && v.length > 0 ? v : null; }

    function errorsShown() {
        const banner = document.getElementById("form-errors");
        return !!banner && !banner.classList.contains("hidden");
    }

    // Flag a field as invalid and focus it; scroll into view only when the
    // error came from the server (the field may be far from the action).
    function markFieldInvalid(field, scroll) {
        const el = fieldForApiPath(field);
        if (!el) return;
        window.smRevealCollapsibleFor?.(el);
        window.smRevealOptionalFor?.(el);
        el.setAttribute("aria-invalid", "true");
        el.focus({ preventScroll: true });
        if (scroll) el.scrollIntoView({ block: "center", behavior: "smooth" });
    }

    function clearErrors() {
        window.smClearFormErrors(document.getElementById("form-errors"));
    }

    function renderClientError(msg) {
        window.smRenderClientError(document.getElementById("form-errors"), msg);
    }

    function renderApiError(json, status) {
        window.smRenderApiError(document.getElementById("form-errors"), json, status, {
            messageFor: (err) => err.code === "SSRF_BLOCKED"
                ? "This URL points to a private/internal range and is blocked. " +
                  "If you need to monitor internal services, deploy a monitor instance inside that network."
                : null,
            onField: (field) => markFieldInvalid(field, true),
        });
    }

    const API_PATH_TO_FORM = {
        "check.url": "http_url",
        "check.verify_tls": "http_verify_tls",
        "check.body": "http_body",
        "check.expected_status": "expected_status_input",
        "check.record_type": "dns_record_type",
        "check.resolver": "dns_resolver",
        "check.expected_contains": "dns_expected_contains",
        "check.server_name": "tls_server_name",
        "interval": "interval_s",
        "renotify_interval_secs": "renotify_secs",
        "name": "name",
    };

    // Protocol-dependent paths: inputs follow `<kind-ish prefix>_<suffix>` and
    // live inside their `fieldset[data-variant]`, so resolving by suffix
    // within the selected panel covers new kinds with no extra mapping.
    const API_PATH_SUFFIX = {
        "check.host": "_host",
        "check.port": "_port",
        "check.domain": "_domain",
        "check.timeout": "_timeout_ms",
        "check.max_redirects": "_max_redirects",
        "check.warn_days": "_warn_days",
        "check.critical_days": "_critical_days",
        "check.period": "_period_s",
        "check.grace": "_grace_s",
    };

    function fieldForApiPath(path) {
        const suffix = API_PATH_SUFFIX[path];
        if (suffix) {
            return form.querySelector(
                `fieldset[data-variant='${currentCheckType()}'] [name$='${suffix}']`);
        }
        if (path.startsWith("check.headers")) {
            return form.querySelector("[name='http_header_key']");
        }
        if (path === "tags") {
            return form.querySelector("[data-tag-add]");
        }
        const name = API_PATH_TO_FORM[path];
        if (!name) return null;
        return form.querySelector(`[name="${name}"]`);
    }

    // The chip tracks the tags being edited here, not the saved ones.
    const tagChips = form.querySelector("[data-tag-chips]");
    function syncRuleChips() {
        const tags = (window.smCollectTags && window.smCollectTags()) || [];
        for (const row of form.querySelectorAll("[data-channel-row]")) {
            const chip = row.querySelector("[data-rule-chip]");
            if (!chip) continue;
            let rule = [];
            try {
                rule = JSON.parse(row.dataset.ruleTags || "[]");
            } catch {
                rule = [];
            }
            chip.hidden = !(window.smTagRuleCovers && window.smTagRuleCovers(rule, tags));
        }
    }
    if (tagChips) {
        tagChips.addEventListener("change", syncRuleChips);
        // A tag added through the input never fires change on the container.
        new MutationObserver(syncRuleChips).observe(tagChips, { childList: true });
    }
    syncRuleChips();
})();
