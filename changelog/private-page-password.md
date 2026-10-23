+++
title = "A page password for private status pages"
date = "2026-10-17"
summary = "A private status page can now open with one password you share, beside or instead of the emailed code."
+++

**One password, shared.** A private page now has two ways in, the emailed code and a page password, and either one lets a visitor in. The password suits readers an email list fits badly: people on personal addresses, a partner's NOC, a wall screen. Turn it on in the **access** tab, type one or press **generate** for a 20-character one, and copy it before you save. It is stored as a hash, so it can be replaced but never shown again.

**Changing it shuts everyone out.** Password visitors are anonymous, so there is nobody to block one by one. A new password, or turning it off, signs out every device that came in with it. Turning it off keeps it, and turning it on again brings the same one back. **Sign them all out** ends the open sessions and leaves the password as it is.

**Guessing is slow.** Ten wrong tries in 15 minutes from one network, then the sign-in screen says how long that network waits. The page itself never locks, so wrong tries from one place cannot keep everyone else out.

**Subscribing.** Someone signed in with the password subscribes by email and confirms the address first, as on a public page. Those subscriptions are marked **via password** and end when the password changes.

**API, MCP and Terraform.** `access.password` in the REST API sets the password (write only, never read back), and `access.session_days` now applies to both ways in. The MCP tools turn a stored password on or off but never take one. Provider 0.18.0 adds a write-only `password` to `uptimepage_status_page` and moves `session_days` to match.

Docs: [who can open a page](/docs/public-status#who-can-open-a-page), [status pages through the API](/docs/api#operator-endpoints-status-pages), [Terraform](/docs/terraform).
