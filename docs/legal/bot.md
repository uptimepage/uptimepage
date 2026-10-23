# Uptimepage Bot

**Last updated:** 2026-10-23

If you found this page in your server logs, our bot requested a page on your site. This page explains what it is, why it happened, and how to stop it.

## What It Is

Uptimepage is an uptime monitoring service. Someone using our service configured a check against a URL on your site. Every few seconds or minutes, we request that URL and record whether it responded, how fast, and with what status code. That is the whole job.

Every request we make was configured by a person. We do not discover sites on our own, and we do not crawl.

## How to Identify It

Our probes send a `User-Agent` that names us and links back to this page:

```
Mozilla/5.0 (compatible; uptimepage/1.5.0; +https://uptimepage.dev/bot)
```

The version number changes as we release. The `uptimepage/` token and the link do not.

The `Mozilla/5.0 (compatible; …)` prefix is the long-standing convention for well-behaved automated clients, and it is what many CDNs read before deciding whether to compress a response. It claims no browser engine: match on `uptimepage/` and you will always find us.

### If you allowlisted us before 2026-08-17, check your rule

Until that date this page said our `User-Agent` **starts with** `uptimepage/`, and it did. It now begins with the `Mozilla/5.0 (compatible; …)` prefix, so the `uptimepage/` token sits in the middle of the string rather than at the front.

A rule written as "starts with" or `^uptimepage/` no longer matches, which means an exemption you set up for us has quietly stopped applying. Change it to a **contains** match on `uptimepage/`. Everything below already assumes that form.

We are sorry for the churn. If your monitor started failing around that date for no reason you could find on your side, this is the first thing to check.

## What It Does and Does Not Do

We request the exact URL that was configured. Nothing else.

- We follow redirects, up to 10 hops.
- If a redirect sets a cookie, we send it back on the later hops of that same check, as a browser would. We drop it when the check ends, so the next check starts with no cookies.
- We request one URL per check. We do not follow links on your pages.
- We do not crawl or spider your site.
- We do not scan ports.
- We do not probe for admin panels, config files, or vulnerabilities.

How often a check runs is set by the customer within the limits of their plan. Most checks run about once a minute.

## robots.txt

We do not read your `robots.txt`, and we want to be straightforward about why.

`robots.txt` is a protocol for crawlers, which discover and index pages on their own. We are not a crawler. We fetch a single URL that a person explicitly asked us to watch, and monitoring is usually the thing that person most wants to keep working. Blocking it in `robots.txt` would silently break their monitoring rather than protect you.

If you do not want us requesting a URL on your site, the ways to stop it are below and they work regardless of `robots.txt`.

## Where the Requests Come From

We probe from multiple regions around the world. The current region list is on our [probe regions page](/docs/hosted/regions).

Our outbound IP addresses are not guaranteed to be static and can change without notice. **Do not build an IP allowlist from observed addresses**; checks from other regions would still be blocked. If you need a static IP allowlist, email us and we will work one out with you.

## How to Allowlist Us

Allowlist by `User-Agent`. Match requests whose `User-Agent` contains `uptimepage/`, and let them through your WAF, rate limiter, or bot protection.

This is worth doing. If your WAF challenges or blocks our probes, the person monitoring your site sees failures that look like an outage on your side when nothing is actually wrong.

### Vercel

If your Vercel project has bot protection, Attack Challenge Mode, or a matching firewall rule enabled, its firewall treats our probe as an automated client. A blocked probe comes back as `403` with an `x-vercel-mitigated: deny` header, or as a Security Checkpoint page with `x-vercel-mitigated: challenge`. Either way the monitor reads as down while your site is fine in a browser. Vercel's own guidance for this case is to use a **Bypass** rule: a challenge cannot be solved by automated clients at all, because it requires a browser to execute JavaScript.

From your project in the Vercel dashboard:

1. Open **Firewall** in the sidebar
2. Select **⋯** → **Configure**, then **Add New...** → **Rule**
3. Name it something like `Allow uptimepage monitoring`
4. Under **If**, choose the `User-Agent` request header with the **contains** operator and the value `uptimepage/`
5. Under **Then**, choose **Bypass**
6. Select **Save Rule**, then **Review Changes** → **Publish**

The rule does not take effect until you publish it.

Two things to know:

- It is a **contains** match, not a prefix match. Our `User-Agent` begins with `Mozilla/5.0 (compatible; …)`, so `uptimepage/` sits in the middle.
- A custom Bypass rule skips your custom and managed rules, but **not** Vercel's platform-level DDoS mitigations. If the block is coming from those, a custom rule will not clear it and you need Vercel support.

If a **Deny** rule with a persistent action already matched one of our probes, Vercel stores that IP at the platform firewall for the timeout you configured. Adding the bypass rule stops new blocks, but an existing one has to expire.

If you would rather not open a path by header, point the monitor at a health endpoint outside the firewall's scope, or exempt just that path.

## Not Every uptimepage Request Is Ours

uptimepage is open source, so anyone can run their own copy, and its probes send the same `uptimepage/` token. A copy we do not run links its operator's page, or the project's [Probe identity](https://github.com/uptimepage/uptimepage/blob/main/docs/bot.md) page when its operator has not set one. A `User-Agent` is only a header, and any client can copy ours.

If you are not sure a request came from us, email the URL or domain to **hello@uptimepage.dev**. We will check whether it is monitored through our service and, as below, block it if you ask. We cannot stop a copy we do not run; for that, contact whoever controls the IP address the request came from.

## If You Did Not Authorise This

Anyone can type a URL into our service, including a URL on a site they do not own. If you did not authorise monitoring of your site and you want it to stop, tell us and we will block it.

Email **hello@uptimepage.dev** with the subject `[ABUSE]` and include:

1. The URL or domain being requested
2. That you are the owner or operator of it

We will block the target so it cannot be monitored through our service. You do not need to prove ownership for us to act on a straightforward request to stop probing a domain.

Unauthorised monitoring is a violation of our [Abuse Policy](/abuse-policy).

## Reporting a Problem

If our bot is behaving badly, requesting too often, ignoring what is written on this page, or causing you load, email **hello@uptimepage.dev** and we will look into it.

## Contact

hello@uptimepage.dev
