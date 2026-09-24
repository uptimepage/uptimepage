+++
title = "How much downtime is 99.9% uptime? 43 minutes a month"
date = "2026-08-02"
slug = "how-much-downtime-is-99-9-uptime"
excerpt = "99.9% uptime allows 43 minutes of downtime a month and 8 hours 46 minutes a year. What uses that time, and why three nines is the normal SaaS promise."
tags = ["sla", "uptime", "slo", "downtime"]
draft = false
og_image = "/static/marketing/og-99-9-uptime.png"
cta_label = "Measure your own 43 minutes"

[[faqs]]
q = "How much downtime is 99.9% uptime?"
a = "99.9% uptime allows 1 minute 26 seconds of downtime per day, 10 minutes 5 seconds per week, 43 minutes 12 seconds per 30-day month, and 8 hours 46 minutes per year. The allowed failure is 0.1% of the period, so multiply any period by 0.001."

[[faqs]]
q = "How many hours per month is 99.9% uptime?"
a = "0.72 hours, which is 43 minutes 12 seconds in a 30-day month. Over a full year it is 8.76 hours. People expect a bigger number because 99.9% sounds almost perfect, but the 0.1% is the part that matters."

[[faqs]]
q = "Is 99.9% uptime good?"
a = "It is the normal promise for a customer-facing SaaS product, and a small team can reach it without waking up for every problem. It is not enough for payments or safety systems, where a four-minute outage at the wrong moment costs more than the SLA credit pays back."

[[faqs]]
q = "What is the difference between a 99.9% SLA and a 99.9% SLO?"
a = "The SLO is the target you use inside the company. The SLA is the number you promise in a contract, with a credit if you miss it. Teams that promise 99.9% usually aim for 99.95% internally, so the contract has some spare time built in and does not break the first time the internal target slips."
+++

> **TL;DR.** 99.9% uptime allows **43 minutes 12 seconds** of downtime per 30-day month, or **8 hours 46 minutes** a year. One bad deploy with a slow rollback can use all of it. It is the normal SaaS promise because it is the lowest number a paying customer accepts, and the highest number a small team can reach without automatic failover. For any other target, use the [uptime SLA calculator](/tools/uptime-sla-calculator).

Three nines is the number in almost every SaaS contract, and almost nobody turns it into minutes before signing. 99.9% sounds like "always up". The 0.1% that is left is 43 minutes a month. That is about one incident that does not go well.

The math is one multiplication. The allowed failure at 99.9% is 0.1%, so multiply the period by 0.001.

## 99.9% uptime in real time

| Period | Allowed downtime |
|--------|------------------|
| Per day | 1 minute 26 seconds |
| Per week | 10 minutes 5 seconds |
| Per 30-day month | **43 minutes 12 seconds** |
| Per quarter (90 days) | 2 hours 10 minutes |
| Per year (365 days) | 8 hours 46 minutes |

The monthly number matters most, because that is the period most contracts measure and most credits pay against. 43 minutes is not much. It is one deploy that fails, is noticed after ten minutes, and takes half an hour to undo.

## What uses the 43 minutes

The time is rarely used by the outage you planned for.

A bad deploy with a manual rollback will use it alone. Ten minutes before anyone notices, five to confirm the problem is real, twenty to undo it and deploy again. That is the month.

An expired certificate is worse, because nobody is watching the expiry date. The downtime runs from the moment the certificate expires until a customer writes to you. It costs nothing to prevent and it still ruins months.

A DNS change with a long TTL takes the decision away from you. You fix the record in a minute, and the internet keeps using the old answer until the TTL runs out.

Then there are the services you do not run. If your payment provider, your login provider or your CDN has 20 minutes of trouble, those 20 minutes come off your budget. Their credit pays back part of their bill, not yours.

None of these are unusual failures, which is the point of three nines. The target sits where normal mistakes still fit inside the budget and a careless month does not.

## Why 99.9% is the normal choice

Look at the targets on either side of it.

At 99.5% you are telling customers you may be down for 3 hours 36 minutes a month. An enterprise buyer will read that as a service that fails during working hours.

