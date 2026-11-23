// Variable helpers for the monitor form. Two affordances, both driven off the
// org's variable list (keys only; secret values never leave the server):
//
//   1. Insert menu: typing `{{` in an interpolable field (HTTP url, header
//      value, body, body-assertion, or a flow step's fill value or assertion)
//      opens a filtered list of variable keys; picking one completes the
//      `{{key}}` token.
//   2. Auth picker: pick a secret variable and a scheme to drop a prefilled
//      `Authorization: Bearer {{key}}` / `x-api-key: {{key}}` header row. The
//      stored credential stays a reference; the secret resolves at probe time.
//
// Fields where a secret is rejected by the resolver (url, assertion) grey out
// secret entries so the footgun is caught before save, not at 422.

(function () {
    const form = document.getElementById("check-form");
    if (!form) return;

    // name -> whether a secret value is allowed there (header value + body only).
    const FIELDS = {
        http_url: false,
        http_header_value: true,
        http_body: true,
        http_expected_body_contains: false,
    };
    // Flow step fields, matched by attribute since client-built rows carry no
    // name. A secret belongs in a fill value, never in an assertion.
    const FLOW_VALUE = "[data-flow-value]";
    const FLOW_CONTAINS = "[data-flow-contains]";
    const SELECTOR = Object.keys(FIELDS)
        .map((n) => `[name="${n}"]`)
        .concat(FLOW_VALUE, FLOW_CONTAINS)
        .join(", ");

    let vars = [];

    fetch("/api/v1/variables", { headers: { "X-Requested-With": "uptimepage" } })
        .then((r) => (r.ok ? r.json() : []))
        .then((list) => {
            vars = Array.isArray(list)
                ? list.map((v) => ({ key: v.key, secret: !!v.is_secret }))
                : [];
            vars.sort((a, b) => a.key.localeCompare(b.key));
            initAuthPicker();
        })
        .catch(() => {});

    function initAuthPicker() {
        const picker = form.querySelector("[data-var-auth-picker]");
        if (!picker) return;
        const select = picker.querySelector("[data-var-auth-select]");
        const scheme = picker.querySelector("[data-var-auth-scheme]");
        const addBtn = picker.querySelector("[data-var-auth-add]");
        const secrets = vars.filter((v) => v.secret);
        if (secrets.length === 0) return;

        select.innerHTML = "";
        for (const v of secrets) {
            const opt = document.createElement("option");
            opt.value = v.key;
            opt.textContent = v.key;
            select.appendChild(opt);
        }
        picker.hidden = false;

        addBtn.addEventListener("click", () => {
            const key = select.value;
            if (!key) return;
            const ref = "{{" + key + "}}";
            const [name, value] =
                scheme.value === "apikey"
                    ? ["x-api-key", ref]
                    : ["Authorization", "Bearer " + ref];
            if (typeof window.smAddHeaderRow === "function") {
                window.smAddHeaderRow(name, value);
                form.dispatchEvent(new Event("input", { bubbles: true }));
            }
        });
    }

    const menu = document.createElement("ul");
    menu.dataset.varMenu = "";
    menu.setAttribute("role", "listbox");
    menu.hidden = true;
    menu.className =
        "fixed z-50 max-h-56 overflow-auto rounded border border-[color:var(--theme-line)] " +
        "bg-[color:var(--theme-surface-elev)] py-1 font-mono text-xs shadow-lg";
    document.body.appendChild(menu);

    let activeField = null;
    let tokenStart = -1; // byte offset of the opening `{{` being completed
    let items = [];
    let highlight = -1;

    const OPEN = /\{\{\s*([A-Za-z0-9_]*)$/;
    const MENU_MIN_WIDTH = 160;
    const GUTTER = 8;

    function fieldAllowsSecret(el) {
        if (el.matches(FLOW_VALUE)) return true;
        return FIELDS[el.getAttribute("name")] === true;
    }

    function closeMenu() {
        menu.hidden = true;
        activeField = null;
        tokenStart = -1;
        items = [];
        highlight = -1;
    }

    function renderMenu() {
        menu.innerHTML = "";
        items.forEach((it, i) => {
            const li = document.createElement("li");
            li.setAttribute("role", "option");
            li.dataset.idx = String(i);
            li.className =
                "cursor-pointer px-2 py-1 " +
                (i === highlight ? "bg-[color:var(--theme-surface-sunk)] " : "") +
                (it.disabled ? "text-quiet" : "text-body");
            li.textContent = it.key;
            if (it.secret) {
                const tag = document.createElement("span");
                tag.className = "ml-2 text-quiet";
                tag.textContent = it.disabled ? "secret · not here" : "secret";
                li.appendChild(tag);
            }
            menu.appendChild(li);
        });
    }

    function openMenuFor(el, prefix) {
        const allowSecret = fieldAllowsSecret(el);
        items = vars
            .filter((v) => v.key.startsWith(prefix))
            .map((v) => ({ key: v.key, secret: v.secret, disabled: v.secret && !allowSecret }));
        if (items.length === 0) {
            closeMenu();
            return;
        }
        activeField = el;
        highlight = items.findIndex((it) => !it.disabled);
        renderMenu();
        const r = el.getBoundingClientRect();
        menu.style.top = Math.round(r.bottom + 4) + "px";
        // Fit the longest key: a fraction of the field clips it.
        const edge = document.documentElement.clientWidth - GUTTER;
        menu.style.width = "max-content";
        menu.style.minWidth = Math.round(Math.max(MENU_MIN_WIDTH, r.width / 2)) + "px";
        menu.style.maxWidth = Math.round(Math.max(MENU_MIN_WIDTH, edge - r.left)) + "px";
        menu.hidden = false;
        // offsetWidth needs the menu shown, so this runs after.
        const left = Math.min(r.left, edge - menu.offsetWidth);
        menu.style.left = Math.round(Math.max(GUTTER, left)) + "px";
    }

    function maybeOpen(el) {
        if (!el.matches || !el.matches(SELECTOR)) return;
        const caret = el.selectionStart;
        if (caret == null) return;
        const before = el.value.slice(0, caret);
        const m = before.match(OPEN);
        if (!m) {
            closeMenu();
            return;
        }
        tokenStart = caret - m[0].length;
        openMenuFor(el, m[1]);
    }

    function choose(idx) {
        if (!activeField || idx < 0 || idx >= items.length) return;
        const it = items[idx];
        if (it.disabled) return;
        const el = activeField;
        const caret = el.selectionStart;
        const before = el.value.slice(0, tokenStart);
        const after = el.value.slice(caret);
        const token = "{{" + it.key + "}}";
        el.value = before + token + after;
        const pos = before.length + token.length;
        el.setSelectionRange(pos, pos);
        el.focus();
        closeMenu();
        el.dispatchEvent(new Event("input", { bubbles: true }));
    }

    form.addEventListener("input", (e) => maybeOpen(e.target));
    form.addEventListener("keydown", (e) => {
        if (menu.hidden || !activeField || e.target !== activeField) return;
        if (e.key === "Escape") {
            closeMenu();
        } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
            e.preventDefault();
            const dir = e.key === "ArrowDown" ? 1 : -1;
            for (let i = 0; i < items.length; i++) {
                highlight = (highlight + dir + items.length) % items.length;
                if (!items[highlight].disabled) break;
            }
            renderMenu();
        } else if (e.key === "Enter") {
            // Absorb Enter whenever the menu is open so it completes a token (or
            // dismisses the menu) instead of submitting the whole monitor form.
            e.preventDefault();
            if (highlight >= 0 && !items[highlight].disabled) choose(highlight);
            else closeMenu();
        } else if (e.key === "Tab") {
            if (highlight >= 0 && !items[highlight].disabled) {
                e.preventDefault();
                choose(highlight);
            } else {
                closeMenu();
            }
        }
    });

    menu.addEventListener("mousedown", (e) => {
        // mousedown (not click) so the field doesn't blur before we read it.
        const li = e.target.closest("[data-idx]");
        if (!li) return;
        e.preventDefault();
        choose(parseInt(li.dataset.idx, 10));
    });

    document.addEventListener("click", (e) => {
        if (activeField && e.target !== activeField && !menu.contains(e.target)) closeMenu();
    });
    form.addEventListener("focusout", (e) => {
        if (e.target === activeField) setTimeout(() => { if (document.activeElement !== activeField) closeMenu(); }, 0);
    });
})();
