+++
title = "Maintenance updates, and finished maintenance in the history"
date = "2026-10-15"
summary = "Post updates to a maintenance window, see maintenance days on the 90-day strip, and keep finished windows in the status page history."
+++

**Status updates.** A maintenance window now takes updates, like an incident: a reminder before it starts, a line when the work runs late, how it went once it ends. Post one from the window's new page in the console, through `POST /api/v1/maintenance/{id}/updates`, or with the MCP tool `post_maintenance_update`. The status page shows the newest one under the window, and email and webhook subscribers get each one (`type` `maintenance_update`). A completed window still takes updates, so you can add a closing note.

**Maintenance days.** On the 90-day strip, a day a window touched shows as maintenance, and time inside a window no longer counts against the uptime figure. An outage outside the window on the same day still counts. Hover over a maintenance day, or tap it, to open the window in the archive. A monitor added to a running window counts from when it was added, and one dropped from it counts until it was dropped.

**Finished maintenance in the history.** Ended windows now sit in the page history next to past incidents, newest first, with **All**, **Incidents** and **Maintenance** tabs on the page and in the 90-day archive. A row opens to the window's description and every update. The history section shows on a quiet page too, and always links the archive. `GET /api/public/v1/maintenance/past` lists ended windows, and `/api/public/v1/status` carries `recent_maintenance`.

**A page keeps what it showed.** An incident or a started window stays in the page history, under the name the page gave the monitor, when that monitor is removed from the page, deleted, or dropped from a running window.

**Upcoming windows.** The page lists windows up to 30 days ahead, and every window time names its zone.

**Date and time picker.** A window's start and end, and an on-call handoff, are now picked in the console's own date and time picker, with a 24-hour clock.

**Updates post once.** The update routes for incidents and for maintenance accept an `Idempotency-Key`, and the console sends one, so a retried or double-clicked update is posted once.

Docs: [scheduling maintenance](/docs/public-status#scheduling-maintenance), [status updates](/docs/public-status#status-updates), [what the page shows](/docs/public-status#what-the-public-page-renders), [idempotency](/docs/api#idempotency). Guide: [how to announce scheduled maintenance](/blog/scheduled-maintenance-status-page).
