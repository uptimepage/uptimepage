+++
title = "Alert sources: page from Alertmanager, Grafana and Datadog"
date = "2026-11-09"
summary = "On Team, Prometheus Alertmanager, Grafana, Datadog or a script can open and close incidents that page through your rotations."
+++

Tools that already watch your systems can now page through Uptimepage. Create an alert source under Settings, pick the tool, point it at the source's URL, and choose who it pages: an escalation policy, the org default, or channels. A firing alert opens an incident, the incident pages and escalates like any other, and the tool's recovery closes it.

Each source comes with the setup for its tool: an Alertmanager receiver, a Grafana contact point (signed if you like), a Datadog webhook with its payload template, or a small JSON shape for scripts. A test alert on the source's page proves the whole path once.

By default one alert group is one incident, so a two hundred pod failure pages once. Repeats never page, an acknowledgement survives them, and a delayed recovery cannot close a newer outage. Every request lands in the source's delivery log with its outcome, the incident it touched, and the start of its body.

Alert incidents carry the source and what the tool sent: labels, annotations, links and the worst severity it reported. The incidents list filters by source, and the REST API and MCP server read them too.

Alert sources are on the Team plan, five per account.

Docs: [alert sources](/docs/alert-sources).
