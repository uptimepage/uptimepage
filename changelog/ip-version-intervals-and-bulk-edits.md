+++
title = "Recently shipped: IPv4 or IPv6, longer intervals, bulk edits"
date = "2026-10-19"
summary = "Pin a monitor to IPv4 or IPv6, check every 6 to 24 hours, edit many monitors at once, see where a failed check went, and resolve from an alert."
+++

**IPv4 or IPv6.** HTTP, TCP and certificate checks raced IPv6 against IPv4 and kept whichever connected, so a broken AAAA record or an IPv6 firewall stayed hidden while IPv4 worked. A monitor can now be pinned to one of them. It asks DNS for that family's addresses only and dials nothing else, and a host with no address of that family fails the check instead of falling back to the other. To watch both, create one monitor for each. Set it on the monitor form, as `ip_version` through the API, or in Terraform provider 0.17.0.

**Longer intervals.** The monitor form offers 6, 12 and 24 hours for HTTP, TCP, ping, DNS and flow monitors. The API and Terraform already took them.

**Bulk edits.** Select monitors in the list to change their check interval or their notification channels in one go: add a channel, remove one, or replace them all. A monitor that cannot take the interval keeps its own, and the reply says which and why.

**Where a failed check went.** After a hostname moves, resolvers keep the old record until it expires, so probes keep reaching the old server and the failure looks like a fault on the new one. A failed HTTP check now shows the server address that answered, the addresses it tried, and what the probe's resolver answers for the host and for how long. An incident's page lists the addresses behind its failed checks with the regions that reached each. Share links, status pages and notifications never show them.

**Resolve from an alert.** A channel can carry a **Resolve** button next to Acknowledge, off until you turn it on. It shows on every channel that carries Acknowledge except ntfy, and a press closes the incident only for a signed-in member, or in a chat app for an account a member linked, so the incident always closes under a name.

**Incidents without a monitor reach their pages.** A declared incident with no monitor said it was published but showed on no page. It now names the status pages it goes on, from the declare form, the incident page, the API and MCP.

**Deleting a monitor keeps its incidents.** Deleting a monitor used to delete its incidents and the alerts sent for them. They now stay, under the monitor's name. An incident the monitor opened closes with a note that monitoring was removed, and the channels it paged are told; one a person declared stays open for someone to resolve.

Docs: [IP version](/docs/monitor-types#ip-version), [intervals](/docs/monitor-types#intervals), [acknowledging and resolving](/docs/notifications#acknowledging-from-the-notification), [publishing an incident](/docs/incidents#publishing-to-a-status-page).
