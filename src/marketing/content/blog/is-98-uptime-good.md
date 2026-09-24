+++
title = "Is 98% uptime good? It allows 7.3 days of downtime a year"
meta_title = "Is 98% Uptime Good? When It Works and When It Fails"
date = "2026-07-17"
updated = "2026-08-03"
slug = "is-98-uptime-good"
excerpt = "98% uptime sounds high, but it allows 7.3 days of downtime a year. When 98% is enough, what to aim for instead, and a downtime table for every target."
tags = ["sla", "uptime", "reliability", "downtime"]
draft = false
og_image = "/static/marketing/og-98-uptime.png"
cta_label = "Measure your real uptime"

[[faqs]]
q = "How much downtime is 98% uptime?"
a = "98% uptime allows 28.8 minutes of downtime per day, 14.4 hours per 30-day month, or 7.3 days per year. The allowed failure is 2% of the time window, so multiply any window by 0.02 to get the number."

[[faqs]]
q = "Is 98% uptime good for a website?"
a = "Not for a public website or an API that customers pay for. 98% allows 14.4 hours of downtime a month. It is fine for internal tools, staging servers, and hobby projects where nobody loses money while it is down."

[[faqs]]
q = "What uptime should I aim for?"
a = "99.9% is the common target for customer-facing services. It allows 43 minutes of downtime per 30-day month, which is enough room for deploys and small failures. Each extra nine costs roughly ten times more engineering."

[[faqs]]
q = "What is the difference between uptime and an SLA?"
a = "Uptime is the measured number: how much of the time your service actually worked. An SLA is a contract promise with a penalty, usually a service credit on your bill. A provider can promise 99.9% in the SLA and still deliver 98%. The credit refunds part of the bill. It does not refund your customers' time."
+++

> **TL;DR.** For a public website or a paid API, no. 98% uptime allows 7.3 days of downtime per year, or 14.4 hours every month. For an internal wiki or a side project, it is fine. Most customer-facing services target 99.9%, which allows 43 minutes a month. The full downtime table is below, and the [uptime SLA calculator](/tools/uptime-sla-calculator) does the math for any target.

![A yellow open 24 hours sign shining on the corner of a dark brick building at night.](/static/marketing/blog-98-open-24-hours.webp)

