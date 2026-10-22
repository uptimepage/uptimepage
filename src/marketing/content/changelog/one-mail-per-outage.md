+++
title = "Status page subscribers get one mail per outage"
date = "2026-10-22"
summary = "Monitors that fail together reach each email subscriber as one mail when the outage starts and one when it ends, and no mail is lost to a rate limit."
+++

**One outage, two mails.** Monitors on a status page that fail together now reach an email subscriber as one outage instead of one mail per monitor. The first mail goes a minute after the first failure and lists every component affected by then, each linking to its incident. The second goes once every component is back and the page has stayed quiet for 5 minutes, and says how long each was down. Five monitors down for two minutes send each subscriber two mails instead of ten. Updates someone writes still go out one by one, as written, and webhook subscribers still get every incident update.

**Every subscriber gets the mail.** All mail now goes out at one shared pace that stays under the mail provider's limit, so a burst of incidents no longer loses subscriber mail to rate limits. A rate limit, a quota or an outage at the provider makes a mail wait and never gives it up. Anything still unsent a day after the event is dropped as no longer news.

**Webhooks are retried on the receiver's terms.** A `429` or `503` with `Retry-After` is honoured, up to an hour later, without using one of the 8 tries. `410 Gone` ends the delivery. Other failures are retried from 30 seconds apart, doubling up to 30 minutes.

**Self-hosting.** Every instance books its sends in one Postgres row, and together they stay under the mail provider's limit per second, which the [email configuration](/docs/configuration) sets. Sign-in links and alerts go ahead of subscriber mail. Two new metrics follow subscriber delivery: settled deliveries by status, and how long the oldest due one has waited.

Docs: [subscriptions](/docs/public-status#subscriptions), [metrics](/docs/metrics).
