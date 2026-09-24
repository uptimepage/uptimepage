+++
title = "Best open-source, self-hosted uptime monitors (2026)"
date = "2026-06-20"
updated = "2026-09-24"
slug = "best-self-hosted-uptime-monitoring-tools"
excerpt = "A fair look at the open-source, self-hostable tools for watching sites and APIs in 2026: what each is good at, where it stops, and how to pick one."
tags = ["open-source", "self-hosted", "monitoring", "status-page"]
draft = false
cta_label = "Try the hosted tier free"
list_items = [
    "Uptime Kuma",
    "Gatus",
    "Checkmate",
    "OneUptime",
    "Apache HertzBeat",
    "Zabbix",
    "Cachet",
    "Statping-ng",
    "Blackbox exporter",
    "Nagios Core",
    "OpenStatus",
    "Uptimepage",
]

[[faqs]]
q = "What is the best Uptime Kuma alternative?"
a = "Depends on which edge you hit. If it is the single shared login and the missing REST API, Uptimepage and OpenStatus both give you teams and monitoring as code; the OpenStatus and Kuma comparison covers that choice. If you want config in Git, Gatus. If you want a fresher UI doing the same job, Checkmate. If you want host metrics and middleware in the same tool, HertzBeat."

[[faqs]]
q = "Which open-source uptime monitor is best for developers?"
a = "If you want everything in Git and reviewed in a pull request, Gatus and Uptimepage both fit the developer workflow, Gatus with a YAML file and Uptimepage with a Terraform provider and a REST API. Uptime Kuma is friendlier to click through, but its API is the internal Socket.io interface rather than a supported REST API for managing monitors, so it is the weaker pick when you want monitoring as code."

[[faqs]]
q = "What is external uptime monitoring?"
a = "It means checking your service from outside your own network, the way a real user reaches it, rather than from a process on the same box. External checks catch DNS, TLS and edge failures that an internal healthcheck never sees. Every tool on this list does external monitoring; the ones with multi-region probes, like Uptimepage, let you check from more than one place at once."

[[faqs]]
q = "Is Zabbix or Nagios a good uptime monitor?"
a = "They are infrastructure monitors that can also check uptime. Both probe URLs from the server, Zabbix with web scenarios and Nagios with plugins like check_http, but neither gives your customers a status page, and both take more to run than a dedicated uptime tool. Plenty of teams run one inside and a separate checker outside."

[[faqs]]
q = "Can I white-label the status page?"
a = "Some can. If you run an agency or resell monitoring and need your own logo, colours and domain in front of clients, look for a tool built for it. That is the slice we cover on white-label uptime monitoring, where each client gets a branded page with no vendor name shown."
+++

If you searched for a self-hosted uptime monitor, or for an alternative to the tool you are outgrowing, you already made the important decision. You want the data on your own servers, no per-monitor invoice, and no vendor that can change the free plan whenever they want. The rest is picking the tool that fits how your team works.

We build one of the tools on this list, Uptimepage, so keep that in mind. We have tried to keep the descriptions of everyone else to facts that stay true rather than feature claims that go out of date in a month, because this whole category moves fast. Check the current docs before you commit. With that said, here is an honest guide for 2026.

> **Key takeaways**
>
> - Uptime Kuma is the homelab default: one container, 31 monitor types and 94 notification integrations, but a single shared login, no official REST API for managing monitors, and basic status pages.
> - Gatus is the pick when monitoring should live in Git as YAML for your own team.
> - Checkmate is the freshest Kuma-style option, with a modern UI and themed status pages, if you can run a Node and MongoDB stack.
> - OneUptime and Apache HertzBeat are full platforms for the whole incident lifecycle, or for databases and network gear, when you can carry the weight.
> - Zabbix and Nagios Core watch your servers from the inside; they can probe a URL, but neither gives customers a status page.
> - Cachet and Statping are status-page-first; teams already on Prometheus can add the Blackbox exporter.
> - OpenStatus is the nearest AGPL alternative that does both jobs with monitoring as code, though self-hosting it runs several services rather than one.
> - Uptimepage (ours) pairs monitoring with a customer status page in one AGPL binary, with a REST API, Terraform, roles and subscribers, for when you outgrow a single shared login.

## What actually matters when you self-host

Three questions sort this category faster than any feature table.

