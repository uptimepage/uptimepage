// Cross-browser <select> replacement. Native <option> elements are
// OS-controlled and unstyleable in Safari/Firefox, so the visible chrome
// is rebuilt from <button>+<ul role="listbox"> and the original <select>
// stays in DOM as the hidden form value carrier. Selecting an item
// writes select.value and dispatches a "change" event so HTMX wiring on
// the parent form picks it up unchanged.
//
// Opt in: `<select data-sm-combobox …>`. Idempotent — safe to re-scan
// after HTMX swaps. Keyboard: ArrowDown/Up navigate, Enter selects,
// Escape closes, A-Z type-ahead jumps to the next matching option.

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

        function rebuildOptions() {
            panel.textContent = "";
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
            position();
            const sel = panel.querySelector('[aria-selected="true"]') || panel.firstElementChild;
            setCursor(sel);
        }
        function moveCursor(delta) {
            const items = Array.from(panel.children);
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
        trigger.addEventListener("keydown", e => {
            if (e.key === "ArrowDown" || e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                if (openApi !== api) open();
                else moveCursor(1);
            } else if (e.key === "ArrowUp") {
                e.preventDefault();
                if (openApi !== api) open();
                else moveCursor(-1);
            }
        });
        window.smPreventPanelBlur(panel);
        panel.addEventListener("click", e => {
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
            root, trigger, panel, select,
            open, close, moveCursor, commit, position,
        };
    }

    document.addEventListener("keydown", e => {
        if (!openApi) return;
        switch (e.key) {
            case "Escape":
                e.preventDefault();
                openApi.close();
                openApi.trigger.focus();
                return;
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
            case "Tab":
                openApi.close();
                return;
        }
        if (e.key.length === 1 && /[a-z0-9]/i.test(e.key)) {
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
            if (s._smComboboxObserver) {
                s._smComboboxObserver.disconnect();
                s._smComboboxObserver = null;
            }
        }
    }
    window.smInitComboboxes = scan;
    document.addEventListener("DOMContentLoaded", scan);
    document.body.addEventListener("htmx:afterSwap", scan);
    document.body.addEventListener("htmx:afterSettle", scan);
    document.body.addEventListener("htmx:beforeCleanupElement", e => cleanup(e.detail.elt));
    if (document.readyState !== "loading") scan();
})();
