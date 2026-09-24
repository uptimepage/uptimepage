+++
title = "The mystery of the \"down\" website"
date = "2026-06-18"
updated = "2026-09-24"
slug = "osi-layers"
excerpt = "\"The site is down!\" But what does \"down\" really mean? A detective story through the seven network layers, and the one quiet failure no alarm caught."
tags = ["monitoring", "networking", "osi", "tls", "dns"]
draft = false
+++

## The mystery of the "down" website

It was a typical Tuesday morning when Jamie, the diligent web admin,
got the message every web admin dreads: *"The site is down!"*

Panic blinked behind their eyes. They stared at the screen. But a
quieter, more useful thought arrived right behind the panic, the one
that separates a long morning from a short one. *What did "down"
actually mean?*

Because Jamie knew a secret that the person who sent the message
didn't. The internet isn't one thing that's either working or broken.
It's a ladder, seven rungs of it, climbing from the physical wires
buried in the ground all the way up to the website itself. "Down"
could be a backhoe through a fibre line. It could be a port that
refused to answer. It could be a certificate that quietly expired at
midnight while everyone slept. Seven different failures, all wearing
the same one-word disguise.

So Jamie did what good detectives do. They started at the bottom of the
ladder and climbed.

## Climbing the ladder

**The wire (layer 1) and the switch (layer 2).** The very bottom rungs:
the physical cable and the local network gear. Jamie couldn't probe
these from their desk; nobody can from the outside. If a trunk line had
been cut, you don't *test* for it, you *infer* it, because everything
above goes dark at once. The lights upstairs were still on, so Jamie
climbed past.

**The network (layer 3).** The first rung you can actually knock on. A
`ping`, a tiny packet sent out to ask "are you even reachable?" Jamie
ran it. The reply came back clean. The server existed, the packet found
its way across the internet and home again. *Good. The foundation
holds.* Up a rung.

**The transport (layer 4).** Now the question got sharper: is anything
*listening*? Jamie opened a TCP connection to port 443, the three-way
handshake that proves a service is awake, no website required, just the
raw "hello / hello back / got it." Port 443 answered. The service was
running. *Strange.* The packets routed. The port was open. By the usual
panic logic, the site was up.

So why were customers seeing red?

**The presentation (layer 6).** Jamie climbed one more rung, to the
layer where bytes get wrapped in encryption and trust, where TLS
lives, with its handshake and, crucially, its certificate. And there it
was. The TLS certificate had **expired** at midnight.

No alarm had fired. Every lower layer was perfectly healthy: the wire,
the route, the open port, all green. But a browser arriving at the site
didn't see a homepage. It saw a full-screen scary warning: *your
connection is not private.* To a visitor, that's worse than down. Down
looks like an outage. This looked like a *trap*.

## The fix, and the lesson

A few clicks. Jamie renewed the certificate, the [chain of trust](/blog/what-is-an-ssl-certificate-chain)
re-formed, and the website sprang back to life. Total time once they
knew *where* to look: minutes.

That last part is the whole story. Jamie didn't fix it fast because
they were a genius. They fixed it fast because they refused to treat
"down" as one thing. They walked the ladder, asked one honest question
per rung, and let the failure announce its own address: *layer 6,
certificate, expired.*

A site is never simply up or down. It's up or down *at a layer.* And
the difference between a five-minute fix and a five-hour one is usually
just knowing which rung to stand on.

## The alarm that should have rung first

Here's the part that nags Jamie on the walk home. The certificate
didn't expire *suddenly.* It had been counting down for ninety days. At
any point in those three months, something could have tapped Jamie on
the shoulder and said *"this lapses Tuesday."* Nothing did. The only
monitor watching the site was checking HTTP, and HTTP looked perfect
right up until the second it didn't.

That's the trap of watching one layer and assuming it speaks for the
others. An HTTP check that's green today tells you nothing about a
certificate expiring tomorrow. A TCP port that's open tells you nothing
about the app behind it returning 500s. Each layer needs its *own*
question asked, on its *own* schedule.

This is exactly the gap [Uptimepage](/status-page-for-saas) is built to
close. Instead of one check pretending to speak for the whole stack, it
runs a dedicated probe per layer, per failure mode:

- **HTTP** (layer 7): status code and body, plus a timing breakdown of
  every step in a single request: DNS lookup, TCP connect, TLS
  handshake, time to first byte, transfer. When a page is *slow* rather
  than *down*, those splits point straight at the guilty layer. To see what layer 7
  answers for one URL right now, including every redirect it takes on the
  way, the [HTTP header and redirect checker](/tools/http-header-checker)
  runs the same walk once.
- **TCP** (layer 4): a raw connect to any host and port. The check
  Jamie ran by hand, running automatically, for databases and SSH and
  mail servers that have no web page at all.
- **Ping** (layer 3): a bare ICMP echo, the "are you even reachable?"
  knock from the opening of the story. It answers below any port or
  page, so a host that falls off the network shows up before an HTTP
  check has even finished timing out.
- **TLS certificate** (layer 6): inspects the cert itself: expiry,
  chain, issuer. The alarm that would have tapped Jamie on the shoulder
  *days* before midnight, while every HTTP check was still cheerfully
  green. You can read those fields for any public host in the
  [SSL certificate checker](/tools/ssl-certificate-checker).
- **DNS** (layer 7): queries a record and checks the *answer*, catching
  a name that resolves to the *wrong* place, not just slowly.
- **Domain expiry** (layer 7): watches the registration lease over
  WHOIS, so the whole site doesn't vanish because a renewal email
  landed in spam.

You'll notice the same words, DNS, TLS, connect, show up twice: once
as *timing phases inside the HTTP check*, and once as *monitors of their
own.* That's not a mistake. Inside HTTP, they measure how *fast* a step
ran during one poll. On their own, they answer a different question
entirely (*is the cert valid, does the name resolve right, is the port
open*) with their own threshold and their own alarm. The cert and
domain checks even run on a slow, hourly cadence, because a certificate
doesn't change between heartbeats; the HTTP and TCP checks run tight,
because that's where the minute-to-minute truth lives.

Jamie smiled, closed the laptop, and felt ready for the next 2:47 a.m.
message, because the next time someone says *"the site is down,"*
they'll already know it's never that simple. They'll know to ask
*which layer.* And the right alarm, [defined once as
code](/blog/monitoring-as-code) and left to watch every rung, will have
already answered before the question was even asked.

The real story behind "down" was never simple. But it was always
*findable*, one layer at a time.
