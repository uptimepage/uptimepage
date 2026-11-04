+++
title = "Status page time zone for subscriber emails"
date = "2026-11-04"
summary = "Each status page can now set a time zone, and its maintenance and outage emails show times in that zone instead of UTC."
+++

**Times your subscribers can read.** An email has no browser to ask, so subscriber emails always showed UTC. Each status page now has a **Time zone** setting in the page editor, under Language. Maintenance and outage emails show their times in that zone, with the zone named: `20 Aug 2026 03:00 CEST`. A window that crosses a clock change names each side correctly.

A zone with no common abbreviation shows its offset instead, such as `UTC-3`. German pages use the German names, such as `MESZ`.

Pages stay on UTC until you pick a zone. The page itself still shows each visitor their own time, and webhook subscribers still get UTC timestamps.

**Everywhere you manage a page.** Set `public_timezone` with `PATCH /api/v1/status-pages/{id}`, `timezone` on the `update_status_page` MCP tool, or `timezone` on `uptimepage_status_page` with Terraform provider 0.20.0 and later.

Docs: [time zone](/docs/public-status#time-zone).
