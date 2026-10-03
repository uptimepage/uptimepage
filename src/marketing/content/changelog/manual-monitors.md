+++
title = "Manual monitors for services no probe can check"
date = "2026-10-03"
summary = "A new monitor type with no probe: you set it to up, degraded or down, and it opens incidents, pages and shows on status pages like any other."
+++

Some services can only be judged by the people running them: a SIP trunk, a carrier interconnect, a partner's back office. A manual monitor puts them on your status page next to everything else. It has no probe, and its state is whatever someone last set.

**Set the state.** A manual monitor starts up. Set it to up, degraded or down from the monitor page, with `PUT /api/v1/targets/{id}/state`, or with the MCP `set_monitor_state` tool. An optional note of one line, up to 200 characters, becomes the incident's cause, so write it for whoever gets paged.

**Like any other monitor.** Down or degraded opens an incident within about 30 seconds and pages its channels, unless a maintenance window holds paging. Status pages show a major outage or degraded performance, and up closes the incident. There is no confirmation count to wait out, because the person who set the state already confirmed it. Downtime counts against uptime, and the monitor counts toward your monitor limit.

**Who and when.** The monitor page shows when the current state was set and which member set it. Every change is recorded with who made it, and setting the same state with the same note again changes nothing. A paused manual monitor keeps the state you set and reports it once you enable it.

**Access.** Reading the state needs `targets:read` and setting it needs `targets:write`, so existing tokens and MCP connections with those scopes can use it now.

**Terraform.** Provider 0.13.0 declares one as `check = { type = "manual" }` on `uptimepage_target`, with `interval = 60`. The state is not configuration, so a set in the app never shows as drift.

**For every monitor.** An incident that opened as degraded is now raised to an outage once its monitor is confirmed down, so its status pages show an outage instead of degraded performance. It used to keep the impact it opened with.

Docs: [manual monitors](/docs/monitor-types#manual), [REST API](/docs/api), [MCP server](/docs/mcp), [Terraform](/docs/terraform).
