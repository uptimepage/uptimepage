+++
title = "Private status pages for the people you list"
date = "2026-10-14"
summary = "A status page can now be private: only the addresses and domains on its list can open it, after signing in with a code sent by email."
+++

**Private pages.** A status page can be public, private or off. A private page opens only for the email addresses and domains on its access list: `example.com` lets in everyone at that domain, and an address lets in one person, up to 500 entries. Pick **private** under **who can open it** in the page editor and fill in the list on the new **access** tab. Private pages come with the Team plan, and with every self-hosted install.

**Signing in.** A visitor sees a sign-in screen in the page's branding and language, enters their email and gets a 6-character code and a sign-in link. Every address gets the same answer, so the screen never tells anyone who is on the list. After the first code, a viewer can save a passkey and skip the email next time. You choose how long a sign-in lasts (until sign-out, or 7, 30 or 90 days) and whether passkeys are offered.

**Everything sits behind the sign-in.** The page, the incident history and detail pages, the JSON API, the badge and the RSS feed answer only a signed-in viewer, and none of it is cached or indexed. A viewer's feed reader gets a personal link that stops working when they lose access.

**Subscribers.** Making a page private removes its webhook subscribers and the email subscribers the list does not cover. A signed-in viewer subscribes as themselves, with no confirmation mail.

**Viewers.** The access tab lists who signed in, when they last visited and their history for the past year. **Block** signs someone out at once and keeps them out whatever the list says.

**API, MCP and Terraform.** The `access` field of a status page sets the list and the sign-in options through the REST API and the MCP tools. Provider 0.16.0 adds an `access` block to `uptimepage_status_page`.

**Off pages stay quiet.** A page that is off no longer sends mail or webhook calls to its subscribers, and nothing is sent for an organization that was deleted.

Docs: [who can open a page](/docs/public-status#who-can-open-a-page), [status pages through the API](/docs/api#operator-endpoints-status-pages), [Terraform](/docs/terraform).
