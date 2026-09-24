+++
title = "What is an uptime SLA? Percentages, credits and fine print"
date = "2026-08-06"
slug = "uptime-sla"
excerpt = "An uptime SLA is a contract promise with a penalty, not a measurement. What each percentage allows, what a credit really pays, and the clauses that decide it."
tags = ["sla", "uptime", "reliability", "downtime"]
draft = false
cta_label = "Measure your real uptime"

[[faqs]]
q = "What is an uptime SLA?"
a = "An uptime SLA is a contract clause where a provider promises a minimum availability over a billing period and owes you something if it misses. The promise is usually a percentage, the penalty is usually a service credit on your next bill, and the contract defines what counts as downtime. Uptime is the measured number. An SLA is the promise about that number."

[[faqs]]
q = "What is a good uptime SLA?"
a = "99.9% is the normal target for a customer-facing service. It allows 43 minutes 12 seconds of downtime in a 30-day month, which leaves room for deploys and small failures. 99.95% and 99.99% are common for infrastructure other companies build on. Each extra nine costs roughly ten times more engineering, so pick the one your customers actually need."

[[faqs]]
q = "What does a service credit actually pay?"
a = "A percentage of what you paid the provider for the affected period, not what the outage cost you. A 10% credit on a $200 month is $20, however much revenue you lost while the service was down. Credits are also usually capped at the monthly fee and have to be claimed within a window, often 30 days, or they expire."

[[faqs]]
q = "Does scheduled maintenance count against an SLA?"
a = "Almost never. Most SLAs exclude announced maintenance windows, faults on your side of the connection, third-party network failures and force majeure. Many also ignore outages shorter than a minimum duration. Those exclusions decide the number far more than the percentage does."

[[faqs]]
q = "How is uptime measured for an SLA?"
a = "Whoever writes the SLA usually also defines the measurement, and the check interval sets the floor on what can be detected. A monitor checking every five minutes cannot see a two-minute outage. If the contract does not say who measures, how often, and from where, the percentage is not a number you can hold anyone to."
+++

> **TL;DR.** An uptime SLA is a contract promise with a penalty, not a measurement. The percentage sets the allowance: 99.9% permits 43 minutes 12 seconds of downtime a month, 99.99% permits 4 minutes 19 seconds. The penalty is normally a service credit worth a slice of your own bill, and you usually have to claim it. The clauses defining what counts as downtime do more work than the number does. Run any target through the [uptime SLA calculator](/tools/uptime-sla-calculator).

## Uptime is measured, an SLA is promised

These get used as if they were the same thing, and they are not.

Uptime is a measurement: the share of a period your service actually worked. It exists whether or not anyone wrote it down.

An uptime SLA is a clause in a contract. A provider promises a minimum availability over a billing period, and if they miss it they owe you something. The promise does not make the service more reliable. It puts a price on failing.

That gap matters when you read a vendor's page. A provider can promise 99.9% and deliver 98%. What you get for the difference is a credit, and the credit is set by the contract rather than by your losses.

## What each percentage allows

The allowance is the leftover: subtract the target from 100% and apply it to the window.

| Target | Per 30-day month | Per year |
| ------ | ---------------- | -------- |
| 98% | 14h 24m | 7.3 days |
| 99% | 7h 12m | 3.65 days |
| 99.5% | 3h 36m | 1.83 days |
| 99.9% | 43m 12s | 8h 45m 36s |
| 99.95% | 21m 36s | 4h 22m 48s |
| 99.99% | 4m 19s | 52m 34s |
| 99.999% | 26s | 5m 15s |

Two things fall out of that table. The first is how quickly the numbers stop being reassuring: 99% sounds close to perfect and permits more than seven hours a month. The second is how fast the top end gets expensive. Going from 99.9% to 99.99% removes 39 minutes a month, and buying those 39 minutes usually means redundancy across regions, automated failover and someone on call who is paid to be woken up.

Each of the common targets has its own breakdown: [98% and what it really allows](/blog/is-98-uptime-good), [99.9% at 43 minutes 12 seconds a month](/blog/how-much-downtime-is-99-9-uptime), [99.95% at 21 minutes 36 seconds](/blog/how-much-downtime-is-99-95-uptime), and [99.99% at 4 minutes 19 seconds](/blog/how-much-downtime-is-99-99-uptime).

## The credit is a refund of your bill, not your losses

This is the part that surprises people the first time they claim.

