// What an element does once its htmx request settles, declared as data
// attributes rather than hx-on. htmx compiles hx-on bodies with Function(),
// so an inline handler needs script-src 'unsafe-eval'; when it can't compile
// one it swallows the throw, leaving a handler that never runs and never says
// so.
//
//   data-on-done-href    navigate once the request settles, either way
//   data-on-ok-trigger   fire this event on body when it succeeded
//   data-on-ok-href      navigate when it succeeded
//   data-on-ok-remove    drop the closest matching ancestor when it succeeded
//   data-on-fail-into    render data-on-fail-message into this selector
//   data-on-fail-toast   toast this text on a response error
//   data-opens-dialog    click opens the <dialog> with this id
(function () {
    document.body.addEventListener("htmx:afterRequest", (ev) => {
        const elt = ev.detail?.elt;
        if (!elt?.getAttribute || window.smLeaving(ev.detail.xhr)) return;

        const done = elt.getAttribute("data-on-done-href");
        if (done) {
            window.location.href = done;
            return;
        }

        if (ev.detail.successful) {
            const trigger = elt.getAttribute("data-on-ok-trigger");
            if (trigger) window.htmx.trigger("body", trigger);
            const href = elt.getAttribute("data-on-ok-href");
            if (href) window.location.href = href;
            const remove = elt.getAttribute("data-on-ok-remove");
            if (remove) elt.closest(remove)?.remove();
            return;
        }

        const into = elt.getAttribute("data-on-fail-into");
        const banner = into && document.querySelector(into);
        if (banner) {
            window.smRenderClientError(banner, elt.getAttribute("data-on-fail-message"));
        }
    });

    document.body.addEventListener("htmx:responseError", (ev) => {
        const msg = ev.detail?.elt?.getAttribute?.("data-on-fail-toast");
        if (msg) window.smToast({ message: msg });
    });

    document.addEventListener("click", (ev) => {
        const opener = ev.target.closest?.("[data-opens-dialog]");
        if (opener) {
            document.getElementById(opener.getAttribute("data-opens-dialog"))?.showModal();
        }
    });
})();
