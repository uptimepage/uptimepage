+++
title = "HTTP checks keep cookies across redirects"
date = "2026-10-22"
summary = "A site that sets a cookie and redirects to itself now shows its real status, and a redirect that goes in circles is called a loop."
+++

**Cookies carry across redirects.** Some sites answer the first visit with a cookie and a redirect back to the same address, and serve the real page only once the cookie comes back. HTTP checks dropped that cookie between hops, so every run of such a monitor ended in "too many redirects" and its real status never showed. A check now sends cookies a redirect sets on the later hops of the same run, as a browser would. Each cookie goes only where its domain and path allow, a `Secure` one only over HTTPS, and no cookie can cover a whole public suffix such as `co.uk`. The cookies are dropped when the check ends, so every run starts with none, and their values never appear in probe output or errors.

**Redirect loops are named.** When a check runs out of hops on a redirect that would repeat a request it already sent, the error now says "redirect loop" instead of "too many redirects". That covers an address that keeps redirecting to itself with the same cookies, and two addresses bouncing between each other, such as `http` and `https` or `www` and the bare domain. A long chain across different addresses still says "too many redirects".

**Configured cookies go out as one header.** A monitor with `Cookie` header rows whose names differ only in case, such as `Cookie` and `cookie`, sent them as two `Cookie` headers, though a request should carry only one. They now go out as one header, together with any cookies a redirect set, and a configured cookie wins over one a redirect sets with the same name. A configured cookie is still left off a redirect to another origin.

More on what our checks send: [our bot](/bot).
