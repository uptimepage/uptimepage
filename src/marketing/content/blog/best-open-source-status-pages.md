+++
title = "Best open-source status pages you can self-host (2026)"
date = "2026-09-24"
slug = "best-open-source-status-pages"
excerpt = "Eleven open-source status pages compared: which ones monitor for you, which let customers subscribe, what each takes to run, and which ones are abandoned."
tags = ["open-source", "self-hosted", "status-page", "incidents"]
draft = false
cta_label = "Try a status page with monitoring built in"
list_items = [
    "Uptime Kuma",
    "Cachet",
    "Upptime",
    "OpenStatus",
    "Kener",
    "cState",
    "Gatus",
    "Checkmate",
    "OneUptime",
    "Statping-ng",
    "Uptimepage",
]

[[faqs]]
q = "What is the best open-source status page?"
a = "It depends on whether the page should watch your services itself. If you only want to publish incidents by hand, cState and Cachet do that well. If the page should turn red on its own when a check fails, pick a tool with monitoring built in, such as Uptime Kuma, OpenStatus, Checkmate or Uptimepage."

[[faqs]]
q = "Which open-source status pages let customers subscribe?"
a = "Cachet, OpenStatus, OneUptime, Kener and Uptimepage all keep a subscriber list and notify it by email, and several add webhooks. Uptime Kuma and cState offer RSS instead, which works for technical readers but not for most customers. Upptime, Gatus, Checkmate and Statping-ng have no subscriber list at all."

[[faqs]]
q = "Can I host a status page for free?"
a = "Yes. Upptime runs on GitHub Actions and GitHub Pages, and cState builds a static site you can host on Netlify, so neither needs a server. The trade-off is that Upptime checks at most every five minutes and cState does no monitoring at all. Every other tool here needs a server when you run it yourself, and some, including Uptimepage, also offer a free hosted tier."

[[faqs]]
q = "Is Cachet still maintained?"
a = "Cachet is being rebuilt as version 3, and the repository is active, but the newest tagged release is still v2.4.1 from November 2023. The v3 README says it is not yet completely ready for production use. If you pick Cachet today, you choose between an old stable release and an untagged development branch."
+++

> **TL;DR**
>
> Open-source status pages split into two groups. Some are only a page: you write each incident by hand, and the page knows nothing about whether your service is up. cState is the clearest example, and Cachet mostly works this way. Others run their own checks and show the results: Uptime Kuma, Upptime, OpenStatus, Kener, Gatus, Checkmate, OneUptime, Statping-ng and Uptimepage. Decide which group you need first. Then check two things most lists skip: whether customers can subscribe to updates, and whether the project still ships.

I build one of the tools on this list, Uptimepage, so read my section with that in mind. For every other tool I stick to what its own repository and documentation say. I checked stars, releases and last commits on the GitHub API on 24 September 2026, and I link each project so you can check them yourself.

## The three questions that decide it

Most status page comparisons list features. Three questions sort this category faster.

1. Does the page watch your services, or do you tell it what happened? A page-only tool shows whatever you last typed. It looks calm during an outage until someone updates it. A page with checks built in turns red on its own. I wrote about why that matters in [a status page you cannot fake](/blog/status-page-you-cant-fake).
2. Can customers subscribe? A status page mostly gets read during an outage, by people who were already affected. An email or webhook subscription tells them before they go looking. RSS works for developers, but most customers will not use it.
3. What do you have to run? A static site on free hosting, one container, and a stack of services are very different commitments.

| Tool | Checks built in | Customer subscribers | What you run | License |
| --- | --- | --- | --- | --- |
| Uptime Kuma | Yes | RSS only | One container | MIT |
| Cachet | Basic HTTP, v3 only | Email + webhook | PHP app + DB + queue + cron | BSD-3 (v2), source-available (v3) |
| Upptime | Yes, every 5 min at best | No | Nothing: GitHub Actions + Pages | MIT |
| OpenStatus | Yes | Email, webhook, Slack, RSS | Several services | AGPL-3.0 |
| Kener | Yes | Email + RSS | Node app + Redis | MIT |
| cState | No | RSS | Nothing: static Hugo site | MIT |
| Gatus | Yes | No | One small binary | Apache-2.0 |
| Checkmate | Yes | No | Node + MongoDB | AGPL-3.0 |
| OneUptime | Yes | Email, SMS, Slack | Docker or Kubernetes, many services | Apache-2.0 + enterprise |
| Statping-ng | Yes | No | One Go binary | GPL-3.0 |
| Uptimepage | Yes | Email + webhook | One binary + Postgres + ClickHouse | AGPL-3.0 |

## Uptime Kuma

