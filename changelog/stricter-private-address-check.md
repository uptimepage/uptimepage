+++
title = "A stricter private address check"
date = "2026-11-03"
summary = "Checks, webhooks and notification channels now refuse IPv6 outside the public unicast range unless it carries a public IPv4, plus a few other ranges."
+++

**Only public addresses pass.** Before a check, webhook or notification channel connects, the address its host resolves to has to be public. For IPv6 that test was a list of ranges to refuse, and a few ranges were missing from it. IPv6 now passes only inside `2000::/3`, the range set aside for public unicast, or as an IPv4-mapped or NAT64 (`64:ff9b::/96`) address, which is judged by the IPv4 it carries. Inside `2000::/3`, the benchmarking and documentation ranges are refused, and so are 6to4 and Teredo addresses that carry a private IPv4. On the IPv4 side, all of `0.0.0.0/8` is refused now, where before only `0.0.0.0` was.

**For self-hosters.** A monitor whose target resolves to IPv6 outside `2000::/3`, such as a site-local `fec0::/10` address or a local-use NAT64 prefix under `64:ff9b:1::/48`, now fails with a blocked address error. If you monitor a private network on purpose, set `security.allow_private_targets = true` and keep the network segmented yourself.

Docs: [refused ranges](/docs/configuration#tuning-notes).
