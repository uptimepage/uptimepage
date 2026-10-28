+++
title = "Monitor groups that alert as one"
date = "2026-10-28"
summary = "A group of monitors can page as one: one incident and one alert for many failures, its own channels, and a live section on your status page."
+++

**One incident for many failures.** Turn on alert as one for a group, and the first member failure opens one group incident that pages the group's channels or escalation policy. Members that fail later join it, and their own channels stay quiet. A DNS move that breaks the NS and A records sends one page, one set of reminders and one recovery.

**A provider outage stays readable.** The first message names every member that is down. Later failures arrive in at most one message a minute, so a hundred members failing together send one or two messages. Turn alert again off to keep later failures on the incident's timeline only.

**Groups get their own settings.** A group used to be a label. It now has a settings page: rename it, pick its channels, reminder, escalation policy (Team plan) and tags, see each member's own channels marked muted, or delete it. Open it from the ⋯ menu on the group's header in the monitors list. Switching alert as one on or off changes only failures that start afterwards.

**On a status page.** Add a group to a page as a live section: a heading with the members' combined state, and each member as its own row. Monitors that join or leave the group join or leave the page.

**API, MCP and Terraform.** The REST API has `/api/v1/groups` and section endpoints on status pages. The MCP server adds `list_monitor_groups` and `update_monitor_group`. Terraform provider 0.19.0 adds `uptimepage_monitor_group` and `uptimepage_status_page_section`, and `group_name` keeps working.

Docs: [monitor groups](/docs/groups).
