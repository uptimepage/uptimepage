// The send-test API answers 200 {delivered:true} or a 4xx error envelope, so
// the outcome goes into the row's own result cell rather than being swapped in.
(function () {
    document.body.addEventListener("htmx:afterRequest", (ev) => {
        const btn = ev.detail?.elt;
        if (!btn?.matches?.("[data-channel-test]") || window.smLeaving(ev.detail.xhr)) return;
        const cell = btn.parentElement.querySelector("[data-test-result]");
        if (!cell) return;
        if (ev.detail.successful) {
            cell.textContent = "✓ sent";
            cell.className = "flash-text flash-text--ok text-xs font-medium";
        } else {
            cell.textContent = "✗ " + window.smErrMsg(ev.detail.xhr, "delivery failed");
            cell.className = "flash-text flash-text--bad text-xs font-medium";
        }
    });
})();
