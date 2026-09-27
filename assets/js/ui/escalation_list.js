// Org-default escalation-policy selector on /settings/escalation. The list is
// swapped in by HTMX, so the change handler is delegated from <body> and
// survives every refresh. Saves to PUT /api/v1/escalation-policies/default.
// A refused save puts the saved choice back.
(function () {
    // The combobox redraws its label on `change`; `reverting` keeps that
    // event from saving again.
    let reverting = false;
    const revert = (sel) => {
        for (const o of sel.options) o.selected = o.defaultSelected;
        reverting = true;
        sel.dispatchEvent(new Event("change", { bubbles: true }));
        reverting = false;
    };
    let clearTimer;
    document.body.addEventListener("change", async (evt) => {
        const sel = evt.target.closest && evt.target.closest("[data-default-select]");
        if (!sel || reverting) return;
        const result = sel.closest("[data-default-box]").querySelector("[data-default-result]");
        const show = (msg, ok) => {
            if (!result) return;
            clearTimeout(clearTimer);
            result.textContent = msg;
            result.className = "flash-text text-xs " + (ok ? "flash-text--ok" : "flash-text--bad");
            if (ok) clearTimer = setTimeout(() => { result.textContent = ""; }, 4000);
        };
        const policyId = sel.value || null;
        const focused = document.activeElement;
        sel.disabled = true;
        try {
            const res = await fetch("/api/v1/escalation-policies/default", {
                method: "PUT",
                headers: {
                    "Content-Type": "application/json",
                    "Accept": "application/json",
                    "X-Requested-With": "uptimepage",
                },
                body: JSON.stringify({ policy_id: policyId }),
            });
            if (res.ok) {
                for (const o of sel.options) o.defaultSelected = o.selected;
                show("✓ saved", true);
            } else {
                let msg = "save failed";
                try {
                    const b = await res.json();
                    if (b && b.error && b.error.message) msg = b.error.message;
                } catch { /* non-JSON body */ }
                revert(sel);
                show("✗ " + msg, false);
            }
        } catch (err) {
            revert(sel);
            show("✗ network error", false);
        } finally {
            sel.disabled = false;
            // After the combobox re-enables its button, which it does from a
            // mutation observer.
            queueMicrotask(() => {
                if (document.activeElement === document.body) focused?.focus();
            });
        }
    });
})();
