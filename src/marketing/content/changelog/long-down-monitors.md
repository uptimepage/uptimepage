+++
title = "Monitors down for days are checked every 15 minutes"
date = "2026-10-21"
summary = "A monitor down from every region for 3 days is checked every 15 minutes until it answers, and its owner gets one email asking if it is still needed."
+++

**Checked less often after 3 days down.** A monitor that has failed every check from every region for 3 days, with its incident open as long, is now checked every 15 minutes from each region instead of on its own interval. It is usually a test server that was switched off or a site that moved, and probing it every minute only confirms the same failure.

**Back to full speed when it recovers.** It goes back to its own interval when its incident closes or a maintenance window covering it starts, and at once when you edit, pause or resume it. After a fix, run a check from its page: a passing result puts it back straight away.

**The outage stays on record.** The incident stays open, uptime counts the whole stretch as down, and status pages are unchanged. The stored interval never changes, so Terraform sees no drift.

**One email, and a mark in the app.** The monitor's owner, or the organization's first owner when it names none, gets one email asking whether they still need it, with a link to its page to pause or delete it. The monitor page shows the slower rate and since when, the monitors list marks the row, and the API and MCP tools return `slowed_at`.

Docs: [long-down monitors](/docs/monitor-types#long-down-monitors).
