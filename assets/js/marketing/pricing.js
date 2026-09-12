// Monthly/yearly toggle on /pricing. Without the script the page shows the
// monthly figures, which the markup renders by default.
(() => {
    const toggle = document.querySelector("[data-cadence]");
    const grid = document.querySelector("[data-cadence-grid]");
    if (!toggle || !grid) return;
    toggle.querySelectorAll("[data-cadence-pick]").forEach((seg) => {
        seg.addEventListener("click", () => {
            const year = seg.dataset.cadencePick === "year";
            grid.classList.toggle("mk-price-grid--year", year);
            toggle.querySelectorAll("[data-cadence-pick]").forEach((b) => {
                const on = b === seg;
                b.classList.toggle("is-on", on);
                b.setAttribute("aria-pressed", String(on));
            });
        });
    });
})();
