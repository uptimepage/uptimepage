+++
# Written by Artem Senenko. Attribution required under AGPL-3.0 sections 7(b) and 7(c); see "Blog posts" in LICENSING.md.
title = "Why DNS returns different IP addresses on different resolvers"
date = "2026-09-05"
slug = "why-dns-returns-different-ip-addresses"
excerpt = "Compare DNS answers from Cloudflare and Google, read TTLs, and query authoritative nameservers to separate normal CDN routing from a broken DNS change."
tags = ["dns", "debugging", "monitoring", "cdn"]
draft = false
+++

> **TL;DR**
>
> Different DNS resolvers can return different IP addresses because they cached a record at different times or received different answers from a CDN's DNS routing. Compare the same hostname and record type, check the response status, then query the authoritative nameservers. A mismatch alone does not tell you which answer is wrong.

You change an A record. Your laptop reaches the new server, a colleague still sees the old site, and a DNS checker shows two addresses. Before changing the record again, find out where those answers came from.

Start with the exact hostname that fails. `example.com`, `www.example.com`, and `api.example.com` can have separate records. An A lookup asks for IPv4 addresses; AAAA asks for IPv6. Comparing one against the other will not tell you whether resolvers agree.

## Compare the answers, including their status

Open the [DNS lookup tool](/tools/dns-lookup), enter the public hostname, and select A. It queries Cloudflare and Google over DNS-over-HTTPS from your browser, showing the selected record type's answers and TTLs side by side. Repeat with AAAA if the service uses IPv6.

This checks two public resolver views. It does not query your company's private DNS, test every region, or establish that a change has reached everyone.

For a terminal comparison, use `dig`, replacing the example hostname with yours:

```bash
dig @1.1.1.1 www.example.com A +noall +comments +answer +authority
dig @8.8.8.8 www.example.com A +noall +comments +answer +authority
```

