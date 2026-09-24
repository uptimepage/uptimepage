+++
title = "How much downtime is 99.95% uptime? 21.6 minutes a month"
date = "2026-08-02"
slug = "how-much-downtime-is-99-95-uptime"
excerpt = "99.95% uptime allows 21 minutes 36 seconds of downtime a month and 4 hours 23 minutes a year. Why it is the target behind a 99.9% promise, not the promise."
tags = ["sla", "uptime", "slo", "downtime"]
draft = false
og_image = "/static/marketing/og-99-95-uptime.png"
cta_label = "Start measuring against 99.95%"

[[faqs]]
q = "How much downtime is 99.95% uptime?"
a = "99.95% uptime allows 43 seconds of downtime per day, 5 minutes 2 seconds per week, 21 minutes 36 seconds per 30-day month, and 4 hours 23 minutes per year. The allowed failure is 0.05% of the period, so multiply any period by 0.0005."

[[faqs]]
q = "How many minutes is 99.95% uptime?"
a = "21.6 minutes per 30-day month, which is 21 minutes 36 seconds. Per week it is 5.04 minutes and per year it is 262.8 minutes, or 4 hours 23 minutes."

[[faqs]]
q = "Why do companies aim for 99.95% but promise 99.9%?"
a = "So that their own alarm goes off while the contract still has spare time. If the target and the promise are the same number, the moment you miss the target you also owe service credits. Aiming for 99.95% behind a 99.9% contract leaves about 21 minutes a month to absorb a bad incident before you owe anything."

[[faqs]]
q = "Is 99.95% uptime better than 99.9%?"
a = "It allows half the downtime: 21 minutes 36 seconds a month instead of 43 minutes 12 seconds. Whether it is better for you depends on whether those extra 21 minutes of safety are worth the work of recovering twice as fast, which usually means automating a step a person does today."
+++

> **TL;DR.** 99.95% uptime allows **21 minutes 36 seconds** of downtime per 30-day month, or **4 hours 23 minutes** a year. That is half of the 99.9% budget. The number works better as an internal target behind a 99.9% contract than as the contract itself, because then your own alarm goes off while the contract still has spare time. For any other target, use the [uptime SLA calculator](/tools/uptime-sla-calculator).

99.95% is unusual, because it is more often a target than a promise. You see it on internal dashboards and in engineering plans far more often than in a contract a customer signs. That is worth understanding before you use the number anywhere.

First the math. The allowed failure is 0.05%, so multiply the period by 0.0005.

## 99.95% uptime in real time

| Period | Allowed downtime |
|--------|------------------|
| Per day | 43 seconds |
| Per week | 5 minutes 2 seconds |
| Per 30-day month | **21 minutes 36 seconds** |
| Per quarter (90 days) | 1 hour 5 minutes |
| Per year (365 days) | 4 hours 23 minutes |

## The space between the target and the promise

If your internal target and your contract number are the same, they fail on the same day. The first month you miss your engineering goal is also the first month you owe [service credits](/blog/uptime-sla), write an apology and put an account at risk. There is no space between the technical problem and the business problem.

Setting the internal target at 99.95% behind a [99.9% contract](/blog/how-much-downtime-is-99-9-uptime) gives you about 21 minutes of that space every month. When you pass 21 minutes 36 seconds, your own alarm goes off and the team treats the month as bad, while the customer is still inside the promise you sold them. That is the difference between hearing about the problem from your monitoring and hearing about it from your customer. Google's SRE book calls this [keeping a safety margin](https://sre.google/sre-book/service-level-objectives/).

This is the same idea as an error budget, measured in minutes instead of a percentage. When the budget for the month is used up, the sensible response is to stop shipping risky changes until the next month starts, which is [what error budgets are for](/blog/error-budgets-explained).

## 21 minutes is shorter than a normal recovery

At 99.9% a person can still be part of the fix. At 99.95% that is much harder. Twenty one minutes has to cover detection, someone seeing the alert, understanding what broke, deciding what to do, and the fix taking effect. If any one of those steps is slow, the month is used up.

To fit inside 21 minutes you have to take the slowest human step out of the recovery rather than try to speed it up. Usually that step is the rollback, so the fix is a deploy that undoes itself when its health check fails, with nobody deciding anything. Sometimes it is a load balancer removing a bad instance on its own.

## Dependencies multiply

This is the part that surprises people. Say your service needs two other services to answer a request, and each one is at 99.95%. Your best possible number is the two multiplied together:

`0.9995 × 0.9995 = 0.99900`

Two dependencies at 99.95% give you 99.9%, before your own code fails even once. Add a third and you are at 99.85%, which is below the contract you were about to sign. Because availability multiplies, a chain always comes out worse than any single part of it.

Asking your vendors for better numbers will not fix this. What fixes it is not needing all of them at the same moment. You can cache the answer, or show a smaller version of the feature instead of an error page, or move the work to a background job so a slow dependency delays a job and not a customer.

## How often you check sets a floor

There is a limit on what you can honestly claim, and your check interval sets it.

A monitor that runs every 60 seconds measures outages to about one minute. A budget of 21 minutes 36 seconds a month is about 21 failed checks at that interval. That is enough to be useful, so 60 seconds is a reasonable interval for a 99.95% target. But a 90 second failure can happen entirely between two checks and never appear at all. And a five minute interval cannot tell 99.95% and 99.99% apart, because both budgets are smaller than one gap between checks.

If you are promising a number in this range, your monitoring needs three things:

- Checks every 60 seconds or faster.
- Checks from outside your own network.
- Checks from more than one place, so a network problem near a single probe does not look like an outage.

The last point matters more here than at easier targets. One false alarm is a large share of the whole month.

For the targets next to this one: [99.9% allows 43 minutes 12 seconds a month](/blog/how-much-downtime-is-99-9-uptime), [99.99% allows 4 minutes 19 seconds](/blog/how-much-downtime-is-99-99-uptime), and [98% allows 14.4 hours](/blog/is-98-uptime-good). The [uptime SLA calculator](/tools/uptime-sla-calculator) handles every other target.

## Common questions

<details class="mk-faq">
<summary>How much downtime is 99.95% uptime?</summary>
<div class="mk-faq__body">

43 seconds a day, 5 minutes 2 seconds a week, 21 minutes 36 seconds per 30-day month, and 4 hours 23 minutes a year. Multiply any period by 0.0005.

</div>
</details>

<details class="mk-faq">
<summary>How many minutes is 99.95% uptime?</summary>
<div class="mk-faq__body">

21.6 minutes per 30-day month. Per week it is 5.04 minutes, and per year 262.8 minutes.

</div>
</details>

<details class="mk-faq">
<summary>Why do companies aim for 99.95% but promise 99.9%?</summary>
<div class="mk-faq__body">

So that their own alarm goes off while the contract still has spare time. If the two numbers are the same, the engineering failure and the business failure happen on the same day.

</div>
</details>

<details class="mk-faq">
<summary>Is 99.95% uptime better than 99.9%?</summary>
<div class="mk-faq__body">

It allows half the downtime: 21 minutes 36 seconds a month instead of 43 minutes 12 seconds. Getting there usually means automating whichever recovery step a person does today.

</div>
</details>
