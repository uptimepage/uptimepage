// Escalation-policy builder. Manages the dynamic level rows and serialises them
// into a NewEscalationPolicy body for POST/PATCH /api/v1/escalation-policies.
// Shares the error-banner layer from api_form.js (loaded before this).
import { parseSpan } from "./_duration.js";

(function () {
    const form = document.getElementById("escalation-form");
    if (!form) return;
    const levels = document.getElementById("levels");
    const tmpl = document.getElementById("level-template");
    const addBtn = document.getElementById("add-level");
    const rail = form.querySelector("[data-level-rail]");

    function rows() {
        return Array.from(levels.querySelectorAll("[data-level-row]"));
    }

    function count(n, one, many) {
        return n === 0 ? null : `${n} ${n === 1 ? one : many}`;
    }

    function repeats() {
        return parseInt(form.querySelector("[name=repeat_count]:checked")?.value, 10) || 0;
    }

    function levelText(row, last) {
        const paged = [
            count(row.querySelectorAll("[data-channel]:checked").length, "channel", "channels"),
            count(row.querySelectorAll("[data-schedule]:checked").length, "schedule", "schedules"),
            count(row.querySelectorAll("[data-person]").length, "person", "people"),
        ].filter(Boolean);
        const what = paged.length ? paged.join(", ") : "pages no one";
        const wait = row.querySelector("[data-delay]").value.trim() || "0";
        if (!last) return `${what} · then ${wait}`;
        return repeats() > 0 ? `${what} · again after ${wait}` : what;
    }

    // The last level's wait is the gap before the ladder is walked again, so
    // with a single walk it does nothing and is not asked for.
    function syncDelays() {
        const all = rows();
        all.forEach((row, i) => {
            const last = i === all.length - 1;
            const unused = last && repeats() === 0;
            row.querySelector("[data-delay-label]").textContent =
                last ? "Wait before walking again" : "Wait before next level";
            row.querySelectorAll("[data-delay-preset], [data-delay]").forEach((el) => { el.disabled = unused; });
            row.querySelector("[data-delay-unused]").hidden = !unused;
        });
    }

    // One radio group per level, so arrow keys stay within it.
    let rowSeq = 0;
    function initRow(row) {
        const group = `delay-${++rowSeq}`;
        row.querySelectorAll("[data-delay-preset]").forEach((r) => { r.name = group; });
    }

    // A typed wait survives a look at a preset and back.
    levels.addEventListener("change", (evt) => {
        const preset = evt.target.closest("[data-delay-preset]");
        if (!preset) return;
        const input = preset.closest("[data-delay-field]").querySelector("[data-delay]");
        if (preset.value === "custom") {
            input.value = input.dataset.custom ?? input.value;
            input.hidden = false;
            return;
        }
        if (!input.hidden) input.dataset.custom = input.value;
        input.hidden = true;
        input.value = preset.value;
    });

    function syncRail() {
        const all = rows();
        rail.replaceChildren(...all.map((row, i) => {
            const card = document.createElement("a");
            card.className = "check-type-card";
            card.href = `#${row.id}`;
            const name = document.createElement("span");
            name.className = "check-type-card__name";
            name.textContent = `level ${i + 1}`;
            const desc = document.createElement("span");
            desc.className = "check-type-card__desc";
            desc.textContent = levelText(row, i === all.length - 1);
            card.append(name, desc);
            return card;
        }));
    }

    function renumber() {
        rows().forEach((row, i) => {
            row.id = `level-${i + 1}`;
            row.querySelector("[data-level-num]").textContent = String(i + 1);
        });
        syncDelays();
        syncRail();
    }

    rows().forEach(initRow);
    renumber();
    levels.addEventListener("input", syncRail);
    levels.addEventListener("change", syncRail);
    form.addEventListener("change", (evt) => {
        if (evt.target.name !== "repeat_count") return;
        syncDelays();
        syncRail();
    });
    form.addEventListener("keydown", (evt) => {
        if ((evt.metaKey || evt.ctrlKey) && evt.key === "Enter") {
            evt.preventDefault();
            form.requestSubmit();
        }
    });

    addBtn.addEventListener("click", () => {
        levels.appendChild(tmpl.content.cloneNode(true));
        initRow(levels.lastElementChild);
        renumber();
    });

    levels.addEventListener("click", (evt) => {
        const person = evt.target.closest("[data-remove-person]");
        if (person) {
            person.closest("[data-person]").remove();
            syncRail();
            return;
        }
        const rm = evt.target.closest("[data-remove-level]");
        if (!rm) return;
        if (rows().length <= 1) {
            renderClientError("A policy needs at least one level.");
            return;
        }
        rm.closest("[data-level-row]").remove();
        renumber();
    });

    const submitBtn = form.querySelector("button[type=submit]");
    form.addEventListener("submit", async (evt) => {
        evt.preventDefault();
        if (submitBtn.disabled) return;
        clearErrors();
        const built = buildBody();
        if (built.error) {
            renderClientError(built.error);
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
                renderClientError(`Network error: ${err.message || err}`);
                return;
            }
            if (res.ok) { navigating = true; window.location = "/settings/escalation"; return; }
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
        const description = (form.querySelector("[name=description]").value || "").trim();
        const repeat = parseInt(form.querySelector("[name=repeat_count]:checked")?.value, 10);
        const all = rows();
        if (all.length === 0) return { error: "Add at least one level." };
        const steps = [];
        for (let i = 0; i < all.length; i++) {
            // A wait not asked for keeps what it held, or none.
            const field = all[i].querySelector("[data-delay]");
            const delay = parseSpan(field.value) ?? (field.disabled ? 0 : null);
            if (delay === null) {
                const which = i === all.length - 1 ? "before walking again" : "before the next level";
                return { error: `Level ${i + 1}: write the wait ${which} with a unit, such as 90s, 5m or 1h, or 0 for none.` };
            }
            const channels = Array.from(all[i].querySelectorAll("[data-channel]:checked")).map(c => c.value);
            const schedules = Array.from(all[i].querySelectorAll("[data-schedule]:checked")).map(c => c.value);
            const people = Array.from(all[i].querySelectorAll("[data-person]")).map(p => p.dataset.person);
            if (channels.length === 0 && schedules.length === 0 && people.length === 0) {
                return { error: `Level ${i + 1} needs at least one channel or schedule.` };
            }
            const targets = channels.map(id => ({ target_type: "channel", channel_id: id }))
                .concat(schedules.map(id => ({ target_type: "schedule", schedule_id: id })))
                .concat(people.map(id => ({ target_type: "user", user_id: id })));
            steps.push({
                level: i + 1,
                delay_secs: delay,
                targets,
            });
        }
        return {
            payload: {
                name,
                description: description || null,
                repeat_count: Number.isFinite(repeat) ? repeat : 0,
                steps,
            },
        };
    }

    function clearErrors() {
        window.smClearFormErrors(document.getElementById("form-errors"));
    }
    function renderClientError(msg) {
        window.smRenderClientError(document.getElementById("form-errors"), msg);
    }
    function renderApiError(json, status) {
        window.smRenderApiError(document.getElementById("form-errors"), json, status);
    }
})();
