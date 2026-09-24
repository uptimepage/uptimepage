+++
title = "How I stop one bad probe from waking you at 3 a.m."
date = "2026-07-25"
slug = "stop-false-uptime-alerts"
excerpt = "One monitoring location with a bad network day is the classic false alert. Here is how repeat checks plus a region vote keep that alert away from your phone."
tags = ["monitoring", "multi-region", "false-alerts", "alerting"]
draft = false
og_image = "/static/marketing/og-stop-false-uptime-alerts.png"

[[faqs]]
q = "What is a false alert in uptime monitoring?"
a = "A false alert says your site is down when it is not. The usual cause is a problem near the probe, not near your site: a bad network path, a busy datacenter, a DNS hiccup on the monitoring side. Your site answered fine for every real user the whole time."

[[faqs]]
q = "How many regions need to agree before an incident opens?"
a = "By default, more than half of the regions that are reporting results. A region only joins the down side after it fails the same check twice in a row. You can change the rule per monitor: any single region, all regions, or a fixed count."

[[faqs]]
q = "What happens when a probe region goes offline?"
a = "It leaves the vote. A region that sends no results is not counted as down and not counted as up. The vote recalculates over the regions that still report. Missing data alone never opens an incident."

[[faqs]]
q = "Does a silent region close my open incident?"
a = "No. Closing needs real proof: the down votes must fall below the threshold and at least one region must show a run of passing checks. Silence is not proof of recovery, so an open incident stays open until real results come back."
+++

> **TL;DR.** The most common false alert in uptime monitoring is one probe location having a bad network day. I treated that as a design requirement from day one and built the checking architecture around two gates: a region only counts as down after it fails the same check twice in a row, and an incident only opens when enough regions agree. A region that goes silent leaves the vote instead of counting as down. One bad location cannot page you.

## The alert that was nothing

The phone buzzes at 3 a.m. "Your API is down." You get up, open the laptop, and everything is green. The site was fine the whole time. One monitoring server, somewhere far away, had a bad network moment and sent an alert about nothing.

This is the false alert everyone in monitoring knows. It costs you sleep, and then it costs you something worse: the next alert feels less serious. Google's SRE book describes the same thing: when pages come [too often](https://sre.google/sre-book/monitoring-distributed-systems/), people start to ignore them, sometimes including the real one. The day a real outage comes, you look at your phone and think "probably nothing again."

I knew this failure mode before I wrote the first line of the scheduler, so it became a requirement, on the same level as "checks must run on time." The rule I started from: a single bad location must never be able to page a customer. The whole checking pipeline, from how probes report to how incidents open, is shaped by that rule.

The shape it took is two gates. A failure has to pass both before anyone gets paged.

## Gate one: fail twice, from the same place

One failed check proves very little. Networks lose packets, routers restart, and sometimes a DNS answer arrives a second too late. All of that can make one check fail while your site is healthy.

So a region only counts as down after it fails the same check twice in a row. Not two failures somewhere in the system: two failures from that one region, back to back. A single blip resets to zero on the next good check.

The count is a setting on each monitor. Two is the default. For a very sensitive check you can set it to one, and for a noisy target you can raise it. Checks run on the monitor's own interval, so with a one-minute check the second failure arrives about a minute after the first. That minute buys you a lot of silence for a very small delay.

Turn the setting yourself and watch what it does to three different stories: one blip, one check that keeps flapping, and one real outage.

<div class="mk-embed-gate"></div>

## Gate two: the vote

Passing gate one gives a region exactly one vote. Nothing more.

Say you check from five regions and one of them has a bad ISP day. It fails twice in a row and votes "down." The other four keep passing. One vote against four is not enough, so nothing happens and nobody gets paged.

The default rule is a majority: more than half of the reporting regions have to agree. You can change it per monitor. "Any" opens an incident on the first confirmed region, which is useful when you want the earliest possible signal and can accept some noise. "All" waits until every region agrees, for a service that only matters when it is unreachable everywhere. Or you pick a fixed number, like two regions out of whatever you assign.

