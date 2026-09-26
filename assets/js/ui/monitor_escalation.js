// Per-monitor escalation-policy binding on the monitor edit form. Saves to
// PUT /api/v1/targets/{id}/escalation-policy on change. value "" clears the
// binding (the monitor falls back to the org default). Owner-only on the API;
// a non-owner sees the 403 message in the result span. A refused save puts the
// saved choice back, so the select never shows a binding the server turned
// down.
(function () {
    const sel = document.querySelector("[data-monitor-policy-select]");
    if (!sel) return;
    // The combobox redraws its label on `change`; `reverting` keeps that
    // event from saving again.
    let reverting = false;
    const revert = () => {
        for (const o of sel.options) o.selected = o.defaultSelected;
        sel.disabled = false;
        reverting = true;
        sel.dispatchEvent(new Event("change", { bubbles: true }));
        reverting = false;
    };
    const result = document.querySelector("[data-monitor-policy-result]");
    let clearTimer;
    const show = (msg, ok) => {
        if (!result) return;
        clearTimeout(clearTimer);
        result.textContent = msg;
        result.className = "flash-text text-xs " + (ok ? "flash-text--ok" : "flash-text--bad");
        if (ok) clearTimer = setTimeout(() => { result.textContent = ""; }, 4000);
    };
    sel.addEventListener("change", async () => {
        if (reverting) return;
        const targetId = sel.dataset.targetId;
        const policyId = sel.value || null;
        sel.disabled = true;
        try {
            const res = await fetch(`/api/v1/targets/${targetId}/escalation-policy`, {
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
                revert();
                show("✗ " + msg, false);
            }
        } catch (err) {
            show("✗ network error", false);
        } finally {
            sel.disabled = false;
        }
    });
})();
