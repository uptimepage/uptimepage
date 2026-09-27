// Shared floating-element positioner. Anchors `panel` relative to `trigger`
// with `position: fixed`, flips above if it would overflow the viewport
// bottom, and clamps to the visible area. Used by every popover/menu/listbox
// (monitors row menu, combobox listbox).
//
// opts.align    — "start" (panel left = trigger left, default) or "end"
//                 (panel right = trigger right)
// opts.gap      — px gap between trigger and panel (default 4)
// opts.minWidth — if true, sets panel.minWidth = trigger width (combobox)

(function () {
    // A field inside the panel still takes the pointer.
    window.smPreventPanelBlur = function (panel) {
        panel.addEventListener("mousedown", e => {
            if (!e.target.closest("input")) e.preventDefault();
        });
    };

    window.smPositionFloating = function (trigger, panel, opts) {
        opts = opts || {};
        const align = opts.align || "start";
        const gap = opts.gap == null ? 4 : opts.gap;

        const rect = trigger.getBoundingClientRect();
        panel.hidden = false;
        panel.style.visibility = "hidden";

        const w = panel.offsetWidth || 160;
        const h = panel.offsetHeight || 200;

        let top = rect.bottom + gap;
        if (top + h > window.innerHeight - 8) {
            top = Math.max(8, rect.top - h - gap);
        }

        let left = align === "end" ? rect.right - w : rect.left;
        if (left + w > window.innerWidth - 8) {
            left = Math.max(8, window.innerWidth - w - 8);
        }
        if (left < 8) left = 8;

        panel.style.top = top + "px";
        panel.style.left = left + "px";
        if (opts.minWidth) panel.style.minWidth = rect.width + "px";
        panel.style.visibility = "";
    };
})();