At 99.99% you are down to 4 minutes 19 seconds a month. Someone has to be paged, wake up, read, understand the problem and act, inside four minutes, every time. You cannot fix that by hiring. The answer is automatic failover between regions, which is a much larger bill.

99.9% is the last target where a person can still be part of the fix. That is how it became the standard number.

## Promise less than you aim for

The safer way to use 99.9% is as the contract number only, with a tighter target inside the company. [99.95%](/blog/how-much-downtime-is-99-95-uptime) allows 21 minutes 36 seconds a month, which is half as much.

The reason is arithmetic. If your internal target and your contract number are the same, then the first month you miss the target is also the first month you owe credits and have to explain yourself to a customer. Leave a gap and your own alarm goes off while half the contract budget is still unused. You find out before the customer does. Google's SRE book calls this [keeping a safety margin](https://sre.google/sre-book/service-level-objectives/).

## Reading a 99.9% SLA that you are buying

The percentage is only part of [what an uptime SLA promises](/blog/uptime-sla). Two other things decide what it is worth.

Start with what counts as down. Some contracts measure only a total outage of the whole service, so a service where half the requests fail, or where every request takes 30 seconds, can still count as up all month. The definition sets the value of the number above it.

Then find what the credit pays. It is almost always a percentage of that month's bill, as in the [AWS compute SLA](https://aws.amazon.com/compute/sla/). If you pay $200 a month and the provider is down for four hours, you get back some part of $200, and your own customers get whatever your own contract promises them. The credit reduces your invoice. It does not cover what the outage cost you, and an SLA has never kept a service running.

## You cannot claim a number you have not measured

43 minutes a month is short enough that guessing does not work. If your only record of an outage is a support ticket, then your uptime number is really the share of time when nobody complained.

To measure it you have to check from outside your own infrastructure, on a fixed interval, and keep every failure whether or not someone was awake. A monitor inside the same network misses the outage that takes the whole network down, and that is the outage that uses your whole month. This is what [external uptime monitoring](/uptime-monitoring-for-developers) does. The number stays honest only if your status page builds it from those checks instead of letting someone set it by hand, which is the idea behind [a status page you cannot fake](/blog/status-page-you-cant-fake).

For the targets next to this one: [99.95% allows 21 minutes 36 seconds a month](/blog/how-much-downtime-is-99-95-uptime), [99.99% allows 4 minutes 19 seconds](/blog/how-much-downtime-is-99-99-uptime), and [98% allows 14.4 hours](/blog/is-98-uptime-good). Any other percentage goes through the [uptime SLA calculator](/tools/uptime-sla-calculator). To see how fast a live incident is using the month, use the [error budget calculator](/tools/error-budget-calculator).

## Common questions

<details class="mk-faq">
<summary>How much downtime is 99.9% uptime?</summary>
<div class="mk-faq__body">

1 minute 26 seconds a day, 10 minutes 5 seconds a week, 43 minutes 12 seconds per 30-day month, and 8 hours 46 minutes a year. Multiply any period by 0.001.

</div>
</details>

<details class="mk-faq">
<summary>How many hours per month is 99.9% uptime?</summary>
<div class="mk-faq__body">

0.72 hours, which is 43 minutes 12 seconds in a 30-day month. Over a year that is 8.76 hours.

</div>
</details>

<details class="mk-faq">
<summary>Is 99.9% uptime good?</summary>
<div class="mk-faq__body">

It is the normal promise for a customer-facing SaaS product, and a small team can reach it without waking up for every problem. It is not enough for payments or safety systems, where four minutes at the wrong moment costs more than the credit pays back.

</div>
</details>

<details class="mk-faq">
<summary>What is the difference between a 99.9% SLA and a 99.9% SLO?</summary>
<div class="mk-faq__body">

The SLO is the target you use inside the company. The SLA is the number you promise in a contract, with a credit if you miss it. Teams that promise 99.9% usually aim for 99.95% internally, so the contract keeps some spare time.

</div>
</details>
