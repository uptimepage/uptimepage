// Group picker. Radio chips of existing groups write the hidden group_name
// carrier (the only submitted field); the + button adds a new group; clicking
// the already-active chip clears back to ungrouped.

(function () {
    const container = document.querySelector("[data-group-chips]");
    const hidden = document.querySelector("[data-group-value]");
    if (!container || !hidden) return;
    const addBtn = container.querySelector("[data-group-add]");
    const newInput = container.querySelector("[data-group-new]");

    function chips() {
        return Array.from(container.querySelectorAll("[data-group-pick]"));
    }
    // Joining a group a status page shows publishes the monitor there, and
    // leaving one takes it off, so the form says so before the save does it.
    const warning = document.querySelector("[data-group-warning]");
    let pagesByGroup = {};
    try { pagesByGroup = JSON.parse(hidden.dataset.groupPages || "{}"); } catch { pagesByGroup = {}; }
    const plural = (n) => `${n} status page${n === 1 ? "" : "s"}`;
    function warn(value) {
        if (!warning) return;
        const initial = hidden.dataset.groupInitial || "";
        if (value === initial) {
            warning.hidden = true;
            return;
        }
        const name = (document.querySelector('[name="name"]')?.value || "").trim() || "this monitor";
        const lines = [];
        const left = pagesByGroup[initial] || 0;
        if (left) lines.push(`Leaving ${initial} takes this monitor off ${plural(left)}, with its page name and detail link there.`);
        const joined = pagesByGroup[value] || 0;
        if (joined) lines.push(`${value} is on ${plural(joined)}; this monitor will show there as ${name}.`);
        warning.textContent = lines.join(" ");
        warning.hidden = lines.length === 0;
    }
    function setGroup(value) {
        hidden.value = value;
        warn(value);
        hidden.dispatchEvent(new Event("change", { bubbles: true }));
    }

    function addGroup(raw) {
        const value = (raw || "").trim();
        if (!value) return;
        const lower = value.toLowerCase();
        let chip = chips().find(c => c.value.toLowerCase() === lower);
        if (!chip) {
            const label = document.createElement("label");
            label.className = "scope-token";
            chip = document.createElement("input");
            chip.type = "radio";
            chip.name = "__group_pick";
            chip.setAttribute("data-group-pick", "");
            chip.value = value;
            chip.className = "sr-only";
            label.appendChild(chip);
            label.appendChild(document.createTextNode(" " + value));
            container.insertBefore(label, newInput);
        }
        chip.checked = true;
        setGroup(chip.value);
    }

    function closeInput() {
        if (!newInput) return;
        newInput.value = "";
        newInput.classList.add("hidden");
    }

    // A radio is already selected when the press lands → treat the click as a
    // toggle-off. Snapshot on pointerdown, before the browser flips it.
    let toggleOff = false;
    container.addEventListener("mousedown", (evt) => {
        const label = evt.target.closest(".scope-token");
        const r = label && label.querySelector("[data-group-pick]");
        toggleOff = !!(r && r.checked);
    });
    container.addEventListener("change", (evt) => {
        const r = evt.target.closest("[data-group-pick]");
        if (r && r.checked) setGroup(r.value);
    });
    container.addEventListener("click", (evt) => {
        const label = evt.target.closest(".scope-token");
        const r = label && label.querySelector("[data-group-pick]");
        if (r && toggleOff) {
            r.checked = false;
            setGroup("");
        }
        toggleOff = false;
    });
    // Keyboard parity for clear: Space/Enter on the already-active chip
    // deselects it (a radio can't otherwise be unchecked from the keyboard).
    container.addEventListener("keydown", (evt) => {
        if (evt.key !== " " && evt.key !== "Enter") return;
        const r = evt.target.closest("[data-group-pick]");
        if (r && hidden.value.trim().toLowerCase() === r.value.toLowerCase()) {
            evt.preventDefault();
            r.checked = false;
            setGroup("");
        }
    });

    if (addBtn && newInput) {
        addBtn.addEventListener("click", () => {
            newInput.classList.remove("hidden");
            newInput.focus();
        });
        newInput.addEventListener("keydown", (evt) => {
            if (evt.key === "Enter" || evt.key === ",") {
                evt.preventDefault();
                addGroup(newInput.value);
                closeInput();
            } else if (evt.key === "Escape") {
                closeInput();
            }
        });
        newInput.addEventListener("blur", () => {
            addGroup(newInput.value);
            closeInput();
        });
    }
})();