A service credit pays back a percentage of what you paid the provider for the affected period. It does not pay back what the outage cost you. If you spend $200 a month and a bad month earns you a 10% credit, you get $20. Your own lost revenue, the support load and the customers who left are not in the calculation.

Three details worth reading before you rely on one:

Credits are usually tiered. Miss by a little and you get a small percentage back, miss badly and you get more, but the ladder is normally capped at the monthly fee for that service. The provider's worst case is giving you the month for free. The [EC2 SLA](https://aws.amazon.com/compute/sla/) is a typical ladder: 10%, 30%, then 100% of the bill.

Credits usually have to be claimed. They are rarely automatic. Most contracts give you a window, often 30 days from the incident, and require you to submit the request with your own evidence. Miss the window and the credit is gone even though the outage happened.

Credits are often the sole remedy. Contracts commonly state that the credit is the only compensation available for missed availability, which closes off other claims. AWS and [Google Cloud](https://cloud.google.com/compute/sla) both call it the "sole and exclusive remedy".

## The definition of downtime does more work than the number

If you only read one part of an SLA, read the exclusions rather than the percentage.

Most contracts do not count announced maintenance windows, faults on your side of the connection, failures in networks neither party controls, problems caused by your own configuration, or beta and preview features. Many also ignore any outage shorter than a minimum duration, so a service that fails for 90 seconds twice a day can still report a clean month.

The result is that two providers advertising the same 99.9% can offer very different promises. The one that excludes unlimited scheduled maintenance is promising less than the one that counts it, and the number on the marketing page is identical.

## How it gets measured decides the number

An availability percentage is only as meaningful as the measurement behind it.

The check interval sets a floor on what can be seen at all. A monitor running every five minutes cannot detect a two-minute outage, so a service checked that way will report better availability than one checked every 60 seconds, without being more reliable.

Where you measure from matters too. A single probe in one region reports that region's view, including its own network problems. Several probes with a confirmation rule separate a real outage from one probe having a bad minute.

Who measures matters most. If the provider both defines downtime and reports it, the number is self-assessed. That is why an independent measurement is worth having, even when you trust the vendor: it gives you the evidence a credit claim needs.

## Which target to pick

Start from what breaks when you are down, not from a number that sounds good.

For internal tools, staging and anything where nobody loses money during an outage, 99% or 99.5% is honest and cheap. For a public site or a paid API, 99.9% is the normal floor, and customers will ask about it during procurement. For infrastructure other companies build their own products on, 99.95% and above is expected, because your downtime becomes their downtime.

Then check you can measure the target you picked. Promising 99.99% means promising to notice a four-minute problem, which a five-minute check interval cannot do. Work out how quickly an incident eats the allowance in the [error budget calculator](/tools/error-budget-calculator), and if you want the uptime figure on your own status page to mean something, publish [a bar measured from checks rather than from what you chose to announce](/blog/status-page-you-cant-fake).

## Common questions

<details class="mk-faq">
<summary>What is an uptime SLA?</summary>
<div class="mk-faq__body">

An uptime SLA is a contract clause where a provider promises a minimum availability over a billing period and owes you something if it misses. Uptime is the measured number. An SLA is the promise about that number, with a penalty attached.

</div>
</details>

<details class="mk-faq">
<summary>What is a good uptime SLA?</summary>
<div class="mk-faq__body">

99.9% for a customer-facing service, which allows 43 minutes 12 seconds a month. 99.95% and 99.99% for infrastructure other companies build on. Each extra nine costs roughly ten times more engineering.

</div>
</details>

<details class="mk-faq">
<summary>What does a service credit actually pay?</summary>
<div class="mk-faq__body">

A percentage of what you paid the provider for the affected period, not what the outage cost you. A 10% credit on a $200 month is $20. Credits are usually capped at the monthly fee and have to be claimed within a window.

</div>
</details>

<details class="mk-faq">
<summary>Does scheduled maintenance count against an SLA?</summary>
<div class="mk-faq__body">

Almost never. Most SLAs exclude announced maintenance, faults on your side, third-party network failures and outages under a minimum duration. Those exclusions decide the number more than the percentage does.

</div>
</details>

<details class="mk-faq">
<summary>How is uptime measured for an SLA?</summary>
<div class="mk-faq__body">

Whoever writes the SLA usually also defines the measurement, and the check interval sets the floor on what can be detected. If the contract does not say who measures, how often and from where, the percentage is not something you can hold anyone to.

</div>
</details>
