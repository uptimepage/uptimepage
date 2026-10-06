+++
title = "Google says on-call needs eight engineers. You have three."
meta_title = "On-call rotation for a small team: what works with three people"
date = "2026-10-06"
slug = "on-call-rotation-small-team"
excerpt = "Google's SRE book says a 24/7 on-call rotation needs eight engineers. What a team of two or three can change instead: the pages, the hours and the ladder."
tags = ["on-call", "alerting", "incident-response", "reliability", "monitoring"]
draft = false
og_image = "/static/marketing/og-on-call-rotation-small-team.png"
cta_label = "Set up your on-call rotation"

list_items = [
  "Decide what is worth a page",
  "Name one person at a time",
  "Split the day before you split the week",
  "Add a ladder for the unanswered page",
  "Make swaps cheap",
  "Test that the page arrives",
  "Count the pages",
]

[[faqs]]
q = "How many people do you need for an on-call rotation?"
a = "Google's SRE book puts the minimum for a 24/7 rotation at eight engineers on one site, with two people on call at a time and nobody on call more than a quarter of the time. A smaller team can still run a rotation, but it has to cover fewer hours, page on fewer alerts, or accept a slower response."

[[faqs]]
q = "Can a team of two or three run on-call?"
a = "Yes, with one person on call at a time and a short escalation ladder behind them. Three people on week-long shifts are each on call a third of the time, which is over Google's 25% limit, so the week has to be quiet enough to live with."

[[faqs]]
q = "What is an escalation policy?"
a = "An escalation policy is an ordered list of who gets paged next when a page goes unanswered. Level one pages when the incident opens, and each level waits a set time for an acknowledgement before the next one pages."

[[faqs]]
q = "How long should an escalation level wait?"
a = "The wait comes from the response time you promised. Google's SRE book gives 5 minutes as a typical response time for user-facing services and 30 minutes for less time-sensitive ones, and the wait before the next level has to fit inside that."

[[faqs]]
q = "Should alerts go to a shared channel or to one person?"
a = "A page should go to one named person, and the shared channel can get a copy. When five people receive the same alert at night, each can assume one of the others has it."
+++

> **TL;DR**
>
> Google's SRE book works out that a 24/7 on-call rotation needs at least eight engineers. A team of two or three cannot reach that number, so it has to change what the number is built from: which alerts page a person, which hours are covered, and how fast someone must answer. After that it needs one named person on call at any moment and a short escalation ladder behind them for the page nobody answers. A weekly count of pages shows whether it is working.

## "We are not Google"

Google's first SRE book has a chapter called [Being On-Call](https://sre.google/sre-book/being-on-call/). Readers wrote back. The follow-up book, the SRE Workbook, opens its own [on-call chapter](https://sre.google/workbook/on-call/) with their feedback, and the first item on the list is this:

> "We are not Google; we're much smaller. We don't have as many people in the rotation, and we don't have sites in different time zones. What you described in your first book is irrelevant to me."

I build [Uptimepage](/), which pages people when a monitor goes down, so I had to decide what a rotation should look like for a team that small. That reader is right about the numbers. The reasoning behind the numbers still works for three people, and it leads to a different setup.

## Where the eight comes from

The book starts from a limit on time:

> "we strive to invest at least 50% of SRE time into engineering: of the remainder, no more than 25% can be spent on-call"

Then it does the arithmetic:

> "Assuming that there are always two people on-call (primary and secondary, with different duties), the minimum number of engineers needed for on-call duty from a single-site team is eight: assuming week-long shifts, each engineer is on-call (primary or secondary) for one week every month."

Two people on call, each for at most a quarter of the time, makes eight. Run the same arithmetic on a small team, with one person on call at a time and week-long shifts:

| Team size | Share of weeks each person is on call |
|---|---|
| 2 | 50% |
| 3 | 33% |
| 4 | 25% |
| 8, with two on call at a time | 25% |

A team of four reaches Google's limit only by giving up the secondary. A team of three is over the limit before anything breaks, and a team of two is on call every other week.

A schedule cannot fix that, because any rotation of three people still has three people in it. What the team can change is how often the pager goes off, during which hours, and how quickly it has to be answered.

The limit exists because pages are expensive. The same chapter estimates that one incident, with its diagnosis, fix and follow-up, takes about six hours of work, which is why it allows at most two incidents in a 12-hour shift. A person on call every third week can live with that week if it is quiet. So the work for a small team is to make the week quiet, then make sure the rare page reaches someone.

