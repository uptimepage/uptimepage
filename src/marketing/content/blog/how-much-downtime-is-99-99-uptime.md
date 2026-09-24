+++
title = "How much downtime is 99.99% uptime? 4.3 minutes a month"
date = "2026-08-02"
slug = "how-much-downtime-is-99-99-uptime"
excerpt = "99.99% uptime allows 4 minutes 19 seconds of downtime a month and 52 minutes 34 seconds a year. Four nines is a design decision, not a question of effort."
tags = ["sla", "uptime", "slo", "downtime"]
draft = false
og_image = "/static/marketing/og-99-99-uptime.png"
cta_label = "Check from outside, every 60 seconds"

[[faqs]]
q = "How much downtime is 99.99% uptime?"
a = "99.99% uptime allows 8.6 seconds of downtime per day, 1 minute per week, 4 minutes 19 seconds per 30-day month, and 52 minutes 34 seconds per year. The allowed failure is 0.01% of the period, so multiply any period by 0.0001."

[[faqs]]
q = "Is 99.99% uptime realistic?"
a = "Only with automatic recovery. Four minutes 19 seconds a month is shorter than the time it takes to page a person, have them wake up, read the alert, find the fault and act. Systems that reach four nines move traffic away from the problem without waiting for a person to decide, so it is a design decision rather than a question of effort or staffing."

[[faqs]]
q = "What is the difference between 99.9% and 99.99% uptime?"
a = "Ten times stricter. 99.9% allows 43 minutes 12 seconds of downtime a month, and 99.99% allows 4 minutes 19 seconds. The extra nine usually costs a second full copy of the system in another location, which is why the two are priced very differently."

[[faqs]]
q = "Is a 100% uptime SLA real?"
a = "The promise is real, but it does not mean what it looks like. A 100% SLA does not say the service never goes down. It says that any downtime at all gives you a credit. Read the exceptions and the measurement period, because those decide what counts as downtime, and they are usually the reason the number is affordable to promise."
+++

> **TL;DR.** 99.99% uptime allows **4 minutes 19 seconds** of downtime per 30-day month, or **52 minutes 34 seconds** a year. That is shorter than the time it takes to page a person and have them act, so four nines is not a target you reach by trying harder. You reach it by making recovery automatic and removing every single point of failure. For other targets, use the [uptime SLA calculator](/tools/uptime-sla-calculator).

Four nines goes into contracts because people read it as "one nine better than three nines". It is ten times stricter, and what has to change to reach it is the design of the system rather than the effort of the team.

The allowed failure is 0.01%, so multiply the period by 0.0001.

## 99.99% uptime in real time

| Period | Allowed downtime |
|--------|------------------|
| Per day | 8.6 seconds |
| Per week | 1 minute |
| Per 30-day month | **4 minutes 19 seconds** |
| Per quarter (90 days) | 12 minutes 58 seconds |
| Per year (365 days) | 52 minutes 34 seconds |

## The budget is shorter than a human answer

List what has to happen when something breaks. The monitor notices, an alert is sent, and a person receives it and stops what they are doing. They open a laptop, work out which of several things is wrong, and apply a fix that then needs time to take effect. All of that has to finish inside four minutes, at three in the morning as reliably as at three in the afternoon.

That is not a realistic thing to ask of people, so systems at this level do not ask it. The person is removed from the recovery entirely. A health check takes a bad server out of service, a failed deploy undoes itself, and traffic moves away from a failing region without anyone approving the move. Somebody is still paged, but they arrive to study an incident that already stopped.

That is what separates [99.9%](/blog/how-much-downtime-is-99-9-uptime) from 99.99%. At three nines a person can be part of the fix and the target still holds. At four nines they cannot.

## One copy of anything sets a limit

Once the budget is four minutes a month, the math starts ruling out whole designs.

A single database with a manual failover cannot do it, because promoting a replica by hand takes longer than the whole monthly budget. A single region cannot do it either, because your provider's own maintenance and network events will pass four minutes in some month. Frequent deploys cannot do it if each one drops connections for 30 seconds, since nine releases would use everything you have.

So four nines has a price, and you pay it in three places. You need a second full copy of the system somewhere else. You need routing that moves traffic between the copies without a person. And you need regular proof that the failover works. That third one gets skipped most often. Skipping it means you find out during a real incident that the standby could never have taken the traffic.