`@` chooses the resolver. The output options retain the response status, answers, and authority section, which can help explain an empty answer. Unlike the browser tool, these commands use ordinary DNS rather than HTTPS. The [BIND dig manual](https://bind9.readthedocs.io/en/latest/manpages.html#dig-dns-lookup-utility) documents the options.

Read the whole set of addresses, not just the first line. The same addresses in a different order are still the same set. Save the results and the time you ran the queries before flushing any caches.

| What you see | What to check next |
|---|---|
| Same addresses, different TTLs | Cache age; this alone is not a record mismatch |
| Old address on one resolver, new address on another | Previous TTL and current authoritative answers |
| Different addresses for a CDN-backed hostname | Whether both belong to the intended CDN routing configuration |
| An answer on one resolver, `SERVFAIL` on another | Resolver diagnostics, DNSSEC, and nameserver reachability |
| Public resolvers agree, but a VPN-connected client differs | The client's resolver and private DNS configuration |

## Different CDN addresses can both be correct

A CDN can direct lookups toward different serving locations. The authoritative DNS service may use the recursive resolver's location, or client-network information supplied through EDNS Client Subnet (ECS), when choosing an answer.

Google Public DNS supports ECS for participating authoritative servers. Cloudflare's 1.1.1.1 generally does not send it. Those policies can produce different answers even when you run both lookups from the same laptop. See [Google's ECS documentation](https://developers.google.com/speed/public-dns/docs/ecs) and [Cloudflare's ECS policy](https://developers.cloudflare.com/1.1.1.1/faq/#does-1111-send-edns-client-subnet-header).

Check your DNS provider's routing configuration before deciding that one address is stale. If the hostname is meant to point at a CDN, comparing its answer with your origin server's IP is the wrong test. If it is supposed to be a single fixed A record, an unexpected address deserves investigation.

Also inspect aliases. An A response can include a CNAME pointing to another hostname before the final addresses. The lookup tool displays only the selected record type, so use the `dig` output to inspect the returned chain.

## A TTL is not a global propagation countdown

Resolvers cache records independently. One may have fetched the old answer shortly before your edit while another asks after it. The TTL shown in a cached response is normally the remaining cache lifetime in seconds, not the time since your change. [Cloudflare's TTL reference](https://developers.cloudflare.com/dns/manage-dns-records/reference/ttl/) explains the caching tradeoff.

Suppose the old record had a TTL of 3,600 seconds. A resolver fetched it at 11:59, and you changed the address at noon. Under ordinary caching, it can retain that answer until roughly 12:59. Setting the new record's TTL to 60 seconds does not shorten the copy already cached elsewhere.

For a planned migration, lower the TTL in advance and allow the previous TTL to elapse before switching the address. Keep the old destination working during the transition when possible.

There are exceptions to simple countdown reasoning. Resolvers can serve expired records when they cannot refresh them, a resilience mechanism described in [RFC 8767](https://datatracker.ietf.org/doc/html/rfc8767). A persistent old answer is a reason to inspect the authoritative service, not proof that a resolver ignored your edit.

## Ask the authoritative nameservers what they serve

Public resolvers fetch and cache answers. Authoritative nameservers serve the zone's records. Compare those sources when a fixed record still differs after the expected cache window.

For a hostname in the `example.com` zone, start with:

```bash
dig example.com NS +short
```

Replace the placeholder server below with each nameserver returned, and query it directly:

```bash
dig @ns1.your-dns-provider.example www.example.com A +norecurse
dig @ns1.your-dns-provider.example example.com SOA +norecurse
```

`+norecurse` requests an answer without recursion. Look for `aa` in the response flags: authoritative answer. A referral or a response without that flag is not the authoritative result you wanted. If the hostname sits in a separately delegated subdomain, query that zone's nameservers instead. A CNAME target in another zone has its own authoritative servers.

For a fixed record, compare the returned address across the servers. Different SOA serials can help identify a stale zone copy, although matching serials alone do not establish that every record is correct. Google's [DNS troubleshooting guide](https://developers.google.com/speed/public-dns/docs/troubleshooting#returning-the-wrong-answers-for-a-domain) recommends checking this when old answers persist.

If you recently changed DNS providers, compare the delegation at your registrar with the intended nameservers too. Editing a zone at the old provider does not update the new provider's zone.

## Treat missing answers separately

`NXDOMAIN` means a name does not exist, potentially the target of an alias. `NOERROR` with no relevant answer can instead mean that the name has no record of the requested type. An IPv4-only service need not have an AAAA record.

Negative answers can be cached too. Creating a previously missing record does not immediately remove cached failures; their lifetime is governed by negative-caching rules, not simply the new record's TTL. [RFC 2308](https://datatracker.ietf.org/doc/html/rfc2308) explains the distinction and caching behavior.

`SERVFAIL` is a resolution failure, not another IP address. Check the resolver's diagnostics for DNSSEC validation errors, broken delegation, or unreachable nameservers. Google's [domain troubleshooting instructions](https://developers.google.com/speed/public-dns/docs/troubleshooting#problems-resolving-a-domain) show where those diagnostics appear. Do not disable DNSSEC validation as a production fix just to make the lookup succeed.

## Check the resolver the affected client actually uses

If both public resolvers agree but an employee on a VPN sees another address, ask which DNS server that connection uses. Split-horizon DNS intentionally serves different public and private answers. [Route 53's private-zone documentation](https://docs.aws.amazon.com/Route53/latest/DeveloperGuide/hosted-zone-private-considerations.html#hosted-zone-private-considerations-split-view-dns) gives a concrete example.

Run a lookup against the affected client's configured resolver and compare it with the public results. Ask the network owner whether that difference is intended before replacing the client's DNS settings. A successful public lookup does not validate a private application name.

## Monitor the expectation you actually need

Once you understand the difference, the lookup tool's **monitor this record** button starts DNS-monitor setup with the hostname filled in. Select the record type and, if needed, a specific resolver.

Uptimepage's [DNS monitor](/docs/monitor-types#dns) can require an expected substring in an answer. Without that expectation, a nonempty answer is enough to pass. Substring matching does not enforce equality of the complete record set, and pinning a rotating CDN address can create false alarms.

Keep an HTTP check alongside DNS monitoring: resolving the intended name does not establish that the application works. [Choosing what an uptime monitor should check](/blog/do-i-need-an-uptime-monitor) covers that distinction. If the surprise answer followed a registrar change or domain lapse, also check [why an expired domain can leave your uptime monitor green](/blog/domain-expired-but-site-still-up).
