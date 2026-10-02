+++
title = "Schedule maintenance windows from your AI assistant"
date = "2026-10-02"
summary = "Five MCP tools let an assistant list, schedule, edit, end and cancel maintenance windows, naming the covered monitors before it acts."
+++

The MCP server can now manage maintenance windows, the same ones the console and the API schedule.

**Read.** `list_maintenance` lists windows by status (active, upcoming or past), and `get_maintenance` reads one, with the monitors it covers and whether it holds their paging.

**Schedule.** `create_maintenance` takes a title, a start and an end, and the monitors the work touches. Paging for those monitors is held while the window runs unless you turn that off, and the checks keep running, so it is the gentler choice than pausing a monitor. The confirmation names the monitors, says whether paging is held, and says which of them sit on a published status page, because that page's subscribers are told about the window.

**Edit, end, cancel.** `update_maintenance` changes an upcoming or running window and shows the old and new value of each field. Its `end_now` flag ends a running window at once. `cancel_maintenance` withdraws a window that has not ended and keeps it under past as a record. A completed window is history and is refused.

**Access.** The read tools need `maintenance:read`, now part of the read only grant. The write tools need `maintenance:write` or `maintenance:delete`, which come with the Manage monitors level on the consent screen. A connection keeps the scopes it was granted, so reconnect to add these.

Docs: [MCP server](/docs/mcp), [scheduling maintenance](/docs/public-status#scheduling-maintenance).
