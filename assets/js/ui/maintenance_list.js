// "End now" on an active maintenance window: PATCH an end time that is surely
// in the past, which the server pins to its own clock, then reload so the
// window moves to the past list. The browser's clock is never sent: a fast one
// would otherwise book a future end. A window that already ended counts as
// done, so a repeat press reloads too. The confirm modal (data-confirm-modal)
// re-dispatches the click once confirmed, so this handler only ever sees a
// confirmed press.
(() => {
    const banner = document.getElementById("maintenance-errors");
    const PAST = new Date(0).toISOString();

    document.addEventListener("click", async (evt) => {
        const btn = evt.target.closest("[data-end-now]");
        if (!btn) return;
        window.smClearFormErrors(banner);
        btn.disabled = true;
        try {
            const res = await fetch(`/api/v1/maintenance/${btn.dataset.id}`, {
                method: "PATCH",
                headers: {
                    "Accept": "application/json",
                    "Content-Type": "application/json",
                    "X-Requested-With": "uptimepage",
                },
                body: JSON.stringify({ ends_at: PAST }),
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
