+++
title = "Schedule maintenance windows from the console"
date = "2026-10-01"
summary = "A Maintenance page to schedule, edit, end and cancel planned work, with paging held for the monitors a window covers."
+++

Maintenance windows used to be API only. The console now has a Maintenance page for them.

**Schedule.** Pick the monitors, a start and an end in your own timezone, and an optional description. Readers see the window on your status page, and subscribers get a notice when you schedule it and another when it ends. A monitor that is not on a status page is flagged in the picker, because readers cannot see it there.

**Hold paging.** On by default. A monitor inside a running window still opens incidents, but its channels stay quiet, and an incident still open when the window ends pages then. Turn it off to keep paging live. An incident you declare by hand always pages.

**Edit, end, cancel.** An upcoming window can be edited or cancelled. A running one can be edited or ended now. Cancelled windows stay under Past with who cancelled them and when, and every change is written to the organization audit log.

**Quota.** Only windows that have not ended count toward the limit. Completed and cancelled ones are history.

API: a `PATCH` that sets `ends_at` to now or earlier on a running window ends it at the server's clock, `status=upcoming` now lists the soonest first, and a window can no longer be created or edited to end in the past otherwise.

Docs: [scheduling maintenance](/docs/public-status#scheduling-maintenance), [REST API](/docs/api).
