+++
title = "Monitor setup explains failed checks and domain expiry"
date = "2026-11-10"
summary = "A failed check now says what it means and what to try, and the domain expiry form tells you when a registration is already watched."
+++

Two changes to setting up a monitor. A failed check now tells you what the failure most likely means and what to try, next to the error. The domain expiry form now tells you which registration a name is checked by, and when a monitor already watches it.

**Failed checks in the test sheet.** Press test now on the monitor form and each region shows its error in plain words. When every region you tested fails the same way, one highlighted line follows it. No reply to ping usually means a firewall drops ICMP, so a TCP or HTTP check on a port the service listens on tells you more. A name that resolves to a private address cannot be reached from outside. There are seven of these hints, for the failures a different check or a different target would fix. Result rows now carry their state on the pill alone, so that line is easy to find.

**Failed checks on the monitor page.** A card under the header lists what the failing regions report, with how many regions report each cause, and the same advice. It refreshes along with the rest of the page. The hint shows until the monitor first passes after you create or edit it. After that the check has shown it fits the target, and the same error means the target changed.

**When the probe recognises the answer.** If the response is something the probe can name, for example an edge network's challenge page, the error says so and the line reads recommended instead of hint, with what to change. That advice rests on what the probe saw, so it shows on established monitors too.

**Regions have to agree.** One region failing its own way, or passing, points at the network path there. Then no advice shows and the card lists the causes side by side.

**Domain expiry asks about the registered domain.** A subdomain has no registration of its own, so a domain expiry check on `app.example.com` asks the registry about `example.com`. The form says so as you type and offers the registered domain with one click, so the monitor carries the name it really checks.

**One registration, one monitor.** When another domain expiry monitor in the organization already watches that registration, the form names it and links to it instead of making the offer. Saving still works, because a second monitor with other thresholds or channels can be what you want. The "also worth watching" panel on a monitor's page follows the same rule: it no longer suggests a domain expiry check that a monitor on another name under the same registration already covers.

**Two form fixes.** Opening a link from the monitor form in a new tab no longer switches off the unsaved-changes warning. Changing a saved monitor's address no longer renames the monitor.

**API and MCP.** `POST /api/v1/targets/test` returns `error_text` and `advice` beside the result. `POST /api/v1/targets/{id}/check-now` adds `error_text` and keeps every other field where it was. The MCP server returns `hint` on a trial run and `last_hint` on `get_monitor`. The domain expiry note lives in the form only, so the API, Terraform and MCP accept the same monitors as before.

Docs: [when a new monitor fails](/docs/monitor-types#a-new-monitor-fails-but-the-service-works-for-you), [domain expiry](/docs/monitor-types#domain-expiry), [REST API](/docs/api), [MCP server](/docs/mcp).
