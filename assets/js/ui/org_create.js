// /settings/organizations create form. Slug shape is the server's rule, so
// this borrows smOrgIdentity rather than growing a second copy of it.
(function () {
    const form = document.getElementById("org-create-form");
    if (!form) return;

    const name = document.getElementById("new-org-name");
    const slug = document.getElementById("new-org-slug");
    const avail = document.getElementById("new-org-slug-avail");
    const errors = document.getElementById("new-org-errors");
    const submit = form.querySelector("button[type=submit]");

    const headers = {
        "Content-Type": "application/json",
        "Accept": "application/json",
        "X-Requested-With": "uptimepage",
    };

    const showAvail = (text, ok) => {
        avail.textContent = text;
        avail.className = "font-mono text-xs " + (ok ? "flash-text--ok" : "flash-text--bad");
    };

    let checkTimer;
    const scheduleCheck = () => {
        clearTimeout(checkTimer);
        const value = slug.value.trim();
        if (!value) { avail.textContent = ""; return; }
        checkTimer = setTimeout(async () => {
            try {
                const data = await window.smOrgIdentity.checkSlug(value);
                if (data.available) showAvail("✓ available", true);
                else showAvail(`✗ ${data.reason || "unavailable"}`, false);
            } catch { avail.textContent = ""; }
        }, 300);
    };

    // Slug trails the name until the user edits it, then it is theirs.
    let slugTouched = false;
    slug.addEventListener("input", () => {
        slugTouched = true;
        scheduleCheck();
    });
    name.addEventListener("input", () => {
        if (slugTouched) return;
        slug.value = window.smOrgIdentity.slugify(name.value);
        scheduleCheck();
    });

    form.addEventListener("submit", async (ev) => {
        ev.preventDefault();
        window.smClearFormErrors(errors);
        const orgName = name.value.trim();
        const orgSlug = slug.value.trim();
        if (!orgName) {
            name.setAttribute("aria-invalid", "true");
            window.smRenderClientError(errors, "Enter an organization name.");
            return;
        }
        if (!orgSlug) {
            slug.setAttribute("aria-invalid", "true");
            window.smRenderClientError(errors, "Enter a URL slug.");
            return;
        }
        // check-slug counts tombstoned orgs, the create index does not — a
        // freed slug submits fine here, then breaks that org's restore.
        const avail_check = await window.smOrgIdentity.checkSlug(orgSlug).catch(() => null);
        if (!avail_check) {
            window.smRenderClientError(errors, "Network error — try again.");
            return;
        }
        if (!avail_check.available) {
            slug.setAttribute("aria-invalid", "true");
            window.smRenderClientError(errors, avail_check.reason || "That slug is unavailable.");
            return;
        }

        submit.disabled = true;
        try {
            const res = await fetch("/api/v1/orgs", {
                method: "POST",
                headers,
                body: JSON.stringify({ name: orgName, slug: orgSlug }),
            });
            if (res.status !== 201) {
                let data = null;
                try { data = await res.json(); } catch { /* not JSON */ }
                window.smRenderApiError(errors, data, res.status);
                submit.disabled = false;
                return;
            }
            // Creation changes the org menu even if switching fails.
            window.smNavCache?.clear();
            const org = await res.json();
            const switched = await fetch("/api/v1/me/active-org", {
                method: "POST",
                headers,
                body: JSON.stringify({ org_id: org.id }),
            });
            // The org exists either way — say so, or a retry creates a second.
            if (switched.status !== 204) {
                window.smRenderClientError(
                    errors,
                    `Created ${org.slug}, but switching into it failed — pick it from the org menu in the header.`,
                );
                submit.disabled = false;
                return;
            }
            // Re-renders against the new org; the empty list is the receipt.
            window.location.reload();
        } catch {
            window.smRenderClientError(errors, "Network error — try again.");
            submit.disabled = false;
        }
    });
})();
