+++
title = "Error budgets, explained: SLOs, burn rate, when to stop shipping"
date = "2026-07-13"
updated = "2026-07-15"
slug = "error-budgets-explained"
excerpt = "What an error budget is, the simple formula, how burn rate becomes an alert, and the rule that makes it work. With a free calculator."
tags = ["sre", "slo", "reliability", "monitoring", "on-call"]
draft = false

[[faqs]]
q = "What is an error budget?"
a = "An error budget is the amount of downtime a service is allowed before it misses its SLO. You find it by multiplying the window by one minus the SLO. A 99.9% SLO over 30 days gives 43 minutes 12 seconds."

[[faqs]]
q = "How do you calculate burn rate?"
a = "Burn rate is your real failure rate divided by the allowed one. That is one minus your measured availability, divided by one minus the SLO. At a 99.9% SLO, a measured 99.8% is a 2x burn rate."

[[faqs]]
q = "What burn rate should trigger an alert?"
a = "A fast alert usually fires at about 14.4x. That rate spends 2% of a 30-day budget in one hour. Slower alerts watch for a steady 3x to 6x that would use up the budget over days."

[[faqs]]
q = "What happens when the error budget runs out?"
a = "When the budget runs out, risky launches stop and the team works on reliability until the budget grows back. While the budget is healthy, you ship fast and take the risk."
+++

> **TL;DR.** An error budget is the downtime your SLO allows before you break it. The formula is simple: window times one minus the SLO, so a 99.9% target gives 43 minutes a month. Burn rate tells you how fast you are spending it, and it maps straight to alerts, like 14.4x for a fast page. The part that matters most is the rule you set in advance: when the budget runs out, risky launches stop until it recovers. The calculator and the full breakdown are below.

An uptime target has a second number hidden inside it. Say you promise 99.9% uptime. You are also saying that 0.1% is allowed to fail. Put that 0.1% into real time, and that is your error budget: the downtime you can have before you break the promise.

This changes how you look at downtime. It stops being a mistake to feel bad about and becomes a budget you can spend: on a risky deploy, on a slow service you depend on, or on a migration. When the budget runs low you slow down. When it is healthy you can move fast. You can try this on your own service with the [error budget calculator](/tools/error-budget-calculator), which does the math below in your browser.

![Budget burn-down chart for a 99.9% SLO at 99.95% measured availability: a green line falling from full budget to the halfway mark over the month, staying above the dashed on-budget line, labelled 50% left.](/static/marketing/blog-error-budget-healthy.webp)

## The formula

An SLO is your reliability target, for example 99.9%. The gap to 100% is the failure you are allowed. Multiply that gap by the length of the window, and you get real time you can spend.

    error budget = window x (1 - SLO)

A 99.9% SLO over a 30-day month is 2,592,000 seconds times 0.001. That is 43 minutes and 12 seconds. That is the whole budget for the month, shared across every incident, not a fresh 43 minutes each day.

So three nines is not "never go down". It is a 43-minute budget each month. Every extra nine costs about ten times more engineering, for downtime that most users never notice. This is why [Google says](https://sre.google/sre-book/embracing-risk/) that 100% is the wrong target for almost every service. The [uptime SLA calculator](/tools/uptime-sla-calculator) shows the full downtime-per-nine table if you want to compare targets.

## Burn rate is the speed

The total tells you how much you can spend. It does not tell you how fast. Two services can both sit at 99.9% for the month: one loses the budget slowly, the other loses it all in a single bad hour. Burn rate tells them apart.

    burn rate = (1 - measured) / (1 - SLO)

A burn rate of 1x spends the whole window exactly. 2x spends it in half the time. Below 1x, you finish the month with budget left. Above 1x, the rate tells you the deadline:

    budget runs out in = window / burn rate

At 2x, a 30-day budget is gone in fifteen days. At 14.4x it is gone in about two days. That same 14.4x spends 2% of the budget in one hour, which is the level most fast alerts are set to.

![Budget burn-down chart for a 99.9% SLO at 99.0% measured availability: an amber line dropping to zero after about a tenth of the month, far below the dashed on-budget line, labelled gone in 3d.](/static/marketing/blog-error-budget-overspent.webp)

## Turning burn rate into alerts

One threshold is not enough. It either alerts too late or it sends too many [false alarms](/blog/boring-uptime). The common fix uses two windows: a long one to confirm the problem is real, and a short one to clear the alert quickly once you fix it. Both have to be burning for the alert to fire. For a 30-day budget, these are the usual settings, from [Google's SRE workbook](https://sre.google/workbook/alerting-on-slos/):

- Fast page: 2% of the budget in 1 hour (with a 5-minute short window). This is a 14.4x burn.
- Page: 5% in 6 hours (30-minute short window). This is a 6x burn.
- Slow ticket: 10% in 3 days (6-hour short window). This is a 1x burn.

The fast page catches a sudden outage. The slow ticket catches a slow problem that would still use up the whole month if nobody looked.

## The rule is the point

The math is the easy part. The value comes from a rule you agree on before anything breaks, which Google writes down as an [error budget policy](https://sre.google/workbook/error-budget-policy/). The rule is simple. When the budget runs out, risky launches stop, and the team works on reliability until the budget grows back. While the budget is healthy, you ship and you take the risk.

One more rule keeps it fair. If you never spend your budget, your SLO is too strict, and you are paying for reliability that no user asked for.

## Where monitoring fits

All of this needs one thing you have to measure: your real availability. The formula is easy once you have that number. Getting the number is the hard part. That is the job of [external monitoring](/uptime-monitoring-for-developers). It checks your service the way a user reaches it, and it records every failure, so the budget matches reality. That number only stays honest if your status page reports what the checks measured, not what someone chose to publish, which is [a status page you cannot fake](/blog/status-page-you-cant-fake).

Run the numbers for your own SLO in the [error budget calculator](/tools/error-budget-calculator). Then decide your rule before you need it.

## Common questions

<details class="mk-faq">
<summary>What is an error budget?</summary>
<div class="mk-faq__body">

An error budget is the amount of downtime a service is allowed before it misses its SLO. A 99.9% SLO over 30 days gives 43 minutes 12 seconds.

</div>
</details>

<details class="mk-faq">
<summary>How do you calculate burn rate?</summary>
<div class="mk-faq__body">

Burn rate is your real failure rate divided by the allowed one. A measured 99.8% against a 99.9% SLO is 2x.

</div>
</details>

<details class="mk-faq">
<summary>What burn rate should trigger an alert?</summary>
<div class="mk-faq__body">

A fast alert usually fires at about 14.4x, which spends 2% of a 30-day budget in one hour. Slower alerts watch for a steady 3x to 6x.

</div>
</details>

<details class="mk-faq">
<summary>What happens when the error budget runs out?</summary>
<div class="mk-faq__body">

When the budget runs out, risky launches stop and the team works on reliability until the budget grows back.

</div>
</details>
