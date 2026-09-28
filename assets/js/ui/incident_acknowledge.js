// The page an alert's Acknowledge button opens: the press posts back to the
// page's own link, then the page reloads to say where the incident now stands.
// When the alert belongs to another of the viewer's orgs, "view incident"
// switches to it on the click, never on opening the page.
(function () {
    "use strict";

    const headers = { "X-Requested-With": "uptimepage" };
    const result = document.querySelector("[data-acknowledge-result]");

    function fail(message) {
        result.textContent = message;
        result.classList.add("flash-text--bad");
    }

    const view = document.querySelector("[data-switch-org]");
    if (view) {
        view.addEventListener("click", async function (ev) {
            ev.preventDefault();
            try {
                const r = await fetch("/api/v1/me/active-org", {
                    method: "POST",
                    headers: { ...headers, "Content-Type": "application/json" },
                    body: JSON.stringify({ org_id: view.dataset.switchOrg }),
                });
                if (r.status === 204) {
                    window.location.href = view.href;
                    return;
                }
                fail("Could not open it in that organization: HTTP " + r.status);
            } catch (err) {
                fail("Could not open it in that organization: " + err.message);
            }
        });
    }

    const holder = document.querySelector("[data-incident-acknowledge]");
    if (!holder) return;
    const button = holder.querySelector("button");
    button.addEventListener("click", async function () {
        button.disabled = true;
        try {
            const r = await fetch(holder.dataset.action, { method: "POST", headers });
            // Taken, reopened, resolved, gone or signed out: the page explains
            // each one itself.
            if (r.ok || [401, 404, 409].includes(r.status)) {
                window.location.reload();
                return;
            }
            throw new Error("HTTP " + r.status);
        } catch (err) {
            fail("Could not acknowledge: " + err.message);
            button.disabled = false;
        }
    });
})();