First, do you want a status page your customers see, or just internal alerts? Some tools do one well and bolt on the other. Second, do you configure by clicking, or do you want the config in Git? That single preference rules out half the list for most people. Third, how much do you want to operate? A single binary on a small VPS is a different commitment from a platform that expects Kubernetes. And whichever you pick, check that its status page shows uptime it measured, not just the incidents you published, which is [a status page you cannot fake](/blog/status-page-you-cant-fake).

Hold those three in mind and the choices get obvious. Here is the whole list against them at a glance, before the detail on each.

| Tool | Customer status page | Monitoring as code | What you operate |
| --- | --- | --- | --- |
| Uptimepage | Yes, with subscribers | Yes, Terraform + REST + MCP | One binary + Postgres + ClickHouse |
| Uptime Kuma | Basic | No REST API | One container |
| Gatus | Basic, developer-focused | Yes, YAML | Tiny binary |
| Checkmate | Yes, themed | UI-driven | Node + MongoDB |
| OneUptime | Yes | Yes, API | Docker or Kubernetes |
| OpenStatus | Yes | Yes, Terraform + REST + MCP | ~6 Docker services |
| Apache HertzBeat | Yes | Yes, YAML templates | Java + database + time-series |
| Cachet | Yes, with subscribers | API, feed it | PHP app + DB + queue + cron |
| Zabbix | No, operator dashboards | JSON-RPC API, no official Terraform | Server + database + PHP frontend |
| Statping-ng | Yes | No | One Go binary |
| Blackbox exporter | No, build it yourself | Yes, config | Prometheus + Alertmanager |
| Nagios Core | No, operator console | Yes, text config files | Server + plugins |

## Uptime Kuma

The default, and for good reason. Uptime Kuma is the tool most people mean when they say "self-hosted uptime monitor." One Docker container, a clean dashboard, a long list of monitor types, notifications to almost anything. If you run a homelab or a handful of side projects, you can stop reading here and go install it.

Its limit shows up with teams. As of 2026 it still uses a single shared login, so everyone who can see the dashboard can change anything. There is [no official REST API for managing monitors](/blog/uptime-kuma-rest-api), the config lives in the database rather than a file you can commit, and the status pages are basic compared to a customer-facing tool. None of that matters for a Raspberry Pi watching your blog. All of it starts to matter once a second person needs access.

If you are hitting those limits, that is the moment the rest of this list becomes interesting. We wrote a longer [comparison with Uptime Kuma](/vs/uptime-kuma) if you want the specifics.

## Gatus

Gatus is the tool for people who want their monitoring in YAML and nowhere else. You describe each endpoint in a config file, commit it, and Gatus watches it. It is tiny, it speaks a lot of protocols, and it fits a GitOps workflow perfectly.

The weakness is also its strength. There is no web UI for editing checks, so a non-engineer cannot touch it, and the status page is functional rather than something you would put in front of customers. If your monitoring should live in a pull request and your audience is your own team, Gatus is a great answer.

## Checkmate

Checkmate is the newest strong option among the tools like Kuma, and it is moving fast. It covers website and HTTP checks, ping, TCP ports, SSL certificates, Docker containers and even game servers, and it ships status pages with a modern UI. Alerts go to the usual places: email, webhooks, Slack, Discord, PagerDuty, Telegram, Teams and SMS via Twilio. If you want hardware metrics too, a separate lightweight agent called Capture adds CPU, memory, disk and temperature per host. It is AGPL, like Uptimepage.

The operational cost is the stack: a Node.js backend plus MongoDB, run through Docker Compose or Helm, rather than a single binary. And it is young, so the same caveat we give about ourselves applies here: fewer years to find and fix rare bugs than Kuma. If you like Kuma's scope but want a fresher UI and status pages with themes, it is worth a look.

## OneUptime

OneUptime is the everything platform: monitoring, status pages, on-call, incident management, and logs in one open-source project. If you want to replace several paid tools at once and you have the operational capacity to run it, it does a lot.

But doing so much is also the problem. It is heavier than a single-binary tool and expects you to be comfortable with Docker or Kubernetes and a real chunk of resources. On a small VPS it is the wrong tool. For a platform team that already runs Kubernetes and wants one open-source system to own the whole incident lifecycle, it is worth the extra work. [OneUptime against Uptime Kuma](/compare/uptime-kuma-vs-oneuptime) works through that trade in detail.

## Apache HertzBeat

HertzBeat is what happens when uptime monitoring grows into a full monitoring platform as an Apache project. It is agentless: everything is described in YAML templates that speak HTTP, SSH, SNMP, JMX, JDBC and Prometheus, so the same tool that pings your website can also watch MySQL, Redis, Kafka, Kubernetes and the switch in your rack. It has a status page builder and alerts to email, Slack, Discord, Telegram, SMS and more.

