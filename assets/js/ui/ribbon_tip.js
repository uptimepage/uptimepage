// Themed hover tooltip for the fleet-ribbon cells, replacing the native
// `title=` (unthemed, ~1s delay). A single <body> node plus document
// delegation survives the table's 5s swap and the cell's scaleY hover; names
// go in via textContent so a customer monitor name can't inject markup.
(function () {
    "use strict";

    var SEL = ".dashboard-ribbon__seg[data-tip-time]";
    var tip;

    function el(cls, text) {
        var node = document.createElement("div");
        node.className = cls;
        if (text != null) node.textContent = text;
        return node;
    }

    function ensureTip() {
        if (!tip) {
            tip = document.createElement("div");
            tip.className = "sm-ribbon-tip";
            tip.setAttribute("role", "presentation");
            tip.hidden = true;
            document.body.appendChild(tip);
        }
        return tip;
    }

    function sameLocalDay(a, b) {
        return a.getFullYear() === b.getFullYear()
            && a.getMonth() === b.getMonth()
            && a.getDate() === b.getDate();
    }

    // Bucket window in the visitor's timezone from the ISO bucket bounds;
    // multi-day ranges (the ribbon carries data-tip-date) include the date so
    // thirty "12:00" cells stay distinguishable. Falls back to the server's
    // UTC data-tip-time when the ISO attrs or Intl are missing.
    function tipLabel(seg) {
        var iso = seg.getAttribute("data-tip-ts");
        var fmt = window.smLocalFmt;
        if (iso && fmt) {
            var from = new Date(iso);
            if (!isNaN(from.getTime())) {
                var withDate = !!seg.closest("[data-tip-date]");
                var fromTime = fmt.time(from);
                var start = withDate ? fmt.dayTime(from) : fromTime;
                var to = new Date(seg.getAttribute("data-tip-to") || "");
                if (isNaN(to.getTime()) || to.getTime() <= from.getTime()) return start;
                var endTime = fmt.time(to);
                // A last cell clamped to now moments after it began.
                if (to.getTime() - from.getTime() < 60000 && endTime === fromTime) return start;
                return start + " – " + (withDate && !sameLocalDay(from, to) ? fmt.dayTime(to) : endTime);
            }
        }
        return seg.getAttribute("data-tip-time") || "";
    }
    window.smRibbonTipLabel = tipLabel;

    // "≈ 12 min" of impact from the failing share of the bucket's wall span.
    function downtimeLabel(seg, total, bad) {
        var from = new Date(seg.getAttribute("data-tip-ts") || "");
        var to = new Date(seg.getAttribute("data-tip-to") || "");
        if (isNaN(from.getTime()) || isNaN(to.getTime()) || total === 0) return null;
        var secs = Math.round(((to.getTime() - from.getTime()) / 1000) * (bad / total));
        if (secs < 1) return null;
        if (secs < 90) return "≈ " + secs + " s";
        if (secs < 5400) return "≈ " + Math.round(secs / 60) + " min";
        return "≈ " + (secs / 3600).toFixed(1) + " h";
    }

    function severity(seg) {
        if (seg.classList.contains("dashboard-ribbon__seg--op")) return "ok";
        if (seg.classList.contains("dashboard-ribbon__seg--deg")) return "warn";
        if (seg.classList.contains("dashboard-ribbon__seg--maj")) return "bad";
        return "none";
    }

    function kv(body, key, value, valueCls) {
        body.appendChild(el("sm-ribbon-tip__key", key));
        body.appendChild(el("sm-ribbon-tip__val" + (valueCls ? " " + valueCls : ""), value));
    }

    function build(seg) {
        var t = ensureTip();
        t.textContent = "";
        // Severity gutter on the card mirrors the hovered cell's colour —
        // the same inset-edge register as .result-row--bad / .flow-step--*.
        var sev = severity(seg);
        t.className = "sm-ribbon-tip sm-ribbon-tip--" + sev;

        var head = el("sm-ribbon-tip__head");
        head.appendChild(el("sm-ribbon-tip__time", tipLabel(seg)));
        head.appendChild(el("sm-ribbon-tip__stat sm-ribbon-tip__stat--" + sev,
            seg.getAttribute("data-tip-stat") || ""));
        t.appendChild(head);

        // Per-bucket check counts; the fleet rail adds its monitor list below.
        if (seg.hasAttribute("data-total")) {
            var total = parseInt(seg.getAttribute("data-total") || "0", 10);
            var bad = parseInt(seg.getAttribute("data-bad") || "0", 10);
            var body = el("sm-ribbon-tip__body");
            if (total === 0) {
                kv(body, "checks", "none recorded");
            } else if (bad === 0) {
                kv(body, "checks", total.toLocaleString() + " passing");
            } else {
                kv(body, "checks",
                    bad.toLocaleString() + " of " + total.toLocaleString() + " failing",
                    "sm-ribbon-tip__val--bad");
                // A failing share of a whole fleet's checks is no one's downtime.
                var dt = seg.closest("[data-tip-fleet]") ? null : downtimeLabel(seg, total, bad);
                if (dt) kv(body, "impact", dt);
            }
            if (seg.hasAttribute("data-ribbon-drill")) {
                body.appendChild(el("sm-ribbon-tip__hint cli-brackets", "click to inspect checks"));
            }
            t.appendChild(body);
        }

        var count = parseInt(seg.getAttribute("data-tip-count") || "0", 10);
        if (count > 0) {
            t.appendChild(el("sm-ribbon-tip__count", seg.getAttribute("data-tip-who") || ""));

            var names = seg.querySelectorAll(".ribbon-seg-names > i");
            if (names.length) {
                var list = document.createElement("ul");
                list.className = "sm-ribbon-tip__list";
                for (var i = 0; i < names.length; i++) {
                    var li = document.createElement("li");
                    li.className = "sm-ribbon-tip__name";
                    li.textContent = names[i].textContent;
                    li.title = names[i].textContent;
                    list.appendChild(li);
                }
                t.appendChild(list);
            }
            if (count > names.length) {
                t.appendChild(el("sm-ribbon-tip__more", "+" + (count - names.length) + " more"));
            }
        }
        return t;
    }

    function show(seg) {
        var t = build(seg);
        if (window.smPositionFloating) {
            window.smPositionFloating(seg, t, { gap: 6 });
        } else {
            t.hidden = false;
        }
    }

    function hide() {
        if (tip) tip.hidden = true;
    }

    function segOf(e) {
        return e.target && e.target.closest ? e.target.closest(SEL) : null;
    }

    document.addEventListener("pointerover", function (e) {
        var seg = segOf(e);
        if (seg) show(seg);
    });
    document.addEventListener("pointerout", function (e) {
        if (segOf(e)) hide();
    });
    // Keyboard parity for the focusable drill cells.
    document.addEventListener("focusin", function (e) {
        var seg = segOf(e);
        if (seg) show(seg);
    });
    document.addEventListener("focusout", hide);
    // The 5s swap replaces the cell node mid-hover; re-anchor to whatever cell
    // is under the pointer now (or hide if none) so the tip doesn't blink out.
    if (document.body) {
        document.body.addEventListener("htmx:afterSettle", function () {
            var hovered = document.querySelector(SEL + ":hover");
            if (hovered) show(hovered);
            else hide();
        });
    }
})();
