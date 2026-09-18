+++
title = "Three new free tools: SSL checker, domain expiry, redirect chain"
date = "2026-09-06"
summary = "SSL certificate checker, domain expiry checker, and an HTTP header and redirect chain checker. No account needed, no email capture."
+++

No account needed, no email capture.

- [SSL certificate checker](/tools/ssl-certificate-checker): chain, issuer, expiry, days left.
- [Domain expiry checker](/tools/domain-expiry-checker): registrar expiry for the domain, the thing that takes a site down with no warning.
- [HTTP header and redirect chain checker](/tools/http-header-checker): every hop with its raw `Location`, the final status, and the full header set. Names the failures the last status code hides: a loop, a hop limit, a chain that leaves the host and drops cookies, a chain ending on plain HTTP.

All tools: [/tools](/tools).
