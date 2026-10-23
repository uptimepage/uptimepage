+++
title = "Certificate, DNS and domain checks show what they read"
date = "2026-10-19"
summary = "The monitor page shows a certificate's or domain's days left and issuer or registrar, the DNS records each region got, and the values a failing check saw."
+++

**What the last check read.** A passing certificate, DNS or domain check kept nothing of what it read, so its monitor page showed little more than a latency chart. The monitor page now shows the reading. A certificate monitor shows the days left, the date, who it is issued to and by whom. A domain monitor shows the days left and the registrar. A DNS monitor shows the records each region got, with an unexpected one marked `+` and an expected one that is missing struck through. When regions read different things, the worst reading comes first, so a stale certificate on one edge or a region that gets other answers stands out. **raw json** shows each region's reading as it is stored.

**What failed, in the check log.** A failing check's line in the check log now names the values: `A records −192.0.2.10 +198.51.100.7 · example.com`, or the issuer of a certificate about to run out. Open the line to see every record. On a share link the line keeps its short sentence, without the values.

**DNS failures in words.** A failing DNS check used to read "check failed". It now says what went wrong: no records of the type you asked for, a record set that changed with how many records are missing and unexpected, or no record holding the value you expect.

**Through the API.** A certificate, DNS or domain result that read something carries `details`, passing ones included. A failing result keeps its `error` text, except that DNS answers in it are now sorted, so an answer that only changed order reads the same.

**Fixes.**

- A DNS monitor on a wildcard name such as `*.example.com` was refused. It is accepted now; a `*` anywhere but the leftmost label still is not.
- A certificate that stores its names in an old string type, common on certificates issued years ago, was named `<no CN>` in alerts and the check log. Its subject and issuer read correctly now.
- After a domain monitor's domain was changed, a failed lookup could report the old domain's expiry. It reports the failure until the new domain's first good lookup.
- The subscribe form on a status page answered 403 to someone also signed in to the console in the same browser. It works now.
- Self-host: builds from this one on start on a database a newer build has already migrated, so rolling back to them works when the newer migrations only add to the schema.

Docs: [certificate monitors](/docs/monitor-types#tls-certificate), [domain monitors](/docs/monitor-types#domain-expiry), [DNS monitors](/docs/monitor-types#dns), [results through the API](/docs/api#results-query).
