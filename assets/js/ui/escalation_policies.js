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

    function renumber() {
        levels.querySelectorAll("[data-level-row]").forEach((row, i) => {
            const n = row.querySelector("[data-level-num]");
            if (n) n.textContent = String(i + 1);
        });
    }

    addBtn.addEventListener("click", () => {
        levels.appendChild(tmpl.content.cloneNode(true));
        renumber();
    });

    levels.addEventListener("click", (evt) => {
        const person = evt.target.closest("[data-remove-person]");
        if (person) {
            person.closest("[data-person]").remove();
            return;
        }
        const rm = evt.target.closest("[data-remove-level]");
        if (!rm) return;
        if (levels.querySelectorAll("[data-level-row]").length <= 1) {
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
        const repeat = parseInt(form.querySelector("[name=repeat_count]").value, 10);
        const rows = Array.from(levels.querySelectorAll("[data-level-row]"));
        if (rows.length === 0) return { error: "Add at least one level." };
        const steps = [];
        for (let i = 0; i < rows.length; i++) {
            const delay = parseSpan(rows[i].querySelector("[data-delay]").value);
            if (delay === null) {
                return { error: `Level ${i + 1}: write the wait before the next level with a unit, such as 90s, 5m or 1h, or 0 to page it at once.` };
            }
            const channels = Array.from(rows[i].querySelectorAll("[data-channel]:checked")).map(c => c.value);
            const schedules = Array.from(rows[i].querySelectorAll("[data-schedule]:checked")).map(c => c.value);
            const people = Array.from(rows[i].querySelectorAll("[data-person]")).map(p => p.dataset.person);
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
