+++
title = "Why your uptime monitor should be boring"
date = "2026-05-20"
updated = "2026-07-24"
slug = "boring-uptime"
excerpt = "A monitor that surprises you is doing the wrong job. What changes when you treat the watchdog like a smoke detector: cheap, dumb, unmissable when it matters."
tags = ["monitoring", "incidents", "status-pages", "on-call"]
draft = false

[[faqs]]
q = "How many probe regions have to agree before an incident opens?"
a = "By default, more than half of the regions that monitor runs in. It is two separate gates. First, a region only counts as failing after it fails a set number of checks in a row on its own, with two in a row as the default, so a single bad sample is never enough. Then the failing regions are counted against the monitor's region policy: any, majority, all, or an explicit count. A monitor assigned three regions therefore needs two of them to agree. Majority is the default because one probe location having a bad network day is the most common false alarm there is."

[[faqs]]
q = "Does waiting for agreement make outage detection slower?"
a = "Slightly, and it is a trade worth making. Two confirmations at the monitor's check interval means you hear about a real outage one interval later than a monitor that pages on the first failed check. The alternative is not faster alerts, it is alerts your on-call has learned to swipe away. Set the policy to any on the few monitors where the earliest possible signal beats the noise, such as a payment callback, and leave the rest on majority."
+++

## Why your uptime monitor should be boring

It's 2:47 a.m. on a Tuesday. The on-call's phone buzzes for the fourth
time tonight. Same monitor, same flap, same 30-second blip that
resolves before they finish reading the alert. They thumb the screen,
mutter, set the phone back down.

The fifth ping is the real one. Nobody hears it.

This is how monitors fail. They miss the outage outright sometimes,
but more often they just get noisy enough that the humans on the other
end learn to tune them out. Two months in, the Slack channel is muted.
Six months in, the on-call rota's morale is sunk. A year in, somebody
is writing a postmortem that opens "the alerting system was working as
designed."

A monitor that surprises you is doing the wrong job. The interesting
parts of your stack belong in your product. The watchdog should be the
dullest thing you own.

## The seduction of cleverness

Every quarter there is a steady temptation to make the monitor
smarter. Add a second data source. Wire in latency percentiles. Build
a dashboard that overlays deploys against error rates. Each of these
sounds like an improvement when it's proposed, and each of them is a
new way the monitor itself can break.

The hard part isn't the database choice or the templating layer. The
hard part is *not* shipping the next clever feature when the room is
enthusiastic about it.

Boring is hard because it looks like restraint, and restraint rarely
wins a planning meeting.

## What the discipline actually buys you

Three things, every one of them invisible until you don't have them.

**An honest signal.** A check that flakes for unrelated reasons is
worse than no check at all. The on-call learns the failure mode is
"noisy," and they stop reading the messages. By the time a real outage
lands, it's tagged in the same mental bucket as the false positives and
quietly dismissed. Honest means one expectation per monitor, written
down somewhere you can argue about, and a state machine that knows the
difference between a sample and an incident.

**A cheap watchdog.** If the monitor is expensive, somebody will
eventually switch it off under cost pressure. That decision gets made
at exactly the wrong moment, usually three weeks before the worst
outage of the year. A monitor that costs almost nothing to run is a
monitor that survives the budget review.

**A cacheable public surface.** When you go down, your status page gets
hammered. Bots scrape it. Customers refresh it. Your support team links
to it in every reply. If rendering that page touches the database that
just fell over, congratulations: now you have two outages instead of
one. The fix is unglamorous. Cache the bytes, set a short
[`Cache-Control`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Cache-Control), let the edge do the work. Ten seconds of caching turns
a thundering herd into a polite trickle.

## The line nobody wants to draw

The trick is knowing when to say no.

A teammate proposes pulling live revenue numbers into the status page
so the team can see "real" impact during an incident. Sounds
reasonable in the meeting. In production, it means the status page now
depends on the billing service, and if billing is what broke, the page
goes white at the worst possible moment.

A vendor pitches an AI-powered alert classifier that promises to reduce
noise by 40%. Sounds great, until you notice it adds a new vendor your
watchdog now depends on. Your monitor now has an SLA that's worse than
most of the things it's monitoring.

A founder asks if the monitor can also do log aggregation, because "we
already have the pipeline." Now the watchdog has the same blast radius
as the log stack, and the log stack is a notoriously crash-prone
neighbour.

In every case, the right answer is the same. The monitor should do
one thing. It should keep doing that one thing while everything else
is on fire.

## Common questions

<details class="mk-faq">
<summary>How many probe regions have to agree before an incident opens?</summary>
<div class="mk-faq__body">

By default, more than half of the regions that monitor runs in. It is two separate gates. First, a region only counts as failing after it fails a set number of checks in a row on its own, with two in a row as the default, so a single bad sample is never enough. Then the failing regions are counted against the monitor's region policy: any, majority, all, or an explicit count. A monitor assigned three regions therefore needs two of them to agree. Majority is the default because one probe location having a bad network day is the most common false alarm there is.

</div>
</details>

<details class="mk-faq">
<summary>Does waiting for agreement make outage detection slower?</summary>
<div class="mk-faq__body">

Slightly, and it is a trade worth making. Two confirmations at the monitor's check interval means you hear about a real outage one interval later than a monitor that pages on the first failed check. The alternative is not faster alerts, it is alerts your on-call has learned to swipe away. Set the policy to any on the few monitors where the earliest possible signal beats the noise, such as a payment callback, and leave the rest on majority.

</div>
</details>

## The reward

Run the monitor this way for a year and you'll mostly forget it
exists.

That's the entire point.

When something does break, the alert arrives in the right channel,
points at the right system, and reaches the right human. The status
page renders in 50 ms because the database isn't in the request path.
The on-call trusts the ping because they haven't been lied to in
months.

The monitor isn't the hero. It isn't the product. It's the smoke
detector you only think about when the kitchen catches fire, and then
you're profoundly grateful you bought the cheap, dumb one with the loud
beeper instead of the smart one that needed a firmware update.

Keep it boring. Keep it cheap. Keep it out of the way.

It's the bet behind [how we built Uptimepage](/status-page-for-saas), and
the reason even [an AI interface to it](/blog/mcp-server) changes nothing in
production. It's also how we'd tell you to read [the open-source and
self-hosted tools](/blog/best-self-hosted-uptime-monitoring-tools): pick the
dull one you'll never have to think about.

And before you promise anyone a number, it's worth seeing [what each uptime
percentage actually costs you in downtime](/tools/uptime-sla-calculator): the
gap between three nines and four is a lot bigger than it reads on a slide.

Your future self, at 2:47 a.m. on some Tuesday, will thank you.
