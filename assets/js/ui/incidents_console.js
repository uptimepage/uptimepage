// Incident console actions: acknowledge / resolve / reopen / add-note /
// post-update / declare.
// Posts to /api/v1/incidents/* (same-origin session cookie + X-Requested-With,
// the custom header that gates state-changing requests). Reloads on success so
// the row/detail reflects the new state.
(function () {
  "use strict";
  // The dashboard banner that hosts this script lives inside a partial htmx
  // re-swaps every 5s, which re-executes the tag. Bind the delegated listeners
  // exactly once or each refresh stacks another → N duplicate POSTs per click.
  if (window.__smIncidentsConsoleInit) return;
  window.__smIncidentsConsoleInit = true;
  const root = document;

  function showError(msg) {
    const b = root.querySelector("[data-incident-error]");
    if (b) {
      b.textContent = msg;
      b.classList.remove("hidden");
    }
    if (window.smToast) window.smToast({ message: msg, kind: "error" });
  }

  async function post(url, body) {
    return request("POST", url, body);
  }

  async function request(method, url, body) {
    const r = await fetch(url, {
      method: method,
      headers: {
        "Content-Type": "application/json",
        Accept: "application/json",
        "X-Requested-With": "uptimepage",
      },
      body: JSON.stringify(body || {}),
    });
    let json = null;
    try {
      json = await r.json();
    } catch (_e) {
      /* empty body */
    }
    return { ok: r.ok, status: r.status, json: json };
  }

  function errMsg(res) {
    const e = (res.json && res.json.error) || {};
    const code = e.code ? e.code + ": " : "";
    return code + (e.message || "request failed (" + res.status + ")");
  }

  async function runAction(id, action) {
    if (!id || !action) return;
    const verb = { acknowledge: "Acknowledge", resolve: "Resolve", reopen: "Reopen" }[action];
    if (!verb) return;
    const body = {};
    // Ack/resolve capture an optional note (the "why"). Resolving a public
    // incident also posts this as the public "Resolved" update, so its prompt
    // says so. reopen just confirms. Cancel aborts; an empty note proceeds.
    if (action !== "reopen" && window.smPrompt) {
      const promptBody =
        action === "resolve"
          ? "This message is posted as the public “Resolved” update on the status page (for public incidents). Leave blank for a default closing message."
          : "Add an optional note for the timeline — why, or what you found.";
      const note = await window.smPrompt({
        title: verb + " incident?",
        body: promptBody,
        placeholder: action === "resolve" ? "resolution message…" : "optional note…",
        multiline: true,
        optional: true,
      });
      // null = cancelled (abort); "" = OK with no note (proceed).
      if (note === null || note === undefined) return;
      const trimmed = note.toString().trim();
      if (trimmed) body.note = trimmed;
    } else if (window.smConfirm) {
      const ok = await window.smConfirm({ title: verb + " incident?", body: "", confirmLabel: verb });
      if (!ok) return;
    }
    const res = await post("/api/v1/incidents/" + encodeURIComponent(id) + "/" + action, body);
    if (!res.ok) return showError(errMsg(res));
    if (window.smToast) window.smToast({ message: verb + "d", kind: "ok" });
    window.location.reload();
  }

  async function assignSelf(el) {
    const id = el.dataset.incidentId;
    const holder = el.closest("[data-self-user-id]");
    const selfId = holder && holder.getAttribute("data-self-user-id");
    if (!id || !selfId) return;
    const res = await post("/api/v1/incidents/" + encodeURIComponent(id) + "/assign", { user_id: selfId });
    if (!res.ok) return showError(errMsg(res));
    if (window.smToast) window.smToast({ message: "Assigned to you", kind: "ok" });
    window.location.reload();
  }

  async function assignTo(select) {
    const id = select.dataset.incidentId;
    if (!id) return;
    const val = (select.value || "").trim();
    const res = await post("/api/v1/incidents/" + encodeURIComponent(id) + "/assign", {
      user_id: val || null,
    });
    if (!res.ok) return showError(errMsg(res));
    if (window.smToast) window.smToast({ message: val ? "Assigned" : "Unassigned", kind: "ok" });
    window.location.reload();
  }

  async function addNote(id) {
    if (!id || !window.smPrompt) return;
    const msg = await window.smPrompt({
      title: "Add note",
      body: "Recorded on the incident's internal timeline.",
      placeholder: "what's happening…",
      multiline: true,
    });
    if (!msg) return;
    const res = await post("/api/v1/incidents/" + encodeURIComponent(id) + "/notes", { message: msg });
    if (!res.ok) return showError(errMsg(res));
    window.location.reload();
  }

  // Checked page ids, or null where the incident has a monitor and so no
  // picker: its pages are the ones carrying that monitor.
  function pickedPages(scope) {
    const picker = scope.querySelector("[data-incident-pages]");
    if (!picker || picker.hidden) return null;
    return Array.from(picker.querySelectorAll("[data-incident-page]:checked")).map((c) => c.value);
  }

  function syncAllPages(picker) {
    const all = picker.querySelector("[data-incident-page-all]");
    if (!all) return;
    const boxes = picker.querySelectorAll("[data-incident-page]");
    all.checked = boxes.length > 0 && Array.from(boxes).every((b) => b.checked);
  }

  async function savePages(id) {
    const pages = pickedPages(root);
    if (!id || !pages) return;
    const res = await post("/api/v1/incidents/" + encodeURIComponent(id) + "/publish", {
      status_page_ids: pages,
    });
    if (!res.ok) return showError(errMsg(res));
    if (window.smToast) window.smToast({ message: "Pages saved", kind: "ok" });
    window.location.reload();
  }

  async function publish(id) {
    if (!id) return;
    const pages = pickedPages(root);
    let title = "";
    if (window.smPrompt) {
      const r = await window.smPrompt({
        title: "Publish to status page?",
        body:
          (pages
            ? "Shows this incident on the status pages picked on this page."
            : "Shows this incident on any status page that lists its monitor.") +
          " Optional public title:",
        placeholder: "leave blank to keep the current title",
        optional: true,
      });
      // null = cancelled (abort); "" = OK with no title (publish, keep title).
      if (r === null || r === undefined) return;
      title = r.toString().trim();
    } else if (window.smConfirm) {
      const ok = await window.smConfirm({ title: "Publish incident?", body: "", confirmLabel: "Publish" });
      if (!ok) return;
    }
    const body = title ? { public_title: title } : {};
    if (pages) body.status_page_ids = pages;
    const res = await post("/api/v1/incidents/" + encodeURIComponent(id) + "/publish", body);
    if (!res.ok) return showError(errMsg(res));
    if (window.smToast) window.smToast({ message: "Published", kind: "ok" });
    window.location.reload();
  }

  async function unpublish(id) {
    if (!id) return;
    if (window.smConfirm) {
      const ok = await window.smConfirm({
        title: "Unpublish incident?",
        body: "Removes it from public status pages. Internal records are kept.",
        confirmLabel: "Unpublish",
      });
      if (!ok) return;
    }
    const res = await post("/api/v1/incidents/" + encodeURIComponent(id) + "/unpublish", {});
    if (!res.ok) return showError(errMsg(res));
    if (window.smToast) window.smToast({ message: "Unpublished", kind: "ok" });
    window.location.reload();
  }

  async function submitUpdate(form) {
    const id = form.dataset.incidentId;
    if (!id) return;
    const fd = new FormData(form);
    const body = {
      phase: (fd.get("phase") || "investigating").toString(),
      message: (fd.get("message") || "").toString().trim(),
    };
    if (!body.message) return showError("message: cannot be empty");
    const res = await post("/api/v1/incidents/" + encodeURIComponent(id) + "/updates", body);
    if (!res.ok) return showError(errMsg(res));
    if (window.smToast) window.smToast({ message: "Update posted", kind: "ok" });
    window.location.reload();
  }

  // Blank fields send null (clear) rather than "", so the public page falls
  // back to generated wording instead of rendering an empty title.
  async function submitEdit(form) {
    const id = form.dataset.incidentId;
    if (!id) return;
    const fd = new FormData(form);
    const text = (k) => (fd.get(k) || "").toString().trim() || null;
    const res = await request("PATCH", "/api/v1/incidents/" + encodeURIComponent(id), {
      title: text("title"),
      severity: (fd.get("severity") || "major").toString(),
      urgency: (fd.get("urgency") || "high").toString(),
      public_title: text("public_title"),
      public_description: text("public_description"),
      ...(fd.has("counts_as_downtime")
        ? { counts_as_downtime: (fd.get("counts_as_downtime") || "0").toString() === "1" }
        : {}),
    });
    if (!res.ok) return showError(errMsg(res));
    if (window.smToast) window.smToast({ message: "Saved", kind: "ok" });
    window.location.href = "/incidents/" + encodeURIComponent(id);
  }

  async function submitDeclare(form) {
    const fd = new FormData(form);
    const tid = (fd.get("target_id") || "").toString().trim();
    const body = {
      title: (fd.get("title") || "").toString().trim(),
      severity: (fd.get("severity") || "major").toString(),
      urgency: (fd.get("urgency") || "high").toString(),
      visibility: (fd.get("visibility") || "internal").toString(),
      notify: (fd.get("notify") || "0").toString() === "1",
      counts_as_downtime: (fd.get("counts_as_downtime") || "0").toString() === "1",
    };
    if (tid) body.target_id = tid;
    const pages = pickedPages(form);
    if (pages) body.status_page_ids = pages;
    const res = await post("/api/v1/incidents", body);
    if (!res.ok) return showError(errMsg(res));
    const id = res.json && res.json.id;
    window.location.href = id ? "/incidents/" + id : "/incidents";
  }

  root.addEventListener("click", function (ev) {
    const act = ev.target.closest("[data-incident-action]");
    if (act) {
      ev.preventDefault();
      return runAction(act.dataset.incidentId, act.dataset.incidentAction);
    }
    const assign = ev.target.closest("[data-incident-assign-self]");
    if (assign) {
      ev.preventDefault();
      return assignSelf(assign);
    }
    const note = ev.target.closest("[data-incident-note]");
    if (note) {
      ev.preventDefault();
      return addNote(note.dataset.incidentId);
    }
    const save = ev.target.closest("[data-incident-save-pages]");
    if (save) {
      ev.preventDefault();
      return savePages(save.dataset.incidentId);
    }
    const pub = ev.target.closest("[data-incident-publish]");
    if (pub) {
      ev.preventDefault();
      return publish(pub.dataset.incidentId);
    }
    const unpub = ev.target.closest("[data-incident-unpublish]");
    if (unpub) {
      ev.preventDefault();
      return unpublish(unpub.dataset.incidentId);
    }
  });

  root.addEventListener("change", function (ev) {
    const sel = ev.target.closest("[data-incident-assign-select]");
    if (sel) return assignTo(sel);
    const all = ev.target.closest("[data-incident-page-all]");
    if (all) {
      const picker = all.closest("[data-incident-pages]");
      picker.querySelectorAll("[data-incident-page]").forEach((b) => {
        b.checked = all.checked;
      });
      return;
    }
    const one = ev.target.closest("[data-incident-page]");
    if (one) return syncAllPages(one.closest("[data-incident-pages]"));
    const target = ev.target.closest("[data-incident-declare-form] select[name=target_id]");
    if (target) {
      const picker = target.form.querySelector("[data-incident-pages]");
      if (picker) picker.hidden = !!target.value;
    }
  });

  root.querySelectorAll("[data-incident-pages]").forEach(syncAllPages);

  root.addEventListener("submit", function (ev) {
    const declare = ev.target.closest("[data-incident-declare-form]");
    if (declare) {
      ev.preventDefault();
      return submitDeclare(declare);
    }
    const update = ev.target.closest("[data-incident-update-form]");
    if (update) {
      ev.preventDefault();
      return submitUpdate(update);
    }
    const edit = ev.target.closest("[data-incident-edit-form]");
    if (edit) {
      ev.preventDefault();
      return submitEdit(edit);
    }
  });

  // ⌘/Ctrl+Enter submits, as on the monitor and channel forms.
  root.addEventListener("keydown", function (e) {
    if (e.key !== "Enter" || !(e.metaKey || e.ctrlKey)) return;
    const form =
      e.target.closest &&
      e.target.closest("[data-incident-declare-form], [data-incident-edit-form]");
    if (!form) return;
    e.preventDefault();
    form.requestSubmit();
  });

  // `/` focuses the incidents search, unless already typing in a field.
  root.addEventListener("keydown", function (e) {
    if (e.key !== "/" || e.metaKey || e.ctrlKey || e.altKey) return;
    const t = e.target;
    if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
    const search = root.getElementById("incidents-search");
    if (search) {
      e.preventDefault();
      search.focus();
      search.select();
    }
  });
})();
