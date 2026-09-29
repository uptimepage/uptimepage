// App accounts that put a name on acknowledgements: start a Telegram link or
// unlink one from the account page, or link the Pushover or Slack account an
// offer was sent to.
(function () {
    "use strict";

    const headers = {
        "Accept": "application/json",
        "Content-Type": "application/json",
        "X-Requested-With": "uptimepage",
    };

    async function errorMessage(r) {
        try {
            const body = await r.json();
            if (body?.error?.message) return body.error.message;
        } catch { /* not JSON */ }
        return "HTTP " + r.status;
    }

    // POST so the CSRF guard covers minting a one-time code; the bot link comes
    // back as JSON.
    document.querySelectorAll("[data-link-telegram]").forEach(function (btn) {
        btn.addEventListener("click", async function () {
            // Each request voids the code before it, so a second one in flight
            // could leave the browser on a dead link.
            btn.disabled = true;
            try {
                const r = await fetch("/api/v1/me/linked-apps/telegram", { method: "POST", headers });
                if (!r.ok) throw new Error(await errorMessage(r));
                window.location = (await r.json()).url;
            } catch (err) {
                window.smToast({ message: "could not start: " + err.message, kind: "error" });
                btn.disabled = false;
            }
        });
    });

    document.querySelectorAll("[data-linked-app-remove]").forEach(function (btn) {
        btn.addEventListener("click", async function () {
            const row = btn.closest("[data-linked-app]");
            const url = "/api/v1/me/linked-apps/" + encodeURIComponent(row.dataset.linkedAppId);
            try {
                const r = await fetch(url, { method: "DELETE", headers });
                if (!r.ok) throw new Error(await errorMessage(r));
                window.location.reload();
            } catch (err) {
                window.smToast({ message: "could not unlink: " + err.message, kind: "error" });
            }
        });
    });

    const offer = document.querySelector("[data-link-offer]");
    if (!offer) return;
    const result = document.querySelector("[data-link-result]");
    const button = offer.querySelector("button");
    button.addEventListener("click", async function () {
        button.disabled = true;
        try {
            const r = await fetch("/api/v1/me/linked-apps/" + encodeURIComponent(offer.dataset.app), {
                method: "POST",
                headers,
                body: JSON.stringify({ code: offer.dataset.code }),
            });
            if (!r.ok) throw new Error(await errorMessage(r));
            window.location = "/settings/account";
        } catch (err) {
            result.textContent = "Could not link: " + err.message;
            result.classList.add("flash-text--bad");
            button.disabled = false;
        }
    });
})();
