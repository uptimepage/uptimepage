// "End now" on an active maintenance window: ask the server to end it at its
// own clock, then reload so the window moves to the past list. A window that
// already ended counts as done, so a repeat press reloads too. The confirm
// modal (data-confirm-modal) re-dispatches the click once confirmed, so this
// handler only ever sees a confirmed press.
(() => {
    const banner = document.getElementById("maintenance-errors");

    document.addEventListener("click", async (evt) => {
        const btn = evt.target.closest("[data-end-now]");
        if (!btn) return;
        window.smClearFormErrors(banner);
        btn.disabled = true;
        try {
            const res = await fetch(`/api/v1/maintenance/${btn.dataset.id}/end`, {
                method: "POST",
                headers: {
                    "Accept": "application/json",
                    "X-Requested-With": "uptimepage",
                },
            });
            if (res.ok) {
                window.location.assign("/maintenance");
                return;
            }
            let json = null;
            try { json = await res.json(); } catch { /* not JSON */ }
            if (json && json.error && json.error.code === "MAINTENANCE_COMPLETED") {
                window.location.assign("/maintenance");
                return;
            }
            window.smRenderApiError(banner, json, res.status);
        } catch (err) {
            window.smRenderClientError(banner, `Network error: ${err.message || err}`);
        }
        btn.disabled = false;
    });
})();
