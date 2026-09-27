// Cross-browser <select> replacement. Native <option> elements are
// OS-controlled and unstyleable in Safari/Firefox, so the visible chrome
// is rebuilt from <button>+<ul role="listbox"> and the original <select>
// stays in DOM as the hidden form value carrier. Selecting an item
// writes select.value and dispatches a "change" event so HTMX wiring on
// the parent form picks it up unchanged.
//
// Opt in: `<select data-sm-combobox …>`. Idempotent — safe to re-scan
// after HTMX swaps. Keyboard: ArrowDown/Up navigate, Enter selects,
// Escape closes, A-Z type-ahead jumps to the next matching option. A long
// list adds `data-sm-combobox-search`: the panel opens on a search box that
// filters the options as you type, in place of the type-ahead.

(function () {
    let openApi = null;
    let idCounter = 0;

    function init(select) {
        if (select.dataset.smComboboxInitialized === "1") return;
        select.dataset.smComboboxInitialized = "1";

        const panelId = "sm-cb-" + (++idCounter);

        const root = document.createElement("div");
        root.className = "sm-combobox";

        const trigger = document.createElement("button");
        trigger.type = "button";
        trigger.className = "sm-combobox__trigger";
        trigger.setAttribute("aria-haspopup", "listbox");
        trigger.setAttribute("aria-expanded", "false");
        trigger.setAttribute("aria-controls", panelId);
        const ariaLabel = select.getAttribute("aria-label");
        if (ariaLabel) trigger.setAttribute("aria-label", ariaLabel);

        const label = document.createElement("span");
        label.className = "sm-combobox__label";
        trigger.appendChild(label);

        const chev = document.createElementNS("http://www.w3.org/2000/svg", "svg");
        chev.setAttribute("class", "sm-combobox__chevron");
        chev.setAttribute("viewBox", "0 0 12 8");
        chev.setAttribute("fill", "none");
        chev.setAttribute("stroke", "currentColor");
        chev.setAttribute("stroke-width", "2");
        chev.setAttribute("stroke-linecap", "round");
        chev.setAttribute("stroke-linejoin", "round");
        chev.setAttribute("aria-hidden", "true");
        const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
        path.setAttribute("d", "M1 1.5l5 5 5-5");
        chev.appendChild(path);
        trigger.appendChild(chev);

        const panel = document.createElement("ul");
        panel.id = panelId;
        panel.className = "sm-combobox__panel";
        panel.setAttribute("role", "listbox");
        panel.hidden = true;
        if (ariaLabel) panel.setAttribute("aria-label", ariaLabel);

        let search = null;
        if (select.hasAttribute("data-sm-combobox-search")) {
            const row = document.createElement("li");
            row.className = "sm-combobox__search-row";
            row.setAttribute("role", "presentation");
            search = document.createElement("input");
            search.type = "search";
            search.className = "sm-combobox__search";
            search.placeholder = "search…";
            search.autocomplete = "off";
            search.spellcheck = false;
            search.setAttribute("aria-label", `Search ${ariaLabel || "options"}`);
            row.appendChild(search);
        }
        // Case, underscores and slashes aside, so `new york` finds
        // America/New_York.
        const fold = (t) => t.toLowerCase().replace(/[_/]+/g, " ");
        const options = () => Array.from(panel.querySelectorAll(".sm-combobox__option:not([hidden])"));

        function rebuildOptions() {
            panel.textContent = "";
            if (search) panel.appendChild(search.parentElement);
            for (let i = 0; i < select.options.length; i++) {
                const opt = select.options[i];
                const li = document.createElement("li");
                li.className = "sm-combobox__option";
                li.setAttribute("role", "option");
                li.dataset.value = opt.value;
                li.textContent = opt.text;
                if (opt.selected) li.setAttribute("aria-selected", "true");
                panel.appendChild(li);
            }
        }
        function syncLabel() {
            const opt = select.options[select.selectedIndex];
            label.textContent = opt ? opt.text : "";
        }
        // Label and listbox both follow the <select>, so a programmatic
        // change (a refused save putting the old value back) reads right.
        function syncSelection() {
            for (const li of panel.children) {
                if (li.dataset.value === select.value) li.setAttribute("aria-selected", "true");
                else li.removeAttribute("aria-selected");
            }
            syncLabel();
        }
        function reflectDisabled() {
            const off = select.disabled;
            trigger.disabled = off;
            root.classList.toggle("sm-combobox--disabled", off);
            if (off && openApi && openApi.root === root) close();
        }
        rebuildOptions();
        syncLabel();

        // Hide the real <select> visually but keep it in the form.
        select.tabIndex = -1;
        select.setAttribute("aria-hidden", "true");
        select.style.position = "absolute";
        select.style.width = "1px";
        select.style.height = "1px";
        select.style.opacity = "0";
        select.style.pointerEvents = "none";

        select.parentNode.insertBefore(root, select);
        root.appendChild(trigger);
        root.appendChild(panel);
        root.appendChild(select);
        reflectDisabled();

        function position() {
            window.smPositionFloating(trigger, panel, { minWidth: true });
        }
        function clearCursor() {
            for (const li of panel.children) li.removeAttribute("aria-current");
        }
        function setCursor(li) {
            clearCursor();
            if (li) {
                li.setAttribute("aria-current", "true");
                li.scrollIntoView({ block: "nearest" });
            }
        }
        function close() {
            if (openApi && openApi.root === root) openApi = null;
            panel.hidden = true;
            trigger.setAttribute("aria-expanded", "false");
            clearCursor();
        }
        function open() {
            if (openApi && openApi.root !== root) openApi.close();
            openApi = api;
            trigger.setAttribute("aria-expanded", "true");
            if (search) {
                search.value = "";
                filter();
            }
            position();
            const sel = panel.querySelector('[aria-selected="true"]') || options()[0];
            setCursor(sel);
            // Keys reach an open panel only through its own controls.
            (search || trigger).focus();
        }
        function filter() {
            const q = fold(search.value.trim());
            for (const li of panel.querySelectorAll(".sm-combobox__option")) {
                li.hidden = q !== "" && !fold(li.textContent).includes(q);
            }
            setCursor(options()[0]);
        }
        if (search) search.addEventListener("input", filter);
        function moveCursor(delta) {
            const items = options();
            if (!items.length) return;
            let idx = items.findIndex(it => it.getAttribute("aria-current") === "true");
            if (idx < 0) idx = items.findIndex(it => it.getAttribute("aria-selected") === "true");
            if (idx < 0) idx = delta > 0 ? -1 : items.length;
            idx = (idx + delta + items.length) % items.length;
            setCursor(items[idx]);
        }
        // Like a native <select>, re-picking the current option is not a change.
        function selectValue(value) {
            const changed = select.value !== value;
            select.value = value;
            close();
            if (changed) select.dispatchEvent(new Event("change", { bubbles: true }));
            trigger.focus();
        }
        function commit() {
            const cur = panel.querySelector('[aria-current="true"]');
            if (cur) selectValue(cur.dataset.value);
        }

        trigger.addEventListener("click", e => {
            e.preventDefault();
            if (openApi && openApi.root === root) close();
            else open();
        });
        // Opens only, and leaves Cmd/Ctrl keys to the page's shortcuts. Once
        // open, the document handler below drives the keys; its
        // defaultPrevented check keeps the key that opened the panel from
        // acting a second time.
        trigger.addEventListener("keydown", e => {
            if (openApi === api || e.metaKey || e.ctrlKey) return;
            if (["ArrowDown", "ArrowUp", "Enter", " "].includes(e.key)) {
                e.preventDefault();
                open();
            }
        });
        // Space clicks a button on keyup, after the keydown already opened
        // the panel or picked from it.
        trigger.addEventListener("keyup", e => {
            if (e.key === " ") e.preventDefault();
        });
        window.smPreventPanelBlur(panel);
        panel.addEventListener("click", e => {
            if (e.target.closest("input")) return;
            e.preventDefault();
            const li = e.target.closest(".sm-combobox__option");
            if (li) selectValue(li.dataset.value);
        });
        panel.addEventListener("mouseover", e => {
            const li = e.target.closest(".sm-combobox__option");
            if (li) setCursor(li);
        });

        // Re-render the listbox if the underlying <select> is mutated
        // externally (e.g. a partial swap replaces options). MutationObserver
        // catches added/removed <option> children; an explicit change-event
        // listener also keeps the visible label in sync if something
        // programmatic flips select.value. Observer is stashed on the select
        // so htmx:beforeCleanupElement can disconnect it before GC.
        const mo = new MutationObserver((records) => {
            let opts = false, dis = false;
            for (const r of records) {
                if (r.type === "childList") opts = true;
                else if (r.attributeName === "disabled") dis = true;
            }
            if (opts) { rebuildOptions(); syncLabel(); }
            if (dis) reflectDisabled();
        });
        mo.observe(select, { childList: true, attributes: true, attributeFilter: ["disabled"] });
        select._smComboboxObserver = mo;
        select.addEventListener("change", syncSelection);

        const api = {
            root, trigger, panel, select, search,
            open, close, moveCursor, commit, position,
        };
    }

    document.addEventListener("keydown", e => {
        if (!openApi || e.defaultPrevented || e.isComposing || !openApi.root.contains(e.target)) return;
        switch (e.key) {
            case "Escape": {
                e.preventDefault();
                const api = openApi;
                api.close();
                api.trigger.focus();
                return;
            }
            case "ArrowDown":
                e.preventDefault();
                openApi.moveCursor(1);
                return;
            case "ArrowUp":
                e.preventDefault();
                openApi.moveCursor(-1);
                return;
            case "Enter":
                e.preventDefault();
                openApi.commit();
                return;
            case " ":
                if (openApi.search) break;
                e.preventDefault();
                openApi.commit();
                return;
            case "Tab":
                openApi.close();
                return;
        }
        if (!openApi.search && !e.metaKey && !e.ctrlKey && !e.altKey
            && e.key.length === 1 && /[a-z0-9]/i.test(e.key)) {
            // The letter is the picker's, not a page shortcut's.
            e.preventDefault();
            // Type-ahead — let the document handler skip if the open combo
            // is the one initialised; ev still fires here because focus
            // sits on .sm-combobox__trigger inside openApi.root.
            const items = Array.from(openApi.panel.children);
            const lower = e.key.toLowerCase();
            let startIdx = items.findIndex(it => it.getAttribute("aria-current") === "true");
            for (let i = 1; i <= items.length; i++) {
                const idx = (startIdx + i + items.length) % items.length;
                const text = (items[idx].textContent || "").trim().toLowerCase();
                if (text.startsWith(lower)) {
                    for (const it of items) it.removeAttribute("aria-current");
                    items[idx].setAttribute("aria-current", "true");
                    items[idx].scrollIntoView({ block: "nearest" });
                    break;
                }
            }
        }
    });
    document.addEventListener("click", e => {
        if (!openApi) return;
        if (!openApi.root.contains(e.target)) openApi.close();
    }, true);
    window.addEventListener("scroll", () => { if (openApi) openApi.position(); }, true);
    window.addEventListener("resize", () => { if (openApi) openApi.position(); });

    function scan() {
        document.querySelectorAll("select[data-sm-combobox]").forEach(init);
    }
    function cleanup(root) {
        if (!root) return;
        const selects = root.matches && root.matches("select[data-sm-combobox]")
            ? [root]
            : (root.querySelectorAll
                ? Array.from(root.querySelectorAll("select[data-sm-combobox]"))
                : []);
        for (const s of selects) {
            if (openApi && openApi.select === s) openApi.close();
            if (s._smComboboxObserver) {
                s._smComboboxObserver.disconnect();
                s._smComboboxObserver = null;
            }
        }
    }
    window.smInitComboboxes = scan;
    window.smCleanupComboboxes = cleanup;
    document.addEventListener("DOMContentLoaded", scan);
    document.body.addEventListener("htmx:afterSwap", scan);
    document.body.addEventListener("htmx:afterSettle", scan);
    document.body.addEventListener("htmx:beforeCleanupElement", e => cleanup(e.detail.elt));
    if (document.readyState !== "loading") scan();
})();
