+++
title = "Self-hosted status pages each get their own address"
date = "2026-11-25"
summary = "On a self-hosted deploy, every status page now has its own URL under /status/{slug}, with its own subscribe form, sign-in and API."
+++

A self-hosted deploy serves its status pages from the app's own host. Until now only the oldest page had an address there, `/status`, so a second page had none. Every page now lives at `/status/{slug}`, and what belongs to it sits under the same path: its incident history, incident pages, subscribe form, private page sign-in and the JSON API at `/status/{slug}/api/public/v1`. `/status` and old incident links still work and lead to the oldest page.

Subscriber mail, the page editor, badges, the API and MCP all link the new address. A private page keeps its sign-in to its own path, so two private pages on one host no longer sign each other out. This covers a deploy with one organization; with more, use subdomain routing.

**Also in 1.8.0.** A paused or held monitor's page and share link no longer show its last result as if it were current. Flow assertions now fill in `{{variables}}` in the text and URL they expect.

Docs: [per-org status pages](/docs/per-org-status).
