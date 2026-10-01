// ⌘K palette. Monitors + pages lazy-load once on first open and filter
// client-side; go/action rows are static. Native <dialog> = focus-trap + ESC.
(function () {
    let dialog, input, results;
    let index = null;
    let rows = [];
    let selected = 0;

    const STATIC = [
        { group: "go", label: "dashboard", href: "/", meta: "g d", icon: "i-grid" },
        { group: "go", label: "monitors", href: "/targets", meta: "g m", icon: "i-pulse" },
        { group: "go", label: "incidents", href: "/incidents", meta: "g i", icon: "i-alert" },
        { group: "go", label: "maintenance", href: "/maintenance", meta: "g w", icon: "i-calendar" },
        { group: "go", label: "status pages", href: "/settings/pages", meta: "g p", icon: "i-www" },
        { group: "go", label: "notifications", href: "/settings/notifications", meta: "g n", icon: "i-mail" },
        { group: "go", label: "variables", href: "/settings/variables", meta: "g v", icon: "i-variable" },
        { group: "go", label: "team", href: "/settings/team", meta: "g t", icon: "i-users" },
        { group: "go", label: "usage & billing", href: "/settings/usage", meta: "g u", icon: "i-gauge" },
        { group: "go", label: "api tokens", href: "/settings/api-tokens", meta: "", icon: "i-key" },
        { group: "go", label: "account", href: "/settings/account", meta: "g a", icon: "i-user" },
        { group: "action", label: "+ new monitor", href: "/targets/new", meta: "" },
    ];

    // Only where the server mounted /help; a row to a 404 is worse than none.
    if (document.body.hasAttribute("data-support-ui")) {
        STATIC.push({ group: "go", label: "get help", href: "/help", meta: "", icon: "i-help" });
    }

    function mount() {
        if (dialog) return;
        dialog = document.createElement("dialog");
        dialog.id = "sm-command-palette";
        dialog.innerHTML =
            '<div class="cmdk__promptrow">' +
            '<span class="cmdk__prompt" aria-hidden="true">❯</span>' +
            '<input class="cmdk__input" type="text" role="combobox" aria-expanded="true"' +
            ' aria-controls="cmdk-results" aria-label="Search monitors, pages, and sections"' +
            ' placeholder="jump to a monitor, page, or section…" autocomplete="off" spellcheck="false">' +
            "</div>" +
            '<div class="cmdk__results" id="cmdk-results" role="listbox"></div>';
        document.body.appendChild(dialog);
        input = dialog.querySelector(".cmdk__input");
        results = dialog.querySelector(".cmdk__results");

        input.addEventListener("input", () => render(input.value));
        input.addEventListener("keydown", onKey);
        dialog.addEventListener("click", (e) => {
            if (e.target === dialog) dialog.close();
        });
        dialog.addEventListener("close", () => {
            input.value = "";
        });
        results.addEventListener("click", (e) => {
            const el = e.target.closest("[data-cmdk-idx]");
            if (el) go(rows[+el.dataset.cmdkIdx]);
        });
    }

    async function load() {
        if (index) return;
        index = []; // set before await: a failed fetch must not re-hammer each open
        const headers = { Accept: "application/json", "X-Requested-With": "uptimepage" };
        try {
            const [t, p] = await Promise.all([
                fetch("/api/v1/targets?limit=10000", { headers }).then((r) => (r.ok ? r.json() : null)),
                fetch("/api/v1/status-pages", { headers }).then((r) => (r.ok ? r.json() : null)),
            ]);
            if (t && Array.isArray(t.items)) {
                for (const m of t.items) {
                    const meta = (m.check && (m.check.url || m.check.type)) || "";
                    index.push({ group: "monitors", label: m.name, meta, href: "/targets/" + m.id });
                }
            }
            if (Array.isArray(p)) {
                for (const s of p) {
                    index.push({ group: "pages", label: s.name, meta: s.slug || "", href: "/settings/pages/" + s.id });
                }
            }
        } catch {
            /* keep static-only */
        }
    }

    const GROUPS = ["monitors", "pages", "go", "action"];
    const GROUP_LABEL = { monitors: "monitors", pages: "status pages", go: "go to", action: "actions" };

    function render(query) {
        const q = query.trim().toLowerCase();
        const all = STATIC.concat(index || []);
        const matched = q
            ? all.filter(
                  (r) =>
                      r.label.toLowerCase().includes(q) ||
                      (r.meta && r.meta.toLowerCase().includes(q)),
              )
            : all;

        rows = [];
        let html = "";
        for (const g of GROUPS) {
            const inGroup = matched.filter((r) => r.group === g);
            if (!inGroup.length) continue;
            html += '<div class="cmdk__group">' + GROUP_LABEL[g] + "</div>";
            for (const r of inGroup) {
                const i = rows.push(r) - 1;
                const icon = r.icon ? '<svg class="mp-icon" aria-hidden="true"><use href="#' + r.icon + '"></use></svg>' : "";
                const meta = r.meta ? '<span class="cmdk__meta">' + esc(r.meta) + "</span>" : "";
                html +=
                    '<div class="cmdk__item" role="option" id="cmdk-opt-' + i + '" data-cmdk-idx="' + i + '">' +
                    icon + '<span class="cmdk__label">' + esc(r.label) + "</span>" + meta + "</div>";
            }
        }
        results.innerHTML = html || '<div class="cmdk__empty">no matches</div>';
        selected = 0;
        mark();
    }

    function mark() {
        const els = results.querySelectorAll(".cmdk__item");
        els.forEach((el, i) => {
            const on = i === selected;
            el.setAttribute("aria-selected", on ? "true" : "false");
            if (on) el.scrollIntoView({ block: "nearest" });
        });
        if (els.length) input.setAttribute("aria-activedescendant", "cmdk-opt-" + selected);
        else input.removeAttribute("aria-activedescendant");
    }

    function onKey(e) {
        if (e.key === "ArrowDown") {
            e.preventDefault();
            if (rows.length) selected = (selected + 1) % rows.length;
            mark();
        } else if (e.key === "ArrowUp") {
            e.preventDefault();
            if (rows.length) selected = (selected - 1 + rows.length) % rows.length;
            mark();
        } else if (e.key === "Enter") {
            e.preventDefault();
            if (rows[selected]) go(rows[selected]);
        }
    }

    function go(row) {
        if (!row) return;
        dialog.close();
        window.location.assign(row.href);
    }

    function esc(s) {
        return String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
    }

    async function open() {
        // Don't stack over another modal (confirm/prompt/share).
        if (document.querySelector("dialog[open]")) return;
        mount();
        render("");
        dialog.showModal();
        input.focus();
        await load();
        render(input.value);
    }
    window.smOpenPalette = open;

    document.addEventListener("keydown", (e) => {
        if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
            e.preventDefault();
            open();
        }
    });
    document.addEventListener("click", (e) => {
        if (e.target.closest("[data-cmdk-open]")) {
            e.preventDefault();
            open();
        }
    });
})();