The cost is complexity. A production deployment is a Java application plus a relational database plus a time-series store, which is a different commitment from one Go binary. Pick it when the wide coverage is the point, when you want one system watching databases and middleware and network gear, not just URLs. For plain uptime checks and a customer status page, it is more platform than the job needs.

## Zabbix

Zabbix turns up on most lists of self-hosted monitors, and it earns the spot, but it answers a different question. It is agent-based: an agent on each host reports CPU, memory, disk, processes, logs and database internals to a central server that stores everything and evaluates triggers. So it can tell you why a server is unhealthy, which no outside checker can. It has been AGPL-3.0 since 7.0.

It can check a website too. Web scenarios run a sequence of HTTP steps and assert on status codes, body strings and response time, and simple checks like icmpping and net.tcp.service need no agent. You configure them the Zabbix way, though, as hosts, templates, items and triggers rather than by pasting in a URL. What you operate is a server, a database and a PHP frontend, usually plus a proxy for anything across a network boundary. It has operational dashboards rather than a page your customers can read, and a full JSON-RPC API but no official Terraform provider. Pick it when you want the inside of your servers watched. For outside-in checks alone, it is a lot of platform to run. [Uptime Kuma against Zabbix](/compare/uptime-kuma-vs-zabbix) goes through that split in detail.

## Cachet and Statping-ng

These two cover the status-page corner. Cachet is a long-running, PHP-based status page going through a rebuild, and the rebuild is where the caveats live: v3 is still in development with no stable release, the newest tagged release remains v2.4.1 from 2023, and the v3 branch ships under a custom source-available license rather than the BSD one 2.x carried. What it does well is the status-page job itself, now including confirmed email subscribers. It is a page first: v3 added a basic HTTP check, but you schedule it yourself and a failure colours a component rather than opening an incident, so in practice you still feed it from elsewhere. Statping-ng is a community-kept fork of the older Statping, a single Go binary that does both monitoring and a status page, with a smaller community behind it.

Pick these if a status page is the actual product you need and the monitoring is secondary or already handled. [Cachet against Uptime Kuma](/compare/uptime-kuma-vs-cachet) sets out what you give up on the checking side, and [the open-source status pages roundup](/blog/best-open-source-status-pages) compares every page-first option.

## A note on Beszel

Beszel comes up in every "Kuma alternative" thread, so let us save you time: it is not an uptime monitor. It is a very good, very light server dashboard. An agent on each host reports CPU, memory, disk, network, temperatures and per-container Docker stats back to a single hub, with alerts to about twenty services. What it does not do is probe an endpoint: no HTTP, TCP, DNS or TLS checks, and no public status page. People run it next to an uptime monitor, not instead of one. If you want host metrics with your uptime checks in a single tool, that is what HertzBeat or OneUptime are for.

## Prometheus, Nagios and the infrastructure stack

Not a product, a pattern, and worth naming because plenty of teams already work this way. If you run Prometheus, the Blackbox exporter probes HTTP, TCP, DNS, and ICMP, and Alertmanager handles the alerting. You get enormous power and you build the experience yourself, including the status page and the on-call flow. For a team that is already deep in Prometheus, adding uptime checks is a small step. For anyone else it is a lot of work to put together. [The Blackbox exporter against Uptime Kuma](/compare/blackbox-exporter-vs-uptime-kuma) counts up what the assembled version actually costs.

Nagios Core is the oldest member of this family and still maintained. It runs every check as a small plugin that returns OK, WARNING, CRITICAL or UNKNOWN, so check_http or check_ping on the Nagios server gives you outside-in probing, and NRPE reaches inside a host. Everything is declared in text configuration files, which suits Git, and it is GPL-2.0. What it lacks is a status page for customers: its web interface is an operator console. Icinga started as a Nagios fork and Checkmk grew up on the Nagios core, so the same trade applies to both. They are very good at watching infrastructure you own, and publishing uptime to anyone outside your team is a separate job.

## OpenStatus

OpenStatus is the closest tool here to what we build: an AGPL project that puts status pages and uptime monitoring in one place, with a real REST API, an MCP server, and a Terraform provider that now covers monitors, status pages and notifications, not just checks. Its hosted service probes from twenty-eight regions across three clouds, and the same monitoring-as-code workflow runs whether you use the managed tier or self-host.

