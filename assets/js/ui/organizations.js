// /settings/organizations row actions. Delegated off the list so a partial
// refresh leaves no stale handlers. confirm_modal.js re-dispatches the
// data-confirm-modal clicks, so those branches only see confirmed ones.
(function () {
    const list = document.getElementById("orgs-list");
    if (!list) return;

    const headers = {
        "Content-Type": "application/json",
        "Accept": "application/json",
        "X-Requested-With": "uptimepage",
    };

    const banner = () => document.getElementById("orgs-errors");

    const fail = async (res, fallback) => {
        let data = null;
        try { data = await res.json(); } catch { /* not JSON */ }
        const el = banner();
        if (el) window.smRenderApiError(el, data, res.status);
        else window.smToast({ message: fallback });
    };

    // Any of these can change the header — active slug, org menu, incident
    // pill. Cheaper to reload than to patch three places from the client.
    const reload = () => {
        window.smNavCache?.clear();
        window.location.reload();
    };

    list.addEventListener("click", async (ev) => {
        const toggle = ev.target.closest("[data-org-edit]");
        if (toggle) {
            const panel = list.querySelector(`[data-org-panel="${toggle.dataset.orgEdit}"]`);
            if (!panel) return;
            panel.hidden = !panel.hidden;
            toggle.setAttribute("aria-expanded", String(!panel.hidden));
            if (!panel.hidden) panel.querySelector("[data-org-field=name]").focus();
            return;
        }

        const switchBtn = ev.target.closest("[data-org-switch]");
        const deleteBtn = ev.target.closest("[data-org-delete]");
        const restoreBtn = ev.target.closest("[data-org-restore]");
        const btn = switchBtn || deleteBtn || restoreBtn;
        if (!btn) return;

        window.smClearFormErrors(banner());
        btn.disabled = true;
        try {
            let res;
            if (switchBtn) {
                res = await fetch("/api/v1/me/active-org", {
                    method: "POST",
                    headers,
                    body: JSON.stringify({ org_id: switchBtn.dataset.orgSwitch }),
                });
            } else if (deleteBtn) {
                res = await fetch(`/api/v1/orgs/${deleteBtn.dataset.orgDelete}`, {
                    method: "DELETE",
                    headers,
                });
            } else {
                res = await fetch(`/api/v1/orgs/${restoreBtn.dataset.orgRestore}/restore`, {
                    method: "POST",
                    headers,
                });
            }
            if (res.ok) { reload(); return; }
            await fail(res, "Request rejected — refresh the page.");
            btn.disabled = false;
        } catch {
            window.smRenderClientError(banner(), "Network error — try again.");
            btn.disabled = false;
        }
    });

    let checkTimer;
    list.addEventListener("input", (ev) => {
        const input = ev.target.closest("[data-org-field=slug]");
        if (!input) return;
        const form = input.closest("[data-org-form]");
        const avail = form.querySelector("[data-org-avail]");
        form.querySelector("[data-org-status]").textContent = "";
        clearTimeout(checkTimer);
        const slug = input.value.trim();
        if (!slug || slug === form.dataset.currentSlug) { avail.textContent = ""; return; }
        checkTimer = setTimeout(async () => {
            try {
                const data = await window.smOrgIdentity.checkSlug(slug);
                avail.textContent = data.available ? "✓ available" : `✗ ${data.reason || "unavailable"}`;
                avail.className = "font-mono text-xs " + (data.available ? "flash-text--ok" : "flash-text--bad");
            } catch { avail.textContent = ""; }
        }, 300);
    });

    list.addEventListener("submit", async (ev) => {
        const form = ev.target.closest("[data-org-form]");
        if (!form) return;
        ev.preventDefault();

        const orgId = form.dataset.orgForm;
        const currentSlug = form.dataset.currentSlug;
        const errors = form.querySelector("[data-org-errors]");
        const status = form.querySelector("[data-org-status]");
        const nameInput = form.querySelector("[data-org-field=name]");
        const slugInput = form.querySelector("[data-org-field=slug]");
        window.smClearFormErrors(errors);
        status.textContent = "";

        const name = nameInput.value.trim();
        const slug = slugInput.value.trim();
        if (!name) {
            nameInput.setAttribute("aria-invalid", "true");
            window.smRenderClientError(errors, "Enter an organization name.");
            return;
        }
        // Sending an unchanged slug reads as a rename — cutover warning for
        // nothing.
        const body = { name };
        if (slug !== currentSlug) {
            if (!slug) {
                slugInput.setAttribute("aria-invalid", "true");
                window.smRenderClientError(errors, "Enter a URL slug.");
                return;
            }
            const avail = await window.smOrgIdentity.checkSlug(slug).catch(() => null);
            if (!avail) {
                window.smRenderClientError(errors, "Network error — try again.");
                return;
            }
            if (!avail.available) {
                slugInput.setAttribute("aria-invalid", "true");
                window.smRenderClientError(errors, avail.reason || "That slug is unavailable.");
                return;
            }
            const ok = await window.smConfirm({
                title: "Change the org slug?",
                body: `Tooling that uses "${currentSlug}" as X-Uptimepage-Org and the current status-page URL will break. The old slug is freed with no redirect.`,
                confirmLabel: "change slug",
            });
            if (!ok) return;
            body.slug = slug;
        }

        const btn = form.querySelector("button[type=submit]");
        btn.disabled = true;
        try {
            const res = await window.smOrgIdentity.patch(orgId, body);
            if (res.ok) { reload(); return; }
            window.smRenderApiError(errors, res.data, res.status);
        } catch {
            window.smRenderClientError(errors, "Network error — try again.");
        }
        btn.disabled = false;
    });
})();
