// "Channels that page you" toggles on /settings/on-call. Each change replaces
// the member's full contact set via PUT /api/v1/on-call/my-contacts. A refused
// save puts the saved set back, so the next change is not built on a channel
// the server turned down. A landed save redraws what depends on it: the
// "no channel pages you" notice and the schedule list's reachability notes.
(function () {
    const root = document.querySelector("[data-contacts]");
    if (!root) return;
    const result = root.querySelector("[data-contacts-result]");
    let clearTimer;

    function show(msg, ok) {
        if (!result) return;
        clearTimeout(clearTimer);
        result.textContent = msg;
        result.className = "flash-text text-xs " + (ok ? "flash-text--ok" : "flash-text--bad");
        if (ok) clearTimer = setTimeout(() => { result.textContent = ""; }, 4000);
    }

    function currentIds() {
        return Array.from(root.querySelectorAll("[data-contact]:checked")).map((c) => c.value);
    }

    let inFlight = false;
    root.addEventListener("change", async (evt) => {
        if (!evt.target.closest("[data-contact]")) return;
        if (inFlight) return;
        inFlight = true;
        const boxes = root.querySelectorAll("[data-contact]");
        boxes.forEach((b) => (b.disabled = true));
        try {
            const res = await fetch("/api/v1/on-call/my-contacts", {
                method: "PUT",
                headers: {
                    "Content-Type": "application/json",
                    "Accept": "application/json",
                    "X-Requested-With": "uptimepage",
                },
                body: JSON.stringify({ channel_ids: currentIds() }),
            });
            if (res.ok) {
                boxes.forEach((b) => (b.defaultChecked = b.checked));
                const notice = document.querySelector("[data-unpageable]");
                if (notice) {
                    notice.hidden = Array.from(boxes).some((b) => b.checked && b.hasAttribute("data-delivers"));
                }
                document.body.dispatchEvent(new CustomEvent("oncall:refresh"));
                // The notice sits at the top of the card, out of view from here.
                if (notice && !notice.hidden) show("⚠ saved, but none of these can deliver a page", false);
                else show("✓ saved", true);
            } else {
                let msg = "save failed";
                try { const b = await res.json(); if (b && b.error && b.error.message) msg = b.error.message; } catch { /* */ }
                boxes.forEach((b) => (b.checked = b.defaultChecked));
                show("✗ " + msg, false);
            }
        } catch {
            show("✗ network error", false);
        } finally {
            boxes.forEach((b) => (b.disabled = false));
            inFlight = false;
        }
    });
})();
