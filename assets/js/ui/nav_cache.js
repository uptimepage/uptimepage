// The nav chrome (org switcher, status pill, avatar) arrives over htmx after
// every full page load, so it pops in late. Replay the last response from
// sessionStorage at once; the fresh fetch still lands and replaces it.
(function () {
    const KEY = "sm_nav";
    const root = document.querySelector("[data-nav-root]");
    const replaced = [];
    let cleared = false;
    let shown = null;

    const safely = (fn) => {
        try {
            return fn();
        } catch {
            return null;
        }
    };
    const forget = () => safely(() => sessionStorage.removeItem(KEY));

    // For edits that keep the user and org but change the chrome, like an org
    // rename. A fetch still in flight must not store the old markup back.
    window.smNavCache = {
        clear() {
            cleared = true;
            forget();
        },
    };

    // Login and error pages carry no nav chrome; dropping the entry here is
    // what keeps the next user from seeing this one's identity.
    if (!root) {
        forget();
        return;
    }

    // Set by the server on every page load. The active org can change on the
    // server or in another tab, so a copy replays only under the identity it
    // was fetched for.
    const identity = document.cookie.match(/(?:^|; )sm_nav_id=([^;]+)/)?.[1] ?? null;

    function replay(html) {
        const tpl = document.createElement("template");
        tpl.innerHTML = html;
        for (const oob of tpl.content.querySelectorAll("[hx-swap-oob]")) {
            const live = document.getElementById(oob.id);
            if (!live) {
                oob.remove();
                continue;
            }
            oob.removeAttribute("hx-swap-oob");
            live.replaceWith(oob);
            replaced.push([oob, live]);
        }
        root.replaceChildren(tpl.content);
        shown = html;
    }

    // A fetch that fails must not leave last page's health claim on screen.
    function revert() {
        for (const [oob, live] of replaced) oob.replaceWith(live);
        replaced.length = 0;
        root.replaceChildren();
        shown = null;
    }

    const stored = identity && safely(() => JSON.parse(sessionStorage.getItem(KEY)));
    if (stored?.id === identity) {
        try {
            replay(stored.html);
        } catch {
            revert();
            forget();
        }
    }

    const isNavRoot = (e) => e.detail.elt?.matches?.("[data-nav-root]");

    // Same markup as the replay: keep the live nodes so the pill animation
    // and an open switcher are not torn down. A root restored from history is
    // a new element and always takes the fresh response.
    document.addEventListener("htmx:beforeSwap", (e) => {
        if (e.detail.elt === root && shown === e.detail.xhr.responseText) {
            e.detail.shouldSwap = false;
        }
    });

    document.addEventListener("htmx:afterRequest", (e) => {
        if (!isNavRoot(e)) return;
        const { successful, xhr } = e.detail;
        if (successful && xhr.responseText) {
            if (identity && !cleared) {
                safely(() =>
                    sessionStorage.setItem(KEY, JSON.stringify({ id: identity, html: xhr.responseText })),
                );
            }
            return;
        }
        if (e.detail.elt === root && shown !== null) revert();
        // Status 0 is an abort or offline blip, not a verdict on the entry.
        if (xhr.status !== 0) forget();
    });
})();