The catch is the shape of the self-hosted stack. Running OpenStatus yourself means operating several separate services, the API, dashboard, checker, workflow processor and status-page renderer, rather than one process. For a team that wants the managed tier and self-hosts occasionally, that is fine. If owning the whole thing on your own box is the point, it is more moving parts than a single-binary tool. We compared [OpenStatus and Uptime Kuma](/compare/openstatus-vs-uptime-kuma) if you are weighing those two.

## Uptimepage

Ours, so here is the good side and the warning together.

Uptimepage is a single Rust binary that does uptime monitoring and a customer-facing status page together, with the parts Uptime Kuma users tend to ask for: a real REST API, a Terraform provider, organizations with roles, multi-region probe agents you run yourself, and status pages your customers can subscribe to over email or webhook. It is AGPL, so `docker compose up` and it is yours, or you can use the [hosted uptime monitor](https://uptimepage.dev) and skip running it. The same data model, API, and Terraform provider work either way, so you are not stuck with that choice. There is more on running it as an [open-source uptime monitor](/open-source-uptime-monitoring), on the [self-hosted setup](/docs/deployment), and on [driving it from code](/terraform-uptime-monitoring).

The caveat: it is younger than Uptime Kuma and has a smaller community, so it has had fewer years to find and fix rare bugs. If you want the most tested and proven option and you do not need an API or multi-tenant access, Uptime Kuma is the safer pick today. If you have outgrown a single shared login and want your monitoring in Git, that gap is exactly what we built for.

## How to choose without overthinking it

For a homelab or a few personal projects, install Uptime Kuma and move on, or try Checkmate if you want the fresher UI. For a team that wants config in Git and only needs internal alerts, use Gatus; we wrote a closer look at [how Kuma and Gatus differ](/compare/uptime-kuma-vs-gatus) if you are torn between those two. If a polished customer status page with subscribers and an API is the point, look at [an open-source status page](/open-source-status-page) with monitoring built in, which is the slice we focus on. If you want to own the entire incident lifecycle and you already run Kubernetes, OneUptime is the broad option. And if you already live in Prometheus, the Blackbox exporter is a small addition. If you need to know why a server is sick, not only whether it answers, that is Zabbix or Nagios territory, and many teams run one of them next to an outside checker.

There is no single best tool here, only the one that matches your three answers. The good news is that all of them are free to try and free to leave, which is the whole point of staying open-source.

## Common questions

<details class="mk-faq">
<summary>What is the best Uptime Kuma alternative?</summary>
<div class="mk-faq__body">

Depends on which edge you hit. If it is the single shared login and the missing REST API, Uptimepage and OpenStatus both give you teams and monitoring as code; [OpenStatus and Kuma compared](/compare/openstatus-vs-uptime-kuma) covers that choice. If you want config in Git, Gatus. If you want a fresher UI doing the same job, Checkmate. If you want host metrics and middleware in the same tool, HertzBeat.

</div>
</details>

<details class="mk-faq">
<summary>Which open-source uptime monitor is best for developers?</summary>
<div class="mk-faq__body">

If you want everything in Git and reviewed in a pull request, Gatus and Uptimepage both fit the developer workflow, Gatus with a YAML file and Uptimepage with a Terraform provider and a REST API. Uptime Kuma is friendlier to click through, but its API is the internal Socket.io interface rather than a supported REST API for managing monitors, so it is the weaker pick when you want monitoring as code.

</div>
</details>

<details class="mk-faq">
<summary>What is external uptime monitoring?</summary>
<div class="mk-faq__body">

It means checking your service from outside your own network, the way a real user reaches it, rather than from a process on the same box. External checks catch DNS, TLS and edge failures that an internal healthcheck never sees. Every tool on this list does external monitoring; the ones with multi-region probes, like Uptimepage, let you check from more than one place at once.

</div>
</details>

<details class="mk-faq">
<summary>Is Zabbix or Nagios a good uptime monitor?</summary>
<div class="mk-faq__body">

They are infrastructure monitors that can also check uptime. Both probe URLs from the server, Zabbix with web scenarios and Nagios with plugins like check_http, but neither gives your customers a status page, and both take more to run than a dedicated uptime tool. Plenty of teams run one inside and a separate checker outside.

</div>
</details>

<details class="mk-faq">
<summary>Can I white-label the status page?</summary>
<div class="mk-faq__body">

Some can. If you run an agency or resell monitoring and need your own logo, colours and domain in front of clients, look for a tool built for it. That is the slice we cover on [white-label uptime monitoring](/white-label-uptime-monitoring), where each client gets a branded page with no vendor name shown.

</div>
</details>
