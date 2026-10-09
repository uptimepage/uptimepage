+++
title = "Calmer status pages, and a recovery period for flapping"
date = "2026-10-09"
summary = "Day colours follow how long a service was down, repeat blips fold into one past incident, and a monitor can wait out its recovery before closing."
+++

**Day colours follow downtime.** A day on the 90-day strip used to take the colour of the worst thing that happened, so a four-minute blip painted it as red as a lost afternoon. The colour now follows how long the component was down, weighted the way Atlassian Statuspage weighs it: a partial outage counts for 30% of its length, and degraded performance does not count as downtime. Under 20 minutes reads yellow, 20 to 60 orange, an hour or more red. Hovering a day lists the time it spent in each state.

**Uptime weighs downtime the same way.** The uptime figures on the status page, the dashboard, the monitor list and page, the uptime API and the MCP tools now count a partial outage at 30% of its length and leave degraded performance out, so the number beside a strip agrees with its colours.

**Past incidents fold.** The past-incidents list was a card per incident, so a service that flapped for an hour filled the page. Incidents on one component less than an hour apart, and incidents on different components that began together, now fold into one row that opens to the incidents inside it. An incident you posted an update to keeps its own row with your words; the lines the platform writes for you are left out. The last week is open and older incidents sit behind one toggle. The archive lists each month the same way.

**A recovery period for flapping monitors.** A monitor can now wait out its recovery before its incident closes: set **Close incident after** in the monitor's detection settings, or `recovery_period_secs` through the API and MCP tools. A failure that confirms inside the window keeps the same incident open, so a service that keeps falling over is one incident and one set of alerts. The end is still dated to when the last recovery began, so the wait is not counted as downtime. It is off by default.

**Recovery is judged per region.** A region now counts as recovered only after as many passing checks in a row as it took failing ones to count as down, and a region that never failed is no longer evidence that the others recovered. A stray failed check no longer moves an incident's end.

Docs: [public status page](/docs/public-status), [incidents](/docs/incidents), [API](/docs/api).