## Dependencies cost more at this level

Availability multiplies. If a request needs four other services, and each one really is at 99.99%, then your best possible number before your own code runs is:

`0.9999 × 0.9999 × 0.9999 × 0.9999 = 0.99960`

That is 99.96%, which allows 17 minutes 17 seconds a month. You have already missed 99.99% by four times, and nothing has gone wrong yet. Every call your request has to wait for costs part of the budget, and no setting makes four required services more reliable than each of them.

This is why systems at this level are built so that most dependencies are not required for most requests. Cache the result, show a smaller version of the feature, or move the work to a queue. The goal is to reduce the number of things that must all work at the same moment.

## Reading a 99.99% SLA that you are buying

The percentage is the least useful part of [an uptime SLA](/blog/uptime-sla).

The measurement period does more work than the number. A monthly period and a yearly period are different promises at the same percentage. Holding 4 minutes 19 seconds in every single month is much harder than spreading 52 minutes 34 seconds anywhere across a year.

Then read the exceptions. Planned maintenance is usually excluded, so is anything the provider can call a customer configuration error, and often so are regional events. A narrow definition of downtime makes a high percentage cheap to promise. That is usually why the high one is on offer.

Last, find out what counts as down at all. It might mean a complete outage, or a high error rate, or slow responses. If only a complete outage counts, a service that fails a third of its requests can stay inside its SLA all month.

## About 100% uptime SLAs

Some providers advertise 100%, [Cloudflare's Business SLA](https://www.cloudflare.com/business-sla/) among them. It is not a claim that the service never fails, and reading it that way is the mistake. It means the amount of downtime needed to earn a credit is zero, so any outage at all gives you money back. Whether that is generous depends on the exceptions and on the size of the credit. Take a 100% SLA with wide maintenance exceptions and a 10% limit on the credit. It can be worth less than an honest 99.9% with a narrow definition and a bigger payout.

In every case the credit pays back part of your bill. It does not pay back your customers' time, and no percentage in a contract keeps anything running.

## You cannot measure four nines with slow checks

There is a limit on what you can honestly claim, and your check interval sets it.

A monthly budget of 4 minutes 19 seconds is about four failed checks at a 60 second interval, and roughly one at a five minute interval. At five minute checks you cannot tell 99.99% and 99.999% apart, because both budgets fit inside a single gap between checks. Anyone reporting four nines from a five minute monitor is describing their tools, not their service.

A number in this range needs four things from your monitoring:

- Checks every 60 seconds or faster.
- Checks from outside your own infrastructure.
- Checks from more than one region, so a single network path cannot create a fake outage.
- A confirmation step, so a short failure is not recorded as downtime straight away.

The last one matters most here. At four minutes a month, one false alarm is a quarter of the budget.

For the targets next to this one: [99.95% allows 21 minutes 36 seconds a month](/blog/how-much-downtime-is-99-95-uptime), [99.9% allows 43 minutes 12 seconds](/blog/how-much-downtime-is-99-9-uptime), and [98% allows 14.4 hours](/blog/is-98-uptime-good). Any other percentage goes through the [uptime SLA calculator](/tools/uptime-sla-calculator).

## Common questions

<details class="mk-faq">
<summary>How much downtime is 99.99% uptime?</summary>
<div class="mk-faq__body">

8.6 seconds a day, 1 minute a week, 4 minutes 19 seconds per 30-day month, and 52 minutes 34 seconds a year. Multiply any period by 0.0001.

</div>
</details>

<details class="mk-faq">
<summary>Is 99.99% uptime realistic?</summary>
<div class="mk-faq__body">

Only with automatic recovery. Four minutes is shorter than paging a person, having them find the fault and act, so the failover has to happen without a person deciding.

</div>
</details>

<details class="mk-faq">
<summary>What is the difference between 99.9% and 99.99% uptime?</summary>
<div class="mk-faq__body">

Ten times stricter: 43 minutes 12 seconds a month against 4 minutes 19 seconds. The extra nine usually costs a second full copy of the system in another location.

</div>
</details>

<details class="mk-faq">
<summary>Is a 100% uptime SLA real?</summary>
<div class="mk-faq__body">

The promise is real, but it does not mean what it looks like. It does not say the service never goes down, only that any downtime at all gives you a credit. The exceptions and the measurement period decide what it is worth.

</div>
</details>