## What a team of three can change

### Decide what is worth a page

The Workbook is direct about this:

> "Engineers shouldn't have to be at a computer and working on a problem within minutes of receiving a page unless there is a very good reason to do so."

Its Table 8-1 gives three examples of a realistic response:

| Incident | Response time |
|---|---|
| Revenue-impacting network outage | 5 minutes |
| Customer order batch processing system stuck | 30 minutes |
| Backups of a database for a pre-launch service are failing | Ticket (response during work hours) |

Sort every alert you have into those three rows. Only the first two should reach a phone at night. The rest go to a channel or an inbox that somebody reads in the morning.

An alert has to pass two more filters before it may page. It has to be real: one probe with a bad network path [should not wake anyone](/blog/stop-false-uptime-alerts). And it has to come with something to do. The SRE book's wording is "All paging alerts should also be actionable." A page that can only be watched until it clears teaches people to ignore pages.

### Name one person at a time

The usual start for a small team is a shared channel: every alert goes to `#alerts`, and everyone is in it. During the day this works, because somebody is looking. At 03:00 five people receive the message, and each of them can reasonably think one of the other four has it.

A rotation puts exactly one name on call at any moment. That person knows it is them, and everyone else knows it is not. The channel can still get a copy, but the page goes to the person.

Hand off on a weekday morning, when both people are awake and can talk about what is still open. A handoff at midnight on Sunday means nobody mentions the disk that is filling up. Keep it at the same local time all year too, so the change does not move by an hour when the clocks do.

### Split the day before you split the week

The Workbook does not expect one person to carry a full week around the clock:

> "In our experience, 24 hours of on-call duty without reprieve isn't a sustainable setup."

Its suggestion fits a small team too: "it would be better for two engineers to split a week of on-call, with one person on-call during the day and one on-call overnight."

With three people that can be two rotations in one schedule. Working hours rotate weekly among all three. Nights and weekends rotate separately, with the order shifted by one, so nobody carries both in the same week.

There is also a cheaper option: leave some hours uncovered on purpose. If your customers are businesses in one region and nothing you run loses data while it is down, a page at 02:00 may only move the fix from 08:00 to 02:30. Decide that as a team, then write it where customers can see it, in your [SLA](/blog/uptime-sla) and on your status page. As a customer I would rather read a stated response window during European working hours than a 24/7 promise kept by one tired person.

Do the arithmetic before you choose. A 99.9% target leaves [43 minutes a month](/blog/how-much-downtime-is-99-9-uptime). One night outage that waits six hours for the morning uses more than eight months of that budget. If the number you promised cannot absorb that, someone has to be on call at night.

### Add a ladder for the unanswered page

One name on call is one point of failure. Phones run out of battery, and a person who was paged twice last night can sleep through the third.

An escalation policy is an ordered list of who gets paged next when a page goes unanswered. Level one pages when the incident opens, and each level waits a set time for an acknowledgement before the next one pages. Acknowledging stops the ladder.

For three people a ladder can be this short:

1. The person on call. Wait 10 minutes.
2. The other two. Wait 15 minutes.
3. Everyone, on the loudest channel each person has.

How long to wait comes from the response time you promised. The SRE book's typical values are "5 minutes for user-facing or otherwise highly time-critical services, and 30 minutes for less time-sensitive systems". If level one has 5 minutes to answer, the wait before level two cannot be 30.

This ladder is the small team's version of Google's secondary. Nobody is formally second on call, and the page still has somewhere to go. Watch how often level two fires. If it fires most weeks, the other two people are on call as well, whatever the schedule says.

### Make swaps cheap

The Workbook again:

> "No one can promise on Monday that they won't have the flu on Thursday."

On a team of eight a swap is easy to arrange. On a team of three every swap lands on one of two people, so it has to take a minute and leave no doubt about who holds the pager afterwards. That means an override on the schedule itself, for exact dates, which ends without anyone remembering to undo it. A chat message asking a colleague to cover Thursday changes nothing about where the page goes.

Put the shifts in everyone's calendar as well. A person who sees an on-call week next to a dentist appointment three weeks ahead asks for the swap now instead of on the day.

### Test that the page arrives

A schedule can name a person the page cannot reach. Their phone is on do-not-disturb, the chat app is muted at night, or the email address was never confirmed. Everything looks covered until the first night page arrives on a silent phone.

