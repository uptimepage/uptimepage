# Monitor groups

A group gathers monitors that belong together, such as every DNS record of one domain or every endpoint of one service. By default a group is a label: the monitors list files its members under one header showing the worst member's state, and each monitor still opens its own incident and pages on its own. Turn on **alert as one** and the group pages as a single thing. The first member failure opens one group incident, later failures join it, and the members' own channels stay quiet.

## Making a group

A monitor joins a group from the **Group** field on its form, or several at once with **Move to group…** in the monitors list's selection bar. Typing a name that does not exist yet creates the group; names are trimmed, at most 50 characters, and unique in the org. A monitor sits in at most one group. A group whose last monitor left stays listed with "0 monitors" until you delete it.

Open a group's settings from the ⋯ menu on its header in the monitors list. There you rename it, choose how it alerts, pick its channels, reminder, escalation policy (Team plan) and tags, and delete it. The page shows each member with its own channels and policy, marked muted while the group alerts as one.

An account holds as many groups as its plan allows monitors, counted across its organizations. Creating one past that is `422 QUOTA_EXCEEDED` with `quota: max_groups`.

## Label only or alert as one

| | Label only | Alert as one |
|---|---|---|
| Incident | one per failing monitor | one group incident, with each member's own incident attached to it |
| Who is paged | each monitor's channels and escalation policy | the group's channels and escalation policy |
| Reminders | each monitor's interval | the group's interval |
| Tags shown and matched by channel rules | the monitor's own | the group's |

Switching changes only failures that start afterwards. An incident open at the moment of the switch ends the way it started, and the switch itself never opens, closes or sends anything. The console asks before it saves a switch for that reason.

While the group alerts as one, a member's form shows its channels, escalation policy and tags greyed, with a note pointing to the group. They are kept, and come back when the monitor leaves the group or the group stops alerting as one. A channel with a tag rule follows the group's tags, so the group's channel picker marks a channel those tags already reach "by tag".

The group's state is down when any member is down, degraded when a member is degraded and none is down, and up when every member is up. Each member still confirms its own failure first, with its own failing checks, regions and recovery period.

## How a group incident runs

The first member failure while the group alerts as one opens the group incident and pages the group's channels, or its escalation policy where one applies. The message names every member down when it goes out: "example.com DNS is down: NS, A and 12 more".

**Another member down.** With **alert again** on, the default, members that fail later are announced in at most one message a minute, naming every member that failed since the last one: "MX and 85 more are down too". A provider-wide outage across a hundred members sends one or two messages, not a hundred. With it off, a later failure only adds a line to the incident's timeline. Either way an acknowledgement stays and the escalation ladder does not restart.

**Reminders and escalation** run on the group's settings while the group incident stays unacknowledged, and name every member still down. When every member still down is paused or held by a maintenance window, reminders and escalation wait, as they would for a paused monitor.

**Recovery.** The group incident closes when the last member incident it holds closes, and the recovery goes to the channels the incident reached.

Each member still opens its own incident, so its uptime, its incident history and the status pages that show it work as before. Attached to the group incident, it pages nobody, reminds nobody and sends no recovery of its own. The incidents list shows the group incident with its members nested under it, and a member incident links to the group incident it is part of.

### Members that change during an outage

- **Held by maintenance.** A member inside a window that holds alerts does not count for the group until the window ends; then its failure joins the group incident, or opens one. A window that only announces work changes nothing.
- **Paused.** Its failure stays in the group incident, which cannot close until the member resumes and recovers.
- **Moved out of the group.** Its current failure stays in the group incident until it recovers. Its next failure alerts on its own.
- **Deleted.** It leaves the incident with a timeline line. If it was the last one down, the group incident closes with a monitor-deleted note, not a recovery.

## Acting on a group incident

Acknowledge and resolve the group incident, not its members. Resolving it resolves its open member incidents too; a member still failing opens a fresh incident on its next check, as a resolved monitor incident does. On a member incident, acknowledge, resolve and reopen are refused with the group incident's id (`409 USE_GROUP_INCIDENT`). A group incident cannot be reopened (`409 GROUP_INCIDENT_NO_REOPEN`): it follows its members, so nothing would close it again, and the next member failure opens a new one. The buttons on an alert give the same answers.

A group incident stays internal and cannot be published (`409 GROUP_INCIDENT_INTERNAL`). Status pages show the members, so publish a member's incident and post updates to it, as for any monitor.

Incident metrics count the group incident once: MTTA, MTTR and incident counts take the group incident and skip the member incidents attached to it.

## Monitoring stopped notices

When the probes covering a member go silent, the "monitoring stopped" notice goes to the group's channels while the group alerts as one. Members that go silent together share one notice: "Monitoring stopped for NS, A and 12 more". A member that goes silent while that notice stands joins it quietly, and "monitoring resumed" goes out once, when every one of them is back, naming them all.

## Deleting a group

Deleting a group keeps its monitors; they become ungrouped. An open group incident closes with a note, not a recovery, and each member still down alerts on its own channels from then on, so the outage is not lost: "Group deleted while NS and A are still down; they now alert on their own." The closed incident keeps the group's name, shown as "name (deleted)". The group also leaves every status page that shows it as a section, taking its members' rows with it.

## On a status page

A group can be shown on a status page as a live section: a heading with the members' combined state, and each member as its own row with its own public name, order and detail link. The section mirrors the group, so a monitor that joins the group appears on the page and one that leaves it is taken off. This works for label groups too.

Add one in the page editor under **monitor groups**, with **+ add group**. The heading defaults to the group's name; set your own if the group's name is operator-facing. A member already on the page by hand moves into the section with its settings. Members past the plan's public component limit stay off the page, and the editor says how many. They stay off when room frees up later: remove the group from the page and add it again.

A section member cannot be taken off the page on its own (`409 SECTION_MEMBER`): take the monitor out of the group, or remove the section. Joining or leaving a group that a page shows changes that page, so the monitor form warns first: picking such a group says the monitor will appear there and under what name, and leaving one says it comes off those pages with its page name and detail link.

## API, MCP and Terraform

- REST: `/api/v1/groups` lists, creates, changes and deletes groups, and `/api/v1/groups/{id}/escalation-policy` sets a group's policy on the Team plan. A monitor joins by `group_id` or by `group_name`. Sections are `/api/v1/status-pages/{id}/sections`. See [REST API](api.md#monitor-groups).
- MCP: `list_monitor_groups` reads groups and `update_monitor_group` changes one. See [MCP server](mcp.md#write-tools).
- Terraform: provider 0.19.0 adds the `uptimepage_monitor_group` and `uptimepage_status_page_section` resources and the `uptimepage_monitor_group` data source, and a monitor joins by `group_id` or `group_name`. See [Terraform](terraform.md#monitor-groups).
