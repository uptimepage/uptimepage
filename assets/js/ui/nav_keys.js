// Go-to shortcuts: press `g`, then a section key, to jump. A hint bar floats
// up while `g` is armed. Skips typing contexts and modifier chords so it never
// fights ⌘K, form input, or an open modal.
(function () {
    const MAP = {
        d: "/",
        m: "/targets",
        i: "/incidents",
        p: "/settings/pages",
        n: "/settings/notifications",
        v: "/settings/variables",
        t: "/settings/team",
        u: "/settings/usage",
        a: "/settings/account",
    };
    const HINTS = [
        ["g d", "dashboard"],
        ["g m", "monitors"],
        ["g i", "incidents"],
        ["g p", "pages"],
        ["g n", "notifications"],
        ["g v", "variables"],
        ["g t", "team"],
        ["g u", "usage"],
        ["g a", "account"],
    ];

    let armed = false;
    let timer = null;
    let hint = null;

    function typing() {
        const el = document.activeElement;
        return (
            el &&
            (el.tagName === "INPUT" ||
                el.tagName === "TEXTAREA" ||
                el.tagName === "SELECT" ||
                el.isContentEditable)
        );
    }

    function showHint() {
        if (!hint) {
            hint = document.createElement("div");
            hint.className = "nav-keys-hint";
            hint.setAttribute("role", "status");
            hint.innerHTML = HINTS.map(
                ([k, v]) => "<span><kbd>" + k + "</kbd>" + v + "</span>",
            ).join("");
            document.body.appendChild(hint);
        }
        hint.hidden = false;
    }

    function disarm() {
        armed = false;
        clearTimeout(timer);
        if (hint) hint.hidden = true;
    }

    document.addEventListener("keydown", (e) => {
        if (e.defaultPrevented || e.metaKey || e.ctrlKey || e.altKey) return;
        if (typing() || document.querySelector("dialog[open]")) {
            disarm();
            return;
        }
        if (!armed) {
            if (e.key === "g") {
                armed = true;
                showHint();
                clearTimeout(timer);
                timer = setTimeout(disarm, 2000);
            }
            return;
        }
        const dest = MAP[e.key.toLowerCase()];
        disarm();
        if (dest) {
            e.preventDefault();
            window.location.assign(dest);
        }
    });
})();
