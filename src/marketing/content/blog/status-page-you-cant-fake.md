+++
title = "The status page you can't fake: measured uptime, not published"
date = "2026-07-15"
updated = "2026-08-08"
slug = "status-page-you-cant-fake"
excerpt = "Why a status page's uptime bar must come from real checks, not the incidents you chose to publish, and a quick test for any page."
tags = ["status-page", "monitoring", "trust", "uptime", "postmortem", "sre", "devops"]
draft = false
faqs = [
  { q = "Can you fake a status page?", a = "You can write anything you want in the incident notes. But a good status page does not let you fake the uptime bar. The bar comes from real checks and one confirmation rule, not from what a person chooses to publish." },
  { q = "Should people be able to set uptime by hand?", a = "No. People should control the incident notes, not the measured timeline. If a person can turn a red day green by hand, the uptime number stops meaning anything." },
  { q = "What is the difference between an incident and downtime?", a = "Downtime is what the checks measured. An incident is what the team writes about it. The timeline should come from the checks, and the story from the notes." },
  { q = "Why does my status page show 100% uptime after an outage?", a = "Usually because the bar is built from published incidents, not from real checks. If no public incident covers the outage, the bar stays green. A bar built from checks would show the red day." },
  { q = "How is uptime measured on a status page?", a = "A good status page counts downtime from its own checks, using a confirmation rule so one short blip does not count. The same rule should feed both the alerts and the bar, so they always agree." },
]
+++

A status page is the one dashboard a company publishes about its own service. It is also the one place where the company has a reason to look good. That is a problem. If the page can be edited to look better than reality, it stops being useful. So when you build or choose a status page, ask one thing: can someone hide a real outage on it?

The short answer: a status page you can trust builds its uptime bar from real checks, not from the incidents a person chose to publish.

## Two layers, and only one is yours to edit

A good status page has two parts. The first part is the measured data: the green and red timeline, the 90-day bar, the [uptime percent](/tools/uptime-sla-calculator). The second part is incidents: the notes a person writes to explain what broke, what they are doing, and when it is fixed. On the page they sit next to each other and look the same. They are not the same, and mixing them is where trust gets lost.

The measured data answers one question: what did the checks see? The incident notes answer a different one: what does the team want to say about it? The first is a fact. The second is a story. A status page you can trust lets the team write the story, but keeps them away from the facts.

The story layer also includes the postmortem, the write-up you post after an outage. A postmortem is honesty you add on purpose. You explain what broke and why, because you choose to. The bar works the other way. It shows the failure on its own, whether you write anything or not. You control the story. You do not control the facts.

## The trap: a bar that only shows what you published

The common mistake is to build the uptime bar from incidents. It feels natural. You already open an incident when something breaks, so why not color the timeline from incidents too? Now the bar turns red only where an incident exists and is marked public.

The problem comes on the day an outage has no public incident. Maybe no one published it. Maybe a setting was wrong. Maybe the monitor was added to the page after the outage, so its incident was saved as private and never checked again. In every case the timeline shows green over a real red day, and it does this quietly. The uptime number goes up. The customer sees 100 percent over a week they remember as broken.

One version of this trap is easy to build by accident, so it is worth explaining. You decide "is this incident public?" once, at the moment the incident opens, based on whether the monitor was on a public page right then. Then you save that answer and never look again. In the code it looks like a live check. It is really a photo taken one time. Move the monitor onto the page a day later, and its past outages stay hidden, because the photo was taken before the monitor was there. Any yes or no mark that is set once and then trusted forever has this problem.

## The fix is a rule, not a switch

The bar should come from the measured data. It needs only one rule to stay calm: wait for confirmation before you count downtime. You do not want one failed check from one place to turn a whole day red, because networks are noisy and one bad checker is not an outage. So you wait for agreement: more than one region failing at the same time, for more than one check in a row. That one rule is enough to keep a short blip from becoming a red day.

Here is a week under that rule. The three lanes are every check, the band underneath is only what the rule let through:

<div class="mk-embed-measured-week"></div>

One thing matters here: the same rule feeds both your alerts and your uptime bar. If the rule that wakes your on-call person is the same rule that colors the timeline, the two can never tell different stories. The moment you add a second rule just for the bar, it will drift from the first, and the page will disagree with itself. On-call gets paged, but the public history says everything was fine.

Publishing stays where it belongs, on the notes. The team decides whether to write an incident, what to say, and when to post the all-clear. They do not decide whether last Tuesday was down. [The checks](/blog/building-an-uptime-monitor-in-rust) already decided that.

## How to test the status page you already have

You can check any status page, including your own, in a few minutes.

- Add a monitor to a page after it has already had an outage. Does the history show the outage, or does it start clean from the day you added the monitor?
- Take a real incident and unpublish it. Does the bar keep the red day, or does the day turn green?
- Make one region fail for one second. Does the whole day go red, or does the bar stay calm?

A page you can trust shows the outage in the first test, keeps the red in the second, and stays calm in the third. A page that fails these is usually not lying on purpose. It just built its timeline on top of what people chose to publish, and that always has holes.

## Where we stand

This is how Uptimepage works. The 90-day bar and each status light come from confirmed downtime, measured across regions with a confirmation rule, not from what someone chose to publish. Incidents are the notes you write by hand. Some status page tools also let a person [set a component to green by hand](https://support.atlassian.com/statuspage/docs/what-is-a-component/). We do not. If you are comparing tools, we keep an honest [comparison with Statuspage](/vs/statuspage) and a wider review of the [Statuspage alternatives](/blog/statuspage-alternatives) worth knowing about. We hold our own pages to the same rule and keep improving it. Most recently we made sure that a monitor added to a page after an outage still shows that outage on the bar, so the history stays complete no matter when the monitor was added.

You should not be able to fake your uptime. You should not be able to fake it by accident either. The bar is a measurement. Keep it that way.

## Common questions

<details class="mk-faq">
<summary>Can you fake a status page?</summary>
<div class="mk-faq__body">

You can write anything you want in the incident notes. But a good status page does not let you fake the uptime bar. The bar comes from real checks and one confirmation rule, not from what a person chooses to publish.

</div>
</details>

<details class="mk-faq">
<summary>Should people be able to set uptime by hand?</summary>
<div class="mk-faq__body">

No. People should control the incident notes, not the measured timeline. If a person can turn a red day green by hand, the uptime number stops meaning anything.

</div>
</details>

<details class="mk-faq">
<summary>What is the difference between an incident and downtime?</summary>
<div class="mk-faq__body">

Downtime is what the checks measured. An incident is what the team writes about it. The timeline should come from the checks, and the story from the notes.

</div>
</details>

<details class="mk-faq">
<summary>Why does my status page show 100% uptime after an outage?</summary>
<div class="mk-faq__body">

Usually because the bar is built from published incidents, not from real checks. If no public incident covers the outage, the bar stays green. A bar built from checks would show the red day.

</div>
</details>

<details class="mk-faq">
<summary>How is uptime measured on a status page?</summary>
<div class="mk-faq__body">

A good status page counts downtime from its own checks, using a confirmation rule so one short blip does not count. The same rule should feed both the alerts and the bar, so they always agree.

</div>
</details>
