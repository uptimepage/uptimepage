// Group settings page: saves the form to PATCH /api/v1/groups/{id}, asking
// first when the save turns alerting as one on or off, and deletes the group
// after a confirm that names the incident it closes. Both return to the page
// that linked here.
(function () {
    const form = document.getElementById("group-form");
    if (!form) return;
    const id = form.dataset.groupId;
    const back = form.dataset.back;
    const banner = document.getElementById("group-form-errors");
    const groupOnly = form.querySelector("[data-group-only]");
    const tagChips = form.querySelector("[data-tag-chips]");
    const headers = {
        "Content-Type": "application/json",
        "Accept": "application/json",
        "X-Requested-With": "uptimepage",
    };

    // A page restored from the back/forward cache after a save is stale.
    let saved = false;
    window.addEventListener("pageshow", (e) => {
        if (e.persisted && saved) window.location.reload();
    });

    const radio = (name) => form.querySelector(`input[name="${name}"]:checked`);
    const alertAsOne = () => radio("alert_as_one")?.value === "true";

    const nobody = form.querySelector("[data-nobody-warning]");
    // Only channels decide it here: a policy may still page, but nobody on a
    // channel hears the members, whose own stay quiet.
    const reachesNobody = () =>
        !form.querySelector("[data-channel-select]:checked") &&
        !Array.from(form.querySelectorAll("[data-rule-chip]")).some((c) => !c.hidden);
    const syncNobody = () => {
        if (nobody) nobody.hidden = !(alertAsOne() && reachesNobody());
    };

    form.addEventListener("change", (e) => {
        if (e.target.name === "alert_as_one" && groupOnly) groupOnly.hidden = !alertAsOne();
        syncNobody();
    });

    function syncRuleChips() {
        const tags = tagChips?.collectTags ? tagChips.collectTags() : [];
        for (const row of form.querySelectorAll("[data-channel-row]")) {
            const chip = row.querySelector("[data-rule-chip]");
            if (!chip) continue;
            let rule = [];
            try { rule = JSON.parse(row.dataset.ruleTags || "[]"); } catch { rule = []; }
            chip.hidden = !(window.smTagRuleCovers && window.smTagRuleCovers(rule, tags));
        }
        syncNobody();
    }
    if (tagChips) {
        tagChips.addEventListener("change", syncRuleChips);
        new MutationObserver(syncRuleChips).observe(tagChips, { childList: true });
    }
    syncRuleChips();

    function body() {
        const out = {
            name: form.querySelector('[name="name"]').value,
            alert_as_one: alertAsOne(),
            alert_again: radio("alert_again")?.value === "true",
            alerts: Array.from(form.querySelectorAll("[data-channel-select]:checked"))
                .map((cb) => ({ channel_id: cb.value })),
        };
        const renotify = radio("renotify_secs");
        if (renotify) out.renotify_interval_secs = Number(renotify.value);
        if (tagChips?.collectTags) out.tags = tagChips.collectTags();
        return out;
    }

    form.addEventListener("submit", async (e) => {
        e.preventDefault();
        window.smClearFormErrors(banner);
        const next = body();
        const was = form.dataset.alertAsOne === "true";
        if (next.alert_as_one !== was) {
            const members = Number(form.dataset.memberCount) || 0;
            const ok = await window.smConfirm({
                title: next.alert_as_one ? "Alert as one?" : "Stop alerting as one?",
                body: next.alert_as_one
                    ? `From the next failure, the ${members} monitor(s) in this group open one incident and page the group's channels; their own channels and policy stay quiet. An incident open now ends the way it started.` +
                      (reachesNobody() ? " No channel is picked, so the group alerts nobody." : "")
                    : `From the next failure, each monitor in this group alerts on its own channels again. An incident open now ends the way it started.`,
                confirmLabel: "Save",
            });
            if (!ok) return;
        }
        try {
            const res = await fetch(`/api/v1/groups/${id}`, {
                method: "PATCH",
                headers,
                body: JSON.stringify(next),
            });
            if (!res.ok) {
                window.smRenderClientError(banner, await window.smApiErrorMessage(res, "Save failed."));
                return;
            }
            saved = true;
            window.location.assign(back);
        } catch (err) {
            window.smRenderClientError(banner, "Network error: " + err.message);
        }
    });

    form.querySelector("[data-group-delete]")?.addEventListener("click", async () => {
        const members = Number(form.dataset.memberCount) || 0;
        const closing = form.dataset.openIncident
            ? " Its open incident closes with a note, not a recovery, and each member still down alerts on its own channels from then on."
            : "";
        const ok = await window.smConfirm({
            title: "Delete group?",
            body: `Its ${members} monitor(s) stay, ungrouped.${closing}`,
            confirmLabel: "Delete",
            danger: true,
        });
        if (!ok) return;
        try {
            const res = await fetch(`/api/v1/groups/${id}`, { method: "DELETE", headers });
            if (!res.ok) {
                window.smRenderClientError(banner, await window.smApiErrorMessage(res, "Delete failed."));
                return;
            }
            window.location.assign(back);
        } catch (err) {
            window.smRenderClientError(banner, "Network error: " + err.message);
        }
    });
})();
