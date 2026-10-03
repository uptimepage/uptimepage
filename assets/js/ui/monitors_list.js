// Monitors page: bulk-select, group-collapse persistence, '/' hotkey,
// per-row ⋯ popover menu. Vanilla JS, event-delegated so handlers
// survive #target-rows swaps.
(function () {
  const ROOT = document;
  const STORAGE_KEY = "sm:monitors:groups:collapsed";

  function persistedCollapsed() {
    try { return JSON.parse(localStorage.getItem(STORAGE_KEY)) || {}; }
    catch { return {}; }
  }
  function saveCollapsed(state) {
    try { localStorage.setItem(STORAGE_KEY, JSON.stringify(state)); }
    catch { /* private mode — non-fatal */ }
  }
  function wireGroupPersistence() {
    const state = persistedCollapsed();
    ROOT.querySelectorAll("details.monitors-group").forEach(d => {
      const key = d.dataset.groupName || "";
      if (state[key]) d.removeAttribute("open");
      d.addEventListener("toggle", () => {
        const s = persistedCollapsed();
        if (d.open) delete s[key]; else s[key] = true;
        saveCollapsed(s);
      });
    });
  }

  function selectedIds() {
    return Array.from(ROOT.querySelectorAll("[data-row-select]:checked"))
      .map(cb => cb.closest("[data-row-id]").dataset.rowId);
  }
  function refreshBulkBar() {
    const bar = ROOT.getElementById("monitors-bulk");
    if (!bar) return;
    const count = selectedIds().length;
    const label = bar.querySelector("[data-bulk-count]");
    if (label) label.textContent = count;
    bar.dataset.empty = count === 0 ? "true" : "false";
    ROOT.querySelectorAll("[data-row-id]").forEach(row => {
      const cb = row.querySelector("[data-row-select]");
      row.dataset.selected = (cb && cb.checked) ? "true" : "false";
    });
  }
  function wireBulkSelect() {
    ROOT.addEventListener("change", e => {
      const t = e.target;
      if (t.matches("[data-row-select]")) refreshBulkBar();
      else if (t.matches("[data-group-select]")) {
        const tbl = t.closest("details").querySelector(".monitors-table");
        tbl.querySelectorAll("[data-row-select]").forEach(cb => { cb.checked = t.checked; });
        refreshBulkBar();
      }
    });
    ROOT.addEventListener("click", e => {
      const clear = e.target.closest("[data-bulk-clear]");
      if (clear) {
        ROOT.querySelectorAll("[data-row-select]:checked").forEach(cb => { cb.checked = false; });
        refreshBulkBar();
        return;
      }
      const action = e.target.closest("[data-bulk-action], [data-bulk-dialog]");
      if (action) {
        e.preventDefault();
        runBulkAction(action);
      }
    });
  }

  // Resolves the dialog's FormData on apply, null on cancel. A `problem` the
  // check names keeps the dialog open.
  function askDialog(dialog, count, check) {
    const form = dialog.querySelector("form");
    form.reset();
    dialog.querySelectorAll("[data-bulk-dialog-count]").forEach(el => { el.textContent = monitors(count); });
    return new Promise(resolve => {
      let result = null;
      // Every way out (Esc, backdrop, cancel, apply) ends in `close`.
      dialog.onclose = () => resolve(result);
      form.onsubmit = e => {
        e.preventDefault();
        const data = new FormData(form);
        const problem = check(data);
        if (problem) {
          window.smToast({ message: problem, kind: "warn" });
          return;
        }
        result = data;
        dialog.close();
      };
      dialog.querySelector("[data-bulk-dialog-cancel]").onclick = () => dialog.close();
      dialog.onclick = e => { if (e.target === dialog) dialog.close(); };
      dialog.showModal();
    });
  }

  function monitors(n) {
    return n + (n === 1 ? " monitor" : " monitors");
  }

  function intervalProblem(data) {
    return data.has("interval") ? null : "Pick an interval.";
  }

  // Only replace may be applied with no channel ticked.
  function channelProblem(data) {
    return data.get("mode") === "set_channels" || data.has("channel_id")
      ? null
      : "Pick at least one channel.";
  }

  async function intervalAction(dialog, count) {
    const data = await askDialog(dialog, count, intervalProblem);
    return data && { type: "set_interval", interval: Number(data.get("interval")) };
  }

  async function channelAction(dialog, count) {
    const data = await askDialog(dialog, count, channelProblem);
    if (!data) return null;
    const type = data.get("mode");
    const channelIds = data.getAll("channel_id");
    if (type !== "set_channels") return { type, channel_ids: channelIds };
    const ok = await window.smConfirm(channelIds.length > 0
      ? {
        title: "Replace channels?",
        body: monitors(count) + " keep only the ticked channels; every other bound channel is removed.",
        confirmLabel: "Replace",
        danger: true,
      }
      : {
        title: "Unbind every channel?",
        body: monitors(count) + " lose every bound channel. Only a channel whose tag rule covers a monitor will still notify it.",
        confirmLabel: "Unbind all",
        danger: true,
      });
    return ok ? { type, channel_ids: channelIds } : null;
  }

  const DIALOG_ACTIONS = { "bulk-interval": intervalAction, "bulk-channels": channelAction };

  function reportOutcome(outcome, quiet) {
    const done = outcome.succeeded.length;
    if (outcome.failed.length === 0) {
      if (!quiet) window.smToast({ message: "Updated " + monitors(done) + ".", kind: "ok" });
      return;
    }
    const reasons = new Map();
    outcome.failed.forEach(f => reasons.set(f.message, (reasons.get(f.message) || 0) + 1));
    const why = Array.from(reasons, ([message, n]) => message + " (" + n + ")").join("; ");
    window.smToast({
      message: "Applied to " + monitors(done) + ", skipped " + outcome.failed.length + ": " + why + ".",
      kind: "warn",
    });
  }

  async function runBulkAction(btn) {
    const bar = ROOT.getElementById("monitors-bulk");
    if (!bar) return;
    const ids = selectedIds();
    if (ids.length === 0) return;
    const dialog = btn.dataset.bulkDialog && ROOT.getElementById(btn.dataset.bulkDialog);
    let body;
    if (dialog) {
      const action = await DIALOG_ACTIONS[dialog.id](dialog, ids.length);
      if (!action) return;
      body = { ids, action };
    } else {
      const action = btn.dataset.bulkAction;
      if (btn.dataset.bulkConfirm) {
        const ok = await window.smConfirm({
          title: "Bulk action",
          body: btn.dataset.bulkConfirm,
          confirmLabel: action === "delete" ? "Delete" : "Continue",
          danger: action === "delete",
        });
        if (!ok) return;
      }
      body = { ids, action: { type: action } };
      if (btn.dataset.bulkPrompt) {
        const input = await window.smPrompt({
          title: "Bulk action",
          body: btn.dataset.bulkPrompt,
          splitOnComma: btn.dataset.bulkPromptSplit !== undefined,
        });
        if (input === null) return;
        body.action[btn.dataset.bulkPromptKey] = input;
      }
    }
    const url = bar.dataset.actionUrl || "/api/v1/targets/bulk-action";
    try {
      const r = await fetch(url, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "Accept": "application/json",
          "X-Requested-With": "uptimepage",
        },
        body: JSON.stringify(body),
      });
      const payload = await r.json().catch(() => null);
      if (!r.ok) {
        const reason = payload && payload.error && payload.error.message;
        window.smToast({ message: reason || "Bulk action failed: " + r.status });
        return;
      }
      if (payload) reportOutcome(payload, !dialog);
      // Neither the interval nor the channels show in a row, so a dialog
      // action keeps the selection for the next one instead of refreshing.
      if (dialog) return;
      const refresh = bar.dataset.refreshUrl;
      const rows = ROOT.getElementById("target-rows");
      if (window.htmx && refresh && rows) {
        window.htmx.ajax("GET", refresh, { target: "#target-rows", swap: "outerHTML" });
      } else {
        window.location.reload();
      }
    } catch (err) {
      window.smToast({ message: "Network error: " + err.message });
    }
  }

  // `position: fixed` escapes the group-card's `overflow: hidden`.
  let openMenu = null;
  function closeOpenMenu() {
    if (!openMenu) return;
    const root = openMenu;
    openMenu = null;
    root.dataset.open = "false";
    const panel = root.querySelector("[data-menu-panel]");
    const btn = root.querySelector("[data-menu-toggle]");
    if (panel) panel.hidden = true;
    if (btn) btn.setAttribute("aria-expanded", "false");
  }
  function positionMenu(root) {
    const btn = root.querySelector("[data-menu-toggle]");
    const panel = root.querySelector("[data-menu-panel]");
    if (!btn || !panel) return;
    window.smPositionFloating(btn, panel, { align: "end" });
  }
  function openMenuFor(root) {
    if (openMenu === root) {
      closeOpenMenu();
      return;
    }
    closeOpenMenu();
    openMenu = root;
    root.dataset.open = "true";
    const btn = root.querySelector("[data-menu-toggle]");
    if (btn) btn.setAttribute("aria-expanded", "true");
    positionMenu(root);
  }
  function wireRowMenu() {
    ROOT.addEventListener("click", e => {
      const toggle = e.target.closest("[data-menu-toggle]");
      if (toggle) {
        e.preventDefault();
        const root = toggle.closest("[data-menu-root]");
        if (root) openMenuFor(root);
        return;
      }
      const action = e.target.closest("[data-row-action]");
      if (action && openMenu && openMenu.contains(action)) {
        e.preventDefault();
        runRowAction(action);
        return;
      }
      if (openMenu && !openMenu.contains(e.target)) closeOpenMenu();
    });
    ROOT.addEventListener("keydown", e => {
      if (e.key === "Escape" && openMenu) {
        closeOpenMenu();
        const btn = e.target.closest("[data-menu-root]")?.querySelector("[data-menu-toggle]");
        if (btn) btn.focus();
      }
    });
    window.addEventListener("scroll", () => { if (openMenu) positionMenu(openMenu); }, true);
    window.addEventListener("resize", () => { if (openMenu) positionMenu(openMenu); });
  }
  async function runRowAction(btn) {
    const id = btn.dataset.targetId;
    const action = btn.dataset.rowAction;
    if (!id || !action) return;
    if (btn.dataset.rowConfirm) {
      const ok = await window.smConfirm({
        title: action === "delete" ? "Delete monitor?" : "Confirm",
        body: btn.dataset.rowConfirm,
        confirmLabel: action === "delete" ? "Delete" : "Continue",
        danger: action === "delete",
      });
      if (!ok) return;
    }
    closeOpenMenu();
    const bar = ROOT.getElementById("monitors-bulk");
    const url = (bar && bar.dataset.actionUrl) || "/api/v1/targets/bulk-action";
    try {
      const r = await fetch(url, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "Accept": "application/json",
          "X-Requested-With": "uptimepage",
        },
        body: JSON.stringify({ ids: [id], action: { type: action } }),
      });
      if (!r.ok) {
        window.smToast({ message: "Action failed: " + r.status });
        return;
      }
      const refresh = bar && bar.dataset.refreshUrl;
      const rows = ROOT.getElementById("target-rows");
      if (window.htmx && refresh && rows) {
        window.htmx.ajax("GET", refresh, { target: "#target-rows", swap: "outerHTML" });
      } else {
        window.location.reload();
      }
    } catch (err) {
      window.smToast({ message: "Network error: " + err.message });
    }
  }

  function wireSearchHotkey() {
    ROOT.addEventListener("keydown", e => {
      if (e.key !== "/" || e.metaKey || e.ctrlKey || e.altKey) return;
      const t = e.target;
      if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
      const search = ROOT.getElementById("monitors-search");
      if (search) {
        e.preventDefault();
        search.focus();
        search.select();
      }
    });
  }

  function init() {
    wireGroupPersistence();
    wireBulkSelect();
    wireRowMenu();
    wireSearchHotkey();
    refreshBulkBar();
  }

  if (window.htmx) {
    document.body.addEventListener("htmx:afterSwap", e => {
      if (e.detail && e.detail.target && e.detail.target.id === "target-rows") {
        closeOpenMenu();
        wireGroupPersistence();
        refreshBulkBar();
      }
    });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