So test it. On the first morning of a shift, send a test page to the person on call and have them acknowledge it from the phone they will have at night, with the settings they will have at night. Use at least one channel that is allowed to make noise through do-not-disturb, such as SMS from a number saved as an emergency contact, or a push app with an emergency priority.

GitLab's 2017 outage shows what an untested path costs. The failure alerts for its backup job were sent every night and rejected by the receiving mail server, and [nobody knew for months](/blog/cron-jobs-fail-silently).

### Count the pages

Google's teams work to a number: "We target a maximum of two incidents per on-call shift". The Workbook also describes a team with a budget of two paging incidents per shift that had been receiving five for a year. Their answer was a project to reduce the pages, approved by senior management.

A small team needs that count more than a large one does, because each page lands on a third of the team. Once a week, look at how many pages there were, how many of them needed a person, and how long the first acknowledgement took. A page that needed nobody is an alert to fix or delete. A slow acknowledgement is a reason to look at the ladder and the paging channel before it is a reason to look at the person.

## What this does not fix

Two people are still on call every other week, and no ladder changes that. What is left is to cover fewer hours or fewer systems and tell customers which, and to pay for the nights. The Workbook's position is short: "On-call usually implies some amount of out-of-hours work. We believe this work should be compensated."

Four is the first team size where one person on call at a time meets the 25% line. If on-call is wearing down a team of three, that is a real argument for the fourth hire, and the table above is how to make it.

## Common questions

<details class="mk-faq">
<summary>How many people do you need for an on-call rotation?</summary>
<div class="mk-faq__body">

Google's SRE book puts the minimum for a 24/7 rotation at eight engineers on one site, with two people on call at a time and nobody on call more than a quarter of the time. A smaller team can still run a rotation, but it has to cover fewer hours, page on fewer alerts, or accept a slower response.

</div>
</details>

<details class="mk-faq">
<summary>Can a team of two or three run on-call?</summary>
<div class="mk-faq__body">

Yes, with one person on call at a time and a short escalation ladder behind them. Three people on week-long shifts are each on call a third of the time, which is over Google's 25% limit, so the week has to be quiet enough to live with.

</div>
</details>

<details class="mk-faq">
<summary>What is an escalation policy?</summary>
<div class="mk-faq__body">

An escalation policy is an ordered list of who gets paged next when a page goes unanswered. Level one pages when the incident opens, and each level waits a set time for an acknowledgement before the next one pages.

</div>
</details>

<details class="mk-faq">
<summary>How long should an escalation level wait?</summary>
<div class="mk-faq__body">

The wait comes from the response time you promised. Google's SRE book gives 5 minutes as a typical response time for user-facing services and 30 minutes for less time-sensitive ones, and the wait before the next level has to fit inside that.

</div>
</details>

<details class="mk-faq">
<summary>Should alerts go to a shared channel or to one person?</summary>
<div class="mk-faq__body">

A page should go to one named person, and the shared channel can get a copy. When five people receive the same alert at night, each can assume one of the others has it.

</div>
</details>

## Sources

- Google, Site Reliability Engineering, chapter 11, [Being On-Call](https://sre.google/sre-book/being-on-call/).
- Google, The Site Reliability Workbook, chapter 8, [On-Call](https://sre.google/workbook/on-call/).
- GitLab, [Postmortem of database outage of January 31](https://about.gitlab.com/blog/postmortem-of-database-outage-of-january-31/), 10 February 2017.

## Where Uptimepage fits

I build [Uptimepage](/). It checks your services from several regions, and when one goes down it pages whoever is on call.

An [on-call schedule](/on-call-scheduling) holds one or more rotations, each on call at all hours or only in the hours you give it. Daily and weekly handoffs stay at the same local time across daylight saving. Overrides cover exact dates. Each person picks the channels that page them, and the schedule list names anyone on a rotation whom no channel can reach. An escalation policy is a ladder of levels with a wait on each, and acknowledging from the chat or email alert stops it. Each person gets a calendar link for their shifts, and the incident reports include mean time to acknowledge.

One gap to know about: there is no phone-call channel yet. SMS, Pushover's emergency priority, Telegram and the chat apps are there, and if you already use PagerDuty you can put it on a level. The [incident docs](/docs/incidents#on-call-schedules) have the details. It is open source under AGPL, so it can also run on your own server.

Whatever you use, write down the answer to one question. At 03:00 tonight, whose phone rings, and whose rings next if they do not answer?
