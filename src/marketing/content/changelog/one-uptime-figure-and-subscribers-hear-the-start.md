+++
title = "One uptime figure, and subscribers hear the start"
date = "2026-09-20"
summary = "Monitors list and status pages now compute uptime the way the dashboard does, status page cards use the strip's words, and subscribers get the opening mail."
+++

**One uptime number.** The dashboard and the monitor page counted confirmed incident downtime; the monitors list counted failed checks. The same monitor read 99.99% on one screen and 99.76% on the next. The list now uses the confirmed figure too.

**Status page uptime is time-based.** It used to be days without an incident over days with data, so a four-minute outage in ten days of history read 90.00%. It now reads 99.97%: confirmed downtime over the time the component has been probed. `uptime_pct` is on the public API as well.

**Status page cards use the strip's words.** The history strip said "Partial outage" while the incident card said "Major". Cards now carry the measured impact (`Degraded`, `Partial outage`, `Major outage`) for monitor-opened incidents and the declared severity's impact for declared ones. The public API adds `impact` beside `severity`.

**Subscribers hear the outage start.** A monitor-opened incident on a status page posted only its closing update, so subscribers received one mail, "Resolved", after the fact. The writer now posts an opening update too, and the mail is titled the way the page titles the incident instead of "Status update".

**Console.** The dashboard incident count is labelled with the range you picked. The monitors list offers ping, heartbeat and flow in the type filter, its heading count follows the filters, and the URL it pushes survives a reload.
