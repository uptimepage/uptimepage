+++
title = "A monitor's type is fixed after creation"
date = "2026-09-19"
summary = "The API no longer lets a PATCH turn one kind of monitor into another. Create a new monitor instead; the old one keeps its history in one piece."
+++

The edit form never offered it, but `PATCH /api/v1/targets/{id}` accepted a `check` of a different `type`: an http monitor rewritten as a heartbeat, or back. The monitor kept its id, so both kinds' results landed in one history and uptime was computed across them.

The API now refuses it with `400 CHECK_KIND_IMMUTABLE`. Change any other field of the check freely; to watch something of another kind, create a new monitor. The same rule holds at the database row, so no other writer can slip past it.

If you manage monitors with the Terraform provider, a `type` change in HCL fails at apply until the provider learns to replace the resource for it.

Docs: [REST API](/docs/api#target-payload).
