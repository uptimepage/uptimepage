// Day-strip popover: one shared `#day-popover`, per-day data read from
// the JSON blob at `#day-strip-data` (keyed by data-comp+data-day on
// each <button>). Only writes top/left/--day-pop-arrow-x; all looks
// in CSS.
(function () {
    "use strict";

    var ARROW_GAP_PX = 10;
    var EDGE_MARGIN_PX = 8;
    var HOVER_CLOSE_DELAY = 140;

    var data = null;         // parsed strip JSON, refreshed on swap
    var open = null;         // { trigger, comp, day }
    var pinned = false;
    var closeTimer = null;

    function loadData() {
        var node = document.getElementById("day-strip-data");
        if (!node) { data = null; return; }
        try { data = JSON.parse(node.textContent || "{}"); }
        catch (_) { data = null; }
    }

    function entry(trigger) {
        if (!data) return null;
        var c = data[trigger.getAttribute("data-comp")];
        if (!c) return null;
        var idx = parseInt(trigger.getAttribute("data-day"), 10);
        if (!isFinite(idx) || idx < 0 || idx >= c.days.length) return null;
        return c.days[idx];
    }

    // --- Render ---------------------------------------------------------

    function popover() { return document.getElementById("day-popover"); }

    function slot(p, name) { return p.querySelector('[data-slot="' + name + '"]'); }

    function fill(p, e) {
        slot(p, "date").textContent = e.date;

        var spans = slot(p, "spans");
        var none = slot(p, "none");
        while (spans.firstChild) spans.removeChild(spans.firstChild);
        var spanTpl = document.getElementById("day-popover-span-tpl");
        if (e.spans && e.spans.length && spanTpl) {
            for (var s = 0; s < e.spans.length; s++) {
                var row = spanTpl.content.cloneNode(true);
                var li = row.querySelector("li");
                li.className = "day-pop-status " + e.spans[s].state_class;
                li.querySelector(".day-pop-status__label").textContent = e.spans[s].state;
                li.querySelector(".day-pop-status__duration").textContent = e.spans[s].duration;
                spans.appendChild(row);
            }
            spans.hidden = false;
            none.hidden = true;
        } else {
            spans.hidden = true;
            none.hidden = false;
        }

        var list = slot(p, "related-list");
        var heading = slot(p, "related-heading");
        while (list.firstChild) list.removeChild(list.firstChild);
        var tpl = document.getElementById("day-popover-related-tpl");
        if (e.related && e.related.length && tpl) {
            for (var i = 0; i < e.related.length; i++) {
                var item = tpl.content.cloneNode(true);
                var a = item.querySelector("a");
                a.href = e.related[i].url;
                a.textContent = e.related[i].title;
                list.appendChild(item);
            }
            heading.hidden = false;
        } else {
            heading.hidden = true;
        }
    }

    // --- Position -------------------------------------------------------

    function position(trigger, p) {
        p.style.left = "0px";
        p.style.top = "0px";
        p.style.removeProperty("--day-pop-arrow-x");
        var t = trigger.getBoundingClientRect();
        var box = p.getBoundingClientRect();
        var vw = document.documentElement.clientWidth;
        var triggerCenter = t.left + t.width / 2;
        var left = triggerCenter - box.width / 2;
        var maxLeft = vw - box.width - EDGE_MARGIN_PX;
        if (left < EDGE_MARGIN_PX) left = EDGE_MARGIN_PX;
        if (left > maxLeft) left = maxLeft;
        var top = t.bottom + ARROW_GAP_PX;
        p.style.left = left + "px";
        p.style.top = top + "px";
        var arrowX = Math.max(8, Math.min(box.width - 8, triggerCenter - left));
        p.style.setProperty("--day-pop-arrow-x", arrowX + "px");
    }

    // --- Open / close ---------------------------------------------------

    function show(trigger) {
        if (!data) loadData();
        var e = entry(trigger);
        if (!e) return;
        var p = popover();
        if (!p) return;
        if (open && open.trigger !== trigger) hideOpen();
        fill(p, e);
        p.hidden = false;
        trigger.setAttribute("aria-expanded", "true");
        open = { trigger: trigger };
        position(trigger, p);
    }

    function hideOpen() {
        if (!open) return;
        var p = popover();
        if (p) {
            p.hidden = true;
            p.style.removeProperty("--day-pop-arrow-x");
        }
        // Skip if the trigger was swapped out by htmx — writing to a
        // detached node is harmless but pointless.
        if (open.trigger.isConnected) {
            open.trigger.setAttribute("aria-expanded", "false");
        }
        open = null;
        pinned = false;
    }

    function scheduleClose() {
        if (pinned) return;
        clearTimeout(closeTimer);
        closeTimer = setTimeout(hideOpen, HOVER_CLOSE_DELAY);
    }
    function cancelClose() { clearTimeout(closeTimer); }

    // --- Listeners ------------------------------------------------------

    document.addEventListener("mouseover", function (e) {
        var trigger = e.target.closest && e.target.closest("[data-day-trigger]");
        if (trigger) {
            cancelClose();
            if (!open || open.trigger !== trigger) show(trigger);
            return;
        }
        if (open && e.target.closest && e.target.closest("#day-popover")) {
            cancelClose();
        }
    });

    document.addEventListener("mouseout", function (e) {
        if (!open) return;
        var to = e.relatedTarget;
        if (to && to.closest) {
            if (to.closest("[data-day-trigger]") === open.trigger) return;
            if (to.closest("#day-popover")) return;
        }
        var from = e.target;
        var leftTrigger = from === open.trigger ||
            (from.closest && from.closest("[data-day-trigger]") === open.trigger);
        var leftPopover = from.closest && from.closest("#day-popover");
        if (leftTrigger || leftPopover) scheduleClose();
    });

    document.addEventListener("focusin", function (e) {
        var trigger = e.target.closest && e.target.closest("[data-day-trigger]");
        if (trigger) { cancelClose(); show(trigger); }
    });
    document.addEventListener("focusout", function (e) {
        if (!open) return;
        var to = e.relatedTarget;
        if (to && to.closest && (
            to.closest("[data-day-trigger]") === open.trigger ||
            to.closest("#day-popover")
        )) return;
        scheduleClose();
    });

    document.addEventListener("click", function (e) {
        var trigger = e.target.closest && e.target.closest("[data-day-trigger]");
        if (trigger) {
            e.preventDefault();
            cancelClose();
            if (open && open.trigger === trigger && pinned) { hideOpen(); }
            else { show(trigger); pinned = true; }
            return;
        }
        if (!open) return;
        if (e.target.closest && e.target.closest("#day-popover")) return;
        hideOpen();
    });

    document.addEventListener("keydown", function (e) {
        if (e.key === "Escape" && open) { hideOpen(); return; }
        var trigger = e.target.closest && e.target.closest("[data-day-trigger]");
        if (!trigger) return;
        var dir = 0, jump = "";
        switch (e.key) {
            case "ArrowLeft":  dir = -1; break;
            case "ArrowRight": dir =  1; break;
            case "Home":       jump = "first"; break;
            case "End":        jump = "last";  break;
            default: return;
        }
        e.preventDefault();
        var strip = trigger.parentElement;
        if (!strip) return;
        var cells = strip.querySelectorAll("[data-day-trigger]");
        if (!cells.length) return;
        var target;
        if (jump === "first") target = cells[0];
        else if (jump === "last") target = cells[cells.length - 1];
        else {
            var idx = Array.prototype.indexOf.call(cells, trigger) + dir;
            if (idx < 0 || idx >= cells.length) return;
            target = cells[idx];
        }
        // Roving tabindex: only the focused cell is in the tab order.
        for (var i = 0; i < cells.length; i++) cells[i].setAttribute("tabindex", "-1");
        target.setAttribute("tabindex", "0");
        target.focus();
    });

    window.addEventListener("scroll", function () { if (open) hideOpen(); }, true);
    window.addEventListener("resize", function () { if (open) hideOpen(); });

    // 30s htmx swap replaces the JSON blob — re-read it.
    document.body && document.body.addEventListener("htmx:afterSwap", function () {
        hideOpen();
        loadData();
    });

    if (document.readyState === "loading") {
        document.addEventListener("DOMContentLoaded", loadData);
    } else {
        loadData();
    }
})();