[Uptime Kuma](https://github.com/louislam/uptime-kuma) is the default answer, with about 91,800 GitHub stars and a 2.5.5 release on 16 September 2026. It is a monitor first, with 31 check types in one container, and its status pages come with it. You can put a page on your own domain and announce maintenance.

The page itself is simple. Incidents are posted by hand, even though Kuma already knows when a check fails, and visitors can follow updates only by RSS. For a homelab or an internal page, that is enough. For a page your customers rely on, the missing subscriber list is the gap people hit first. [Uptime Kuma against Cachet](/compare/uptime-kuma-vs-cachet) shows the two approaches side by side.

## Cachet

[Cachet](https://github.com/cachethq/cachet) (about 15,200 stars) is the classic self-hosted status page, and it is status-page-first: components, incidents, scheduled maintenance, metrics, and subscribers by email and webhook. If your monitoring already lives somewhere else and you want a proper page to publish to, that model makes sense.

The caveat is the rebuild. The newest tagged release is v2.4.1 from November 2023. Version 3 ships from an untagged development branch whose README says it is not yet completely ready for production use, and it moves from the BSD license to a source-available one. Version 3 added a basic HTTP check, but you schedule it yourself, and a failure only colours a component. It does not open an incident.

## Upptime

[Upptime](https://github.com/upptime/upptime) (about 17,200 stars) has the cleverest design here. It runs checks as scheduled GitHub Actions, keeps history as commits in your repository, opens a GitHub Issue for each outage and publishes a static page on GitHub Pages, with your own domain if you like. There is no server and no bill.

The design is also the limit. A GitHub Actions schedule cannot run more often than every five minutes, and it often runs late, so short outages go unseen. There is no list your customers can subscribe to. For an open-source project or a personal site it is close to perfect. [Upptime against Uptime Kuma](/compare/uptime-kuma-vs-upptime) covers the trade-off in detail.

## OpenStatus

[OpenStatus](https://github.com/openstatusHQ/openstatus) (about 9,100 stars, AGPL-3.0) is the tool closest to what I build. It puts a status page and uptime checks in one product, opens incidents from failing checks, supports custom domains, and lets visitors subscribe by email, webhook, Slack or RSS. It also has a REST API and a Terraform provider.

Two things to know before self-hosting it. Its open-source checker implements HTTP, TCP and DNS checks; other types appear in its API schema but not in that checker. And running it yourself means operating several separate services, not one process. It ships continuously without tagged releases. [OpenStatus against Uptime Kuma](/compare/openstatus-vs-uptime-kuma) goes deeper.

## Kener

[Kener](https://github.com/rajnandan1/kener) (about 5,200 stars, MIT, v4.1.5 on 6 September 2026) calls itself a status page system and puts effort into how the page looks: logo, colours, custom CSS, themes, light and dark mode, localisation and embeddable badges. It runs its own checks (twelve types, including API, ping, TCP, DNS, SSL, SQL, gRPC, Docker, heartbeat and game servers), manages incidents with timelines, and schedules maintenance windows. Incidents are written by hand by default, and it can also open them from alert events if you turn that on.

Visitors can subscribe by email, confirmed with a one-time code, to incident and maintenance updates once you turn subscriptions on and set up email sending. Each page also has an RSS feed. Alerts to your own team go to email, webhook, Slack or Discord. It runs as a Node app with Redis. [Kener against Uptime Kuma](/compare/uptime-kuma-vs-kener) compares the two.

## cState

[cState](https://github.com/cstate/cstate) (about 2,900 stars, MIT) is a static status page built with Hugo. It loads fast, works in very old browsers, and can be hosted free on Netlify. You write incidents as files, from the command line or through a CMS, and readers get RSS and a read-only API.

Its README is direct about the limit: "it cannot do automatic monitoring out of the box", because the site is static. That is a fair design if you only want an information page that stays up when everything else is down. The last commit was on 2 June 2026 and the last release, 6.0.1, is from July 2025, so it moves slowly.

## Gatus

[Gatus](https://github.com/TwiN/gatus) (about 12,200 stars, Apache-2.0, v5.37.0 on 24 September 2026) is a health dashboard configured in YAML, which makes it popular with teams that want their checks in Git. The dashboard doubles as the status page.

It is not a customer status page in the usual sense. There is no incident timeline and no subscriber list. For an internal page your own team watches, it is excellent and very light. For customers, it is the wrong shape. [Uptime Kuma against Gatus](/compare/uptime-kuma-vs-gatus) explains where each fits.

## Checkmate

[Checkmate](https://github.com/bluewave-labs/Checkmate) (about 10,900 stars, AGPL-3.0, v3.12.0 in September 2026) is a newer monitor with a modern interface. Its status pages come with five themes and scheduled maintenance. When a monitor changes state, it opens or resolves an incident on its own, and it sends alerts to more than a dozen services.

I found no subscriber list for page visitors in its source. It needs Node and MongoDB, and host metrics need its separate Capture agent.

## OneUptime

[OneUptime](https://github.com/OneUptime/oneuptime) (about 7,600 stars) is a whole observability platform: monitoring, on-call, incidents, logs, traces and status pages. Its status pages are among the most complete here. Incidents update the page automatically, and subscribers are notified by email and SMS, or through Slack.

The cost is size. It runs as many services on Docker or Kubernetes, and its license is Apache 2.0 with a separate enterprise directory. If a status page is all you need, it is a lot to operate. If you want to replace several paid tools at once, it is the broad option.

## Statping-ng

[Statping-ng](https://github.com/statping-ng/statping-ng) (about 2,000 stars, GPL-3.0) is the community fork of Statping: one Go binary that checks HTTP, TCP, UDP, ICMP and gRPC and shows the results on a status page. There is no subscriber list.

It is slowing down. Its last commit and its last release, v0.93.0, are both from 4 June 2025. It still works, but plan on it staying roughly as it is.

## Uptimepage

Ours, so the good side and the warning together.

Uptimepage is one Rust binary that runs uptime checks and a customer status page together. A failing check opens the incident on the page. Visitors subscribe by confirmed email or signed webhook, and you can schedule maintenance. The page shows uptime it measured, not only what someone typed. Configuration can live in Git through the Terraform provider, and there is also a REST API and an MCP server. It is AGPL, so you can run it yourself with Postgres and ClickHouse, or use the hosted service, where Pro and Team put the page on your own domain. The [open-source status page](/open-source-status-page) page has the details, and [Uptimepage against Upptime, Cachet and Statping](/vs/self-hosted-status-pages) compares it with the page-first tools.

The warning: the [repository](https://github.com/uptimepage/uptimepage) is young and small next to Uptime Kuma or Cachet. It has had fewer years to find rare bugs. If you want the most proven option and RSS is enough for your readers, Uptime Kuma is the safer choice today.

## Listed everywhere, no longer maintained

Two names still appear in most "best status page" lists, and I would not start a new page on either.

- Staytus ([adamcooke/staytus](https://github.com/adamcooke/staytus)): last commit on 10 September 2021.
- The original Statping ([statping/statping](https://github.com/statping/statping)): last release in December 2020 and last commit in October 2023. Use the Statping-ng fork if you want that design.

The community-kept [awesome-status-pages](https://github.com/ivbeg/awesome-status-pages) list is a good place to find more options, but it does not mark projects that have stopped.

## How to choose

- You want a free page with no server, and five-minute checks are fine: Upptime.
- You only publish incidents by hand and want a page that stays up when everything else is down: cState.
- A homelab or an internal page: Uptime Kuma, or Gatus if your checks should live in Git.
- Customers should subscribe, and you run the page yourself: Kener, OpenStatus, OneUptime or Uptimepage, depending on how much you want to operate and whether a failing check should open the incident for you. Cachet fits if your monitoring already lives elsewhere and you accept the v3 situation.

All of them are free to try and free to leave. If you would rather compare hosted services, [Statuspage alternatives](/blog/statuspage-alternatives) covers the paid options, and [how to write incident status updates](/blog/how-to-write-incident-status-updates) covers what to put on the page once you have one.

## Common questions

<details class="mk-faq">
<summary>What is the best open-source status page?</summary>
<div class="mk-faq__body">

It depends on whether the page should watch your services itself. If you only want to publish incidents by hand, cState and Cachet do that well. If the page should turn red on its own when a check fails, pick a tool with monitoring built in, such as Uptime Kuma, OpenStatus, Checkmate or Uptimepage.

</div>
</details>

<details class="mk-faq">
<summary>Which open-source status pages let customers subscribe?</summary>
<div class="mk-faq__body">

Cachet, OpenStatus, OneUptime, Kener and Uptimepage all keep a subscriber list and notify it by email, and several add webhooks. Uptime Kuma and cState offer RSS instead, which works for technical readers but not for most customers. Upptime, Gatus, Checkmate and Statping-ng have no subscriber list at all.

</div>
</details>

<details class="mk-faq">
<summary>Can I host a status page for free?</summary>
<div class="mk-faq__body">

Yes. Upptime runs on GitHub Actions and GitHub Pages, and cState builds a static site you can host on Netlify, so neither needs a server. The trade-off is that Upptime checks at most every five minutes and cState does no monitoring at all. Every other tool here needs a server when you run it yourself, and some, including Uptimepage, also offer a free hosted tier.

</div>
</details>

<details class="mk-faq">
<summary>Is Cachet still maintained?</summary>
<div class="mk-faq__body">

Cachet is being rebuilt as version 3, and the repository is active, but the newest tagged release is still v2.4.1 from November 2023. The v3 README says it is not yet completely ready for production use. If you pick Cachet today, you choose between an old stable release and an untagged development branch.

</div>
</details>
