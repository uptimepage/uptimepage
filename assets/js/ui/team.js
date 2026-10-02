// /settings/team: invite form + delegated row actions. confirm_modal.js
// intercepts the data-confirm-modal buttons and re-dispatches on confirm,
// so the handlers below only run for confirmed clicks.
(function () {
    const form = document.getElementById("invite-form");
    if (!form) return;
    const banner = document.getElementById("form-errors");
    const email = document.getElementById("invite-email");
    const role = document.getElementById("invite-role");
    const list = document.getElementById("team-list");

    // The partial re-pins the org on every refresh; the form value is only
    // the pre-first-load fallback. Mutations must target the org whose rows
    // are actually on screen, not the one from page-render time.
    const orgId = () =>
        list.querySelector("[data-org-id]")?.dataset.orgId || form.dataset.orgId;

    const headers = {
        "Content-Type": "application/json",
        "Accept": "application/json",
        "X-Requested-With": "uptimepage",
    };

    form.addEventListener("submit", async (ev) => {
        ev.preventDefault();
        if (!email.value || !email.checkValidity()) {
            email.setAttribute("aria-invalid", "true");
            window.smRenderClientError(banner, "Enter a valid email address.");
            return;
        }
        window.smClearFormErrors(banner);
        // Owner grants full control — match the friction of in-place
        // promotion, which goes through the confirm modal.
        if (role.value === "owner") {
            const ok = await window.smConfirm({
                title: "Invite as owner?",
                body: `${email.value.trim()} will get full control of this organization on accept, including managing members and deleting it.`,
                confirmLabel: "invite as owner",
            });
            if (!ok) return;
        }
        const btn = form.querySelector("button[type=submit]");
        btn.disabled = true;
        try {
            const res = await fetch(`/api/v1/orgs/${orgId()}/invitations`, {
                method: "POST",
                headers,
                body: JSON.stringify({ email: email.value.trim(), role: role.value }),
            });
            if (res.status === 201) {
                email.value = "";
                htmx.trigger("body", "team:refresh");
            } else {
                let data = null;
                try { data = await res.json(); } catch { /* not JSON */ }
                window.smRenderApiError(banner, data, res.status);
            }
        } catch {
            window.smRenderClientError(banner, "Network error — try again.");
        } finally {
            btn.disabled = false;
        }
    });

    list.addEventListener("click", async (ev) => {
        const roleBtn = ev.target.closest("[data-team-role]");
        const removeBtn = ev.target.closest("[data-team-remove]");
        const revokeBtn = ev.target.closest("[data-team-revoke]");
        const resendBtn = ev.target.closest("[data-team-resend]");
        // Unconfirmed clicks never reach here — confirm_modal.js capture
        // listener swallows them and re-dispatches after the user confirms.
        const btn = roleBtn || removeBtn || revokeBtn || resendBtn;
        if (!btn) return;

        window.smClearFormErrors(banner);
        btn.disabled = true;
        let res;
        try {
            if (roleBtn) {
                res = await fetch(`/api/v1/orgs/${orgId()}/members/${btn.dataset.userId}`, {
                    method: "PATCH",
                    headers,
                    body: JSON.stringify({ role: btn.dataset.newRole }),
                });
            } else if (removeBtn) {
                res = await fetch(`/api/v1/orgs/${orgId()}/members/${btn.dataset.userId}`, {
                    method: "DELETE",
                    headers,
                });
            } else if (resendBtn) {
                res = await fetch(
                    `/api/v1/orgs/${orgId()}/invitations/${btn.dataset.invitationId}/resend`,
                    { method: "POST", headers },
                );
            } else {
                res = await fetch(
                    `/api/v1/orgs/${orgId()}/invitations/${btn.dataset.invitationId}`,
                    { method: "DELETE", headers },
                );
            }
            if (res.status === 204) {
                // Leaving the org yourself: the partial would 403 — go home.
                if (removeBtn && removeBtn.dataset.self !== undefined) {
                    window.smNavCache?.clear();
                    window.location.href = "/";
                    return;
                }
                htmx.trigger("body", "team:refresh");
            } else {
                let data = null;
                try { data = await res.json(); } catch { /* not JSON */ }
                window.smRenderApiError(banner, data, res.status);
                btn.disabled = false;
            }
        } catch {
            window.smRenderClientError(banner, "Network error — try again.");
            btn.disabled = false;
        }
    });
})();
