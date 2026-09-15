// /settings/billing: the plan cards pick a target, the action rail sends it
// to /api/v1/account/billing. Every move is confirmed by the provider's
// webhook, so a successful call reloads the page to show what landed rather
// than trusting the button.
(function () {
    const form = document.getElementById("billing-form");
    if (!form) return;
    const API = "/api/v1/account/billing";
    const HEADERS = {
        "Accept": "application/json",
        "Content-Type": "application/json",
        "X-Requested-With": "uptimepage",
    };
    const status = form.dataset.status;
    const owner = form.dataset.owner === "1";
    const errorBox = form.querySelector("[data-billing-error]");
    const msg = form.querySelector("[data-act-msg]");
    const act = name => form.querySelector(`[data-act="${name}"]`);

    function fail(text) {
        if (!errorBox) return;
        errorBox.textContent = text;
        errorBox.classList.remove("hidden");
    }

    function busy(on) {
        form.querySelectorAll("[data-act]").forEach(b => { b.disabled = on; });
        if (msg) msg.textContent = on ? "talking to the provider…" : "";
        // Enabling every button again would undo what layout() decided.
        if (!on) layout();
    }

    async function call(method, path, body) {
        const res = await fetch(API + path, {
            method,
            headers: HEADERS,
            body: body ? JSON.stringify(body) : undefined,
        });
        if (!res.ok) throw new Error(await window.smApiErrorMessage(res, `request failed (${res.status})`));
        return res.json();
    }

    // Back from the checkout: the plan appears when the webhook lands, which
    // is usually seconds, so the page asks until the status moves.
    if (form.dataset.confirming === "1") {
        let tries = 0;
        const tick = async () => {
            tries += 1;
            try {
                const view = await call("GET", "");
                if (view.status !== status) {
                    location.replace("/settings/billing");
                    return;
                }
            } catch { /* keep waiting */ }
            if (tries < 30) setTimeout(tick, 2000);
            else {
                const note = form.querySelector("[data-confirming-note]");
                if (note) note.textContent = "still waiting on the provider. this page updates once the payment is confirmed; reload in a minute.";
            }
        };
        setTimeout(tick, 2000);
    }

    if (!owner) return;

    const cards = Array.from(form.querySelectorAll("[data-plan-card]"));
    const current = cards.find(c => c.hasAttribute("data-plan-current"));
    const currentId = current ? current.querySelector("input").value : null;

    const interval = () => form.dataset.interval;
    const currentInterval = form.dataset.intervalCurrent || null;

    function selected() {
        const c = cards.find(x => x.querySelector("input").checked);
        return c ? { id: c.querySelector("input").value, name: c.dataset.planName } : null;
    }

    // A card without a price on the picked cadence cannot be bought on it.
    function priceCards() {
        const key = interval() === "year" ? "planYear" : "planMonth";
        cards.forEach(c => {
            const input = c.querySelector("input");
            input.disabled = !(key in c.dataset);
            if (input.disabled && input.checked) {
                input.checked = false;
            }
        });
    }

    function spotlight(pick) {
        const id = pick ? pick.id : null;
        form.querySelectorAll("[data-pitch]").forEach(p => { p.hidden = p.dataset.pitch !== id; });
        form.querySelectorAll("[data-cmp-col]").forEach(c => {
            c.classList.toggle("is-target", c.dataset.cmpCol === id && id !== currentId);
        });
    }

    function layout() {
        const pick = selected();
        const otherPlan = pick && pick.id !== currentId;
        const otherCadence = pick && !otherPlan && currentInterval && interval() !== currentInterval;
        const other = otherPlan || otherCadence;
        const pendingPlan = form.dataset.pendingPlan || null;
        // The current plan again, while a move away is booked: undo the move.
        const stay = pick && !other && pendingPlan !== null;
        // The booked move itself: dead at the cadence it is booked on, a
        // cadence switch at the other.
        const rebook = otherPlan && pick.id === pendingPlan;
        const booked = rebook && interval() === form.dataset.pendingInterval;
        spotlight(pick);
        const checkout = act("checkout");
        const change = act("change");
        const cancel = act("cancel");
        const revoke = act("revoke");
        const portal = act("portal");
        const card = act("card");
        const live = status === "active";
        if (checkout) {
            checkout.hidden = live || status === "past_due";
            checkout.disabled = !other;
            checkout.textContent = other ? `checkout ${pick.name}` : "checkout";
        }
        if (change) {
            change.hidden = !live;
            change.disabled = !(other || stay) || booked || form.dataset.cancelBooked === "1";
            change.textContent = booked ? `${pick.name} is booked`
                : rebook ? `bill the booked ${pick.name} ${interval()}ly`
                : otherPlan ? `move to ${pick.name}`
                : otherCadence && pendingPlan !== null ? `keep ${pick.name}, billed ${interval()}ly`
                : otherCadence ? `switch to ${interval()}ly billing`
                : stay ? `keep ${pick.name}`
                : "move plan";
        }
        if (card) card.hidden = status !== "past_due";
        if (revoke) revoke.hidden = !(live && form.dataset.cancelBooked === "1");
        if (cancel) cancel.hidden = !(live && form.dataset.cancelBooked !== "1");
        if (portal) portal.hidden = form.dataset.portal !== "1";
    }

    cards.forEach(c => c.addEventListener("change", layout));
    form.querySelectorAll("[data-interval-rail] input").forEach(r => r.addEventListener("change", () => {
        form.dataset.interval = r.value;
        priceCards();
        layout();
    }));
    priceCards();
    layout();

    async function run(fn) {
        if (errorBox) errorBox.classList.add("hidden");
        busy(true);
        try {
            await fn();
        } catch (err) {
            busy(false);
            fail(err.message || "something went wrong");
        }
    }

    act("checkout")?.addEventListener("click", () => run(async () => {
        const pick = selected();
        const handoff = await call("POST", "/checkout", { plan_id: pick.id, interval: form.dataset.interval });
        location.href = handoff.url;
    }));

    act("change")?.addEventListener("click", () => run(async () => {
        const pick = selected();
        await call("PUT", "/plan", { plan_id: pick.id, interval: form.dataset.interval });
        location.reload();
    }));

    act("cancel")?.addEventListener("click", async () => {
        const ok = await window.smConfirm({
            title: "Cancel the subscription?",
            body: "Paid service runs to the end of the period you already paid for. After that the account lands on its free plan and anything the free plan does not cover is held, not deleted.",
            confirmLabel: "cancel at period end",
            danger: true,
        });
        if (!ok) return;
        run(async () => {
            await call("POST", "/cancel");
            location.reload();
        });
    });

    act("revoke")?.addEventListener("click", () => run(async () => {
        await call("DELETE", "/cancel");
        location.reload();
    }));

    act("portal")?.addEventListener("click", () => run(async () => {
        const handoff = await call("POST", "/portal");
        location.href = handoff.url;
    }));
})();
