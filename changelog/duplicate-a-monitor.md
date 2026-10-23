+++
title = "Duplicate a monitor into many"
date = "2026-10-12"
summary = "Duplicate copies a monitor with every setting into up to 100 new ones, each with its own name and address. Plus fixes for API retries and heartbeat creates."
+++

**Duplicate a monitor.** Pick **duplicate** in the menu on a monitor's row or page. Each copy runs with every setting of the source and gets its own name and, if you like, its own URL, host or domain. Paste a list of addresses into one box to add a row for each. Up to 100 copies go in one batch, and the batch is created entirely or not at all.

**Everything carries over.** The old copy filled in the create form in your browser, so it dropped credentials set through the API, the regions and the escalation policy. Duplicate copies the saved monitor instead: the check with its credentials, the interval, the paused or running state, tags, group, alert channels and regions. The escalation policy comes along when you could attach it yourself, as the org owner on a plan with on-call. You own the copies, and a heartbeat copy gets its own ping URL. History, incidents, status page components and share links stay with the source.

**Through the API.** `POST /api/v1/targets/{id}/duplicate` takes a list of copies, each with a `name` and an optional `address`. It counts against the bulk rate limit and accepts an `Idempotency-Key` header.

**Also fixed.**

- A retry with the same `Idempotency-Key` used to get the first answer back for 24 hours, even when that answer was a 429 or a 5xx. Only a success is kept now, so a retry gets a fresh answer.
- A bulk create with a very large response could create every monitor and then answer 500, inviting a retry that made each one twice. It now answers with the monitors.
- Creating a heartbeat could answer with an error after the monitor was saved, so a client or agent that retried got two. The create now succeeds whenever the monitor is saved.

Docs: [duplicating through the API](/docs/api#duplicating-a-target), [idempotency](/docs/api#idempotency).
