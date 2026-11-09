// Alert source form and page: writes go to /api/v1/alert-sources.
(() => {
    const HEADERS = {
        "Content-Type": "application/json",
        "Accept": "application/json",
        "X-Requested-With": "uptimepage",
    };
    const GROUPED = ["alertmanager", "grafana"];

    async function call(path, method, body) {
        try {
            const r = await fetch(path, {
                method,
                headers: HEADERS,
                body: body ? JSON.stringify(body) : undefined,
            });
            if (r.ok) {
                window.location.reload();
                return;
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
    }

    document.addEventListener("click", async (evt) => {
        const rotate = evt.target.closest("[data-src-rotate]");
        if (rotate) {
            const ok = await window.smConfirm({
                title: "Rotate the token?",
                body: "A new token works at once. The old one keeps working for 24 hours, so the tool still pages while you update it.",
                confirmLabel: "rotate",
            });
            if (ok) await call(`/api/v1/alert-sources/${rotate.dataset.sourceId}/rotate`, "POST", {});
            return;
        }
        const revoke = evt.target.closest("[data-src-revoke-prev]");
        if (revoke) {
            const ok = await window.smConfirm({
                title: "Cut off the old token?",
                body: "A tool still sending it is refused from now on, and its alerts page nobody.",
                confirmLabel: "cut it off",
                danger: true,
            });
            if (ok) await call(`/api/v1/alert-sources/${revoke.dataset.sourceId}/previous-token`, "DELETE");
            return;
        }
        const pause = evt.target.closest("[data-src-pause]");
        if (pause) {
            const enabled = pause.dataset.enabled === "true";
            await call(`/api/v1/alert-sources/${pause.dataset.sourceId}`, "PATCH", { enabled: !enabled });
        }
    });

    // A source row opens the source from anywhere in it, unless a link in it
    // was clicked or text is being selected.
    document.addEventListener("click", (evt) => {
        const row = evt.target.closest("[data-src-row]");
        if (!row || evt.target.closest("a, button") || window.getSelection()?.toString()) return;
        const href = row.querySelector("[data-src-open]").href;
        if (evt.metaKey || evt.ctrlKey) window.open(href, "_blank", "noopener");
        else window.location.assign(href);
    });

    const tabs = document.querySelectorAll("[data-src-tab]");
    function selectTab(name) {
        tabs.forEach((tab) => {
            const on = tab.dataset.srcTab === name;
            tab.setAttribute("aria-selected", String(on));
            document.getElementById(tab.getAttribute("aria-controls")).hidden = !on;
        });
    }
    if (tabs.length) {
        const wanted = window.location.hash.slice(1);
        if ([...tabs].some((t) => t.dataset.srcTab === wanted)) selectTab(wanted);
        document.addEventListener("click", (evt) => {
            const tab = evt.target.closest("[data-src-tab]");
            if (tab) {
                selectTab(tab.dataset.srcTab);
                history.replaceState(null, "", `#${tab.dataset.srcTab}`);
                return;
            }
            const seg = evt.target.closest("[data-src-filter]");
            if (seg) {
                seg.parentElement.querySelectorAll("[data-src-filter]").forEach((s) => {
                    s.setAttribute("aria-pressed", String(s === seg));
                    s.classList.toggle("sm-rail__seg--active", s === seg);
                });
                return;
            }
            const row = evt.target.closest("tr[data-src-event]");
            if (row && !evt.target.closest("a")) toggleRow(row);
        });
        document.addEventListener("keydown", (evt) => {
            const row = evt.target.closest("tr[data-src-event]");
            if (row && evt.target === row && (evt.key === "Enter" || evt.key === " ")) {
                evt.preventDefault();
                toggleRow(row);
            }
        });
    }
    function toggleRow(row) {
        const open = row.getAttribute("aria-expanded") !== "true";
        row.setAttribute("aria-expanded", String(open));
        row.nextElementSibling.hidden = !open;
    }

    const form = document.getElementById("source-form");
    if (!form) return;
    const banner = document.getElementById("form-errors");
    const creating = form.dataset.mode === "create";

    function kind() {
        const checked = form.querySelector("input[name=kind]:checked");
        return checked ? checked.value : null;
    }

    function syncKind() {
        const k = kind();
        if (!k) return;
        form.querySelector("[data-grouping]").hidden = !GROUPED.includes(k);
        form.querySelector("[data-signing]").hidden = k !== "grafana";
        const name = form.querySelector("[name=name]");
        name.placeholder = name.getAttribute(`data-hint-${k}`) || "";
    }

    form.addEventListener("change", (evt) => {
        if (evt.target.name === "kind") syncKind();
    });
    form.addEventListener("keydown", (evt) => {
        if ((evt.metaKey || evt.ctrlKey) && evt.key === "Enter") {
            evt.preventDefault();
            form.requestSubmit();
        }
    });

    function value(name) {
        const el = form.querySelector(`[name=${name}]`);
        return el ? el.value : null;
    }

    const checkedChannels = () =>
        Array.from(form.querySelectorAll("[data-channel-select]:checked"), (c) => c.value);
    const policySelect = form.querySelector("[name=escalation_policy_id]");
    // An edit sends the route only when it changed: the server refuses a route
    // change that leaves nobody paged, and a source whose channel went away
    // elsewhere must still take a rename or a new secret.
    const loadedChannels = checkedChannels().join(",");
    const loadedPolicy = policySelect ? policySelect.value : null;

    function body() {
        const payload = {
            name: (value("name") || "").trim(),
            notify_recovery: form.querySelector("[name=notify_recovery]").checked,
            renotify_interval_secs: Number(form.querySelector("[name=renotify_interval_secs]:checked")?.value ?? 3600),
        };
        const k = creating ? kind() : null;
        if (creating) payload.kind = k;
        const grouping = form.querySelector("[name=grouping]:checked");
        if (!form.querySelector("[data-grouping]").hidden && grouping) payload.grouping = grouping.value;
        const channels = checkedChannels();
        if (creating || channels.join(",") !== loadedChannels) {
            payload.alerts = channels.map((id) => ({ channel_id: id }));
        }
        if (policySelect) {
            const id = policySelect.value || null;
            if (creating ? id : policySelect.value !== loadedPolicy) payload.escalation_policy_id = id;
        }
        if (!form.querySelector("[data-signing]").hidden) {
            const clear = form.querySelector("[name=clear_secret]");
            const secret = value("signing_secret");
            if (clear && clear.checked) payload.signing_secret = null;
            else if (secret) payload.signing_secret = secret;
        }
        return payload;
    }

    const submit = form.querySelector("button[type=submit]");
    form.addEventListener("submit", async (evt) => {
        evt.preventDefault();
        if (submit.disabled) return;
        window.smClearFormErrors(banner);
        const payload = body();
        if (!payload.name) {
            window.smRenderClientError(banner, "Give the source a name.");
            return;
        }
        const label = submit.textContent;
        submit.disabled = true;
        submit.textContent = "saving…";
        let leaving = false;
        try {
            let res;
            try {
                res = await fetch(form.dataset.action, {
                    method: form.dataset.method,
                    headers: HEADERS,
                    body: JSON.stringify(payload),
                });
            } catch (err) {
                window.smRenderClientError(banner, `Network error: ${err.message || err}`);
                return;
            }
            let json = null;
            try { json = await res.json(); } catch { /* empty */ }
            if (res.ok) {
                leaving = true;
                const id = (json && json.id) || form.dataset.id;
                window.location = `/settings/alert-sources/${id}`;
                return;
            }
            if (json) window.smRenderApiError(banner, json, res.status);
            else window.smRenderClientError(banner, `Request failed (${res.status})`);
        } finally {
            if (!leaving) { submit.disabled = false; submit.textContent = label; }
        }
    });
})();