Both gates are easier to see moving than described. Press play to watch a real vote go from all green to a page, then change the rule and watch where that line falls.

<div class="mk-embed-quorum"></div>

A monitor checked from a single region behaves the same under every rule. The vote only starts to protect you when you add a second location, and it gets better with a third.

## Silence is not failure

Here is the part that took the most care to get right.

The probe regions push their results to the control plane, the brain of the system. The brain never calls out to ask "are you alive?" during a vote. It just counts the results that arrived in the last few check cycles.

That gives silence a clear meaning. A region that lost its own connection cannot send anything, so it simply is not in the vote. It is not a down vote and not an up vote; the region is out until it reports again. The majority recalculates over the regions that still speak. Five regions where one goes dark becomes a vote of four.

I think this is the only honest reading. A region that cannot reach the brain is telling you nothing about your site. Treating its silence as "down" would turn every probe outage on the monitoring side into a fake incident on yours.

Switch between those two readings yourself. The same eight cycles end either in quiet or in a page for an outage the site never had.

<div class="mk-embed-silence"></div>

And it works in both directions. Missing data never opens an incident, and it never closes one. An open incident only closes when the down votes fall below the threshold and at least one region shows a real run of passing checks. Recovery needs proof, the same way failure does.

## Who watches the watchers

There is one gap left. If every region that covers your monitor goes dark, the vote has nobody in it. Your site could burn down and no incident would open, because no data means no votes.

For that case there is a separate signal, one level below the checks. Every probe sends a small check-in to the brain on its own schedule, a heartbeat that has nothing to do with your monitors. When the last live probe covering a monitor goes stale, you get a different message: "NO DATA: monitoring interrupted, no check results received." It is honest about what it knows: the service cannot see your site right now. It does not claim your site is down, because it has no idea.

When probing returns, you get a "monitoring RESUMED, receiving check results again" note, and the vote picks up where it left off.

One more detail I like. If a large share of all monitors goes silent at the same time, that pattern is almost never a thousand customer problems. It is one problem, mine. In that case the system alarms me and holds the customer notices, so my bad day does not become spam on yours.

## What this means for your setup

Three practical points follow from this.

Check from more than one region. The vote cannot protect you with a single location. Two regions with the majority rule means both have to agree, which already kills the classic false alert. Three gives you two out of three, which is the right balance for most services.

Leave the confirmation count at two unless you have a reason. It is the difference between "a packet got lost" and "this endpoint is failing."

Pick the rule to match the monitor. A payment API deserves majority or even any. An internal tool nobody uses at night can wait for all. The setting is per monitor, so you do not have to choose one policy for everything.

The goal of all this machinery is boring: when your phone buzzes, it is real. Everything else is plumbing.

## Common questions

<details class="mk-faq">
<summary>What is a false alert in uptime monitoring?</summary>
<div class="mk-faq__body">

A false alert says your site is down when it is not. The usual cause is a problem near the probe, not near your site: a bad network path, a busy datacenter, a DNS hiccup on the monitoring side. Your site answered fine for every real user the whole time.

</div>
</details>

<details class="mk-faq">
<summary>How many regions need to agree before an incident opens?</summary>
<div class="mk-faq__body">

By default, more than half of the regions that are reporting results. A region only joins the down side after it fails the same check twice in a row. You can change the rule per monitor: any single region, all regions, or a fixed count.

</div>
</details>

<details class="mk-faq">
<summary>What happens when a probe region goes offline?</summary>
<div class="mk-faq__body">

It leaves the vote. A region that sends no results is not counted as down and not counted as up. The vote recalculates over the regions that still report. Missing data alone never opens an incident.

</div>
</details>

<details class="mk-faq">
<summary>Does a silent region close my open incident?</summary>
<div class="mk-faq__body">

No. Closing needs real proof: the down votes must fall below the threshold and at least one region must show a run of passing checks. Silence is not proof of recovery, so an open incident stays open until real results come back.

</div>
</details>