*The whole job of an uptime target is that this sign stays on. Photo by [the blowup](https://unsplash.com/@theblowup) on [Unsplash](https://unsplash.com/photos/yellow-and-black-no-smoking-sign-yoWW2VY9A5s).*

98% looks like a top grade. In school it is. Uptime does not grade like school: the whole scale for public services lives between 99% and 100%, and serious targets differ only in the digits after the decimal. On that scale, 98% sits at the bottom.

The trick to reading any uptime number is to turn the percentage into time. The allowed failure at 98% is 2%. Two percent of a year is 7.3 days. Two percent of a 30-day month is 14.4 hours. If your shop makes $2,000 a day, 98% uptime means you accept about $14,600 of closed-door time per year and the target still counts as met.

![One year at 98% uptime drawn as 52 weeks of day cells: four short red outage runs totalling 7.3 days scattered through a green year, labelled with the note that the 98% target is still met, and the math line 28.8 minutes per day, 14.4 hours per month, 7.3 days per year, $14,600 at $2,000 a day.](/static/marketing/blog-98-uptime-year.webp)

## The downtime table

Multiply the window by the allowed failure and the percentages stop hiding. The last column prices the downtime for that $2,000-a-day shop.

| Uptime | Per day | Per 30-day month | Per year | Lost sales per year |
|--------|---------|------------------|----------|---------------------|
| 90% | 2.4 hours | 3 days | 36.5 days | $73,000 |
| 95% | 1.2 hours | 36 hours | 18.3 days | $36,500 |
| **98%** | 28.8 minutes | 14.4 hours | 7.3 days | $14,600 |
| 99% | 14.4 minutes | 7.2 hours | 3.7 days | $7,300 |
| 99.5% | 7.2 minutes | 3.6 hours | 1.8 days | $3,650 |
| 99.9% | 1.4 minutes | 43 minutes | 8.8 hours | $730 |
| 99.95% | 43 seconds | 21.6 minutes | 4.4 hours | $365 |
| 99.99% | 8.6 seconds | 4.3 minutes | 52.6 minutes | $73 |
| 99.999% | 0.9 seconds | 26 seconds | 5.3 minutes | $7 |

Two jumps on this table do most of the work in real contracts. From 98% to 99.9%, the allowed downtime drops from 14.4 hours a month to 43 minutes. From 99.9% to 99.99%, it drops from 43 minutes to 4.3 minutes, and that last jump is usually the expensive one.

The money column makes the trade clear. The first jump is worth about $13,900 a year to the shop. The second is worth $657, and it costs far more engineering than the first. The extra nines only pay off when the number per day is much bigger than $2,000.

## When 98% is enough

Plenty of systems can live at 98% and nobody gets hurt:

- An internal wiki. People retry after lunch.
- A staging environment. Downtime there is often planned.
- A batch job that builds reports at night. It has hours of slack before anyone reads the output.
- A home server on a residential connection. Your power company already decided your uptime for you.

The shared pattern: when these systems go down, nobody loses money and nobody loses trust. Paying for more nines there is waste. If you never spend your downtime budget, your target is too strict, which is the same rule that governs [error budgets](/blog/error-budgets-explained).

## When it is not

A checkout page, a paid API, a login service. Here 98% fails twice.

First, the direct cost: 14.4 hours a month of failed requests, missed payments, and support tickets.

Second, the trust cost, which is larger and slower. A customer who hits your outage twice in one week does not check your uptime report. They remember that your service is the one that breaks.

There is also a contract problem. If your customers have SLAs of their own, your 98% becomes their ceiling. Nobody can build a 99.9% service on top of a 98% dependency without doing extra engineering to route around it.

## The shape of the downtime matters

98% per month is 14.4 hours, but the number says nothing about how those hours land.

Thirty minutes of planned maintenance every night at 03:00 adds up to 98% and most users never notice. One 14-hour outage on the day of your product launch is also 98%. Same score, very different month.

![Two 30-day bars that both score 98% uptime: the top bar loses thirty minutes every night at 03:00, shown as thin red ticks, captioned nobody writes a postmortem; the bottom bar loses one 14.4-hour red block on launch day, captioned every customer notices.](/static/marketing/blog-98-uptime-shape.webp)

This is why a single uptime percentage is a summary, not the full story. You also want to know the longest single outage and when it happened. A monthly 98% made of small night-time blips is a working service with a maintenance window. A monthly 98% made of one long daytime outage is an incident with a report to write.

## Uptime is measured, an SLA is promised

The two get mixed up constantly. Uptime is a measurement: the fraction of time your service actually answered. An SLA is [a promise in a contract](/blog/uptime-sla), with a defined penalty when it is broken.

The penalty is almost always a service credit. If a provider misses its 99.9% SLA, you get a percentage of that month's bill back. Your customers get nothing, because the credit refunds your invoice, not their time. So an SLA tells you how confident the provider is, and it caps your refund. It does not keep your service up.

The practical rule: promise a number you have measured, not a number that sounds good. If you have never measured your uptime, you do not know whether you are at 98% or 99.9%, and the gap between those two is 13.7 hours a month.

## What to aim for instead

[99.9%](/blog/how-much-downtime-is-99-9-uptime) is the default target for customer-facing services for a reason. 43 minutes a month is enough room for a bad deploy and a couple of small failures, and it is achievable by a small team without heroics. Above that, each nine costs roughly ten times more engineering and most users cannot feel the difference.

![Four steps rising from 98% to 99.99%, each labelled with its downtime per 30-day month, from 14.4 hours down to 4.3 minutes. The 99.9% step is outlined in green and tagged aim here. The caption reads: each step to the right costs about ten times more engineering.](/static/marketing/blog-98-nines-ladder.webp)

Pick the target from the table, then measure against it. The measuring part is what [external uptime monitoring](/uptime-monitoring-for-developers) does: it checks your service from outside, the way a user reaches it, and records every failure whether you were watching or not. The measured number only stays honest if your status page shows it directly, which is the idea behind [a status page you cannot fake](/blog/status-page-you-cant-fake).

Each of the common targets has its own breakdown: [99.9% allows 43 minutes 12 seconds a month](/blog/how-much-downtime-is-99-9-uptime), [99.95% allows 21 minutes 36 seconds](/blog/how-much-downtime-is-99-95-uptime), and [99.99% allows 4 minutes 19 seconds](/blog/how-much-downtime-is-99-99-uptime).

Run your own numbers in the [uptime SLA calculator](/tools/uptime-sla-calculator), or work out how fast an outage spends your budget in the [error budget calculator](/tools/error-budget-calculator). If the downtime column next to your current target surprises you, that is the sign you picked a percentage instead of a promise.

## Common questions

<details class="mk-faq">
<summary>How much downtime is 98% uptime?</summary>
<div class="mk-faq__body">

98% uptime allows 28.8 minutes of downtime per day, 14.4 hours per 30-day month, or 7.3 days per year. Multiply any window by 0.02.

</div>
</details>

<details class="mk-faq">
<summary>Is 98% uptime good for a website?</summary>
<div class="mk-faq__body">

Not for a public website or an API that customers pay for. It is fine for internal tools, staging, and hobby projects where downtime costs nothing.

</div>
</details>

<details class="mk-faq">
<summary>What uptime should I aim for?</summary>
<div class="mk-faq__body">

99.9% is the common target for customer-facing services. It allows 43 minutes per 30-day month. Each extra nine costs roughly ten times more.

</div>
</details>

<details class="mk-faq">
<summary>What is the difference between uptime and an SLA?</summary>
<div class="mk-faq__body">

Uptime is the measured number: how much of the time your service actually worked. An SLA is a contract promise with a penalty, usually a service credit. The credit refunds part of your bill, not your customers' time.

</div>
</details>
