+++
title = "WordPress missed schedule: why it happens and how to fix it"
date = "2026-10-04"
slug = "wordpress-missed-schedule"
excerpt = "A post marked \"Missed schedule\" means WP-Cron did not run on time. Here are the four usual causes, and how to hear about it the next time it stops."
tags = ["wordpress", "wp-cron", "cron", "heartbeat", "monitoring"]
draft = false
cta_label = "Watch WP-Cron with a heartbeat"

list_items = [
  "Nobody visited at the right time",
  "The page cache answered every visit",
  "The site cannot reach itself",
  "WP-Cron was switched off and nothing replaced it",
]

[[faqs]]
q = "Why does WordPress say missed schedule?"
a = "WordPress says missed schedule when a scheduled post's time has passed and WP-Cron has not yet run the event that publishes it. WP-Cron only runs when a request loads WordPress, so a quiet hour or a failed loopback request leaves the post waiting."

[[faqs]]
q = "How do I fix missed schedule in WordPress?"
a = "Run WP-Cron from a real system cron instead of from visits. Set DISABLE_WP_CRON to true in wp-config.php and add a cron job that runs due events every five minutes, with WP-CLI or by calling wp-cron.php."

[[faqs]]
q = "Can a caching plugin cause missed schedule?"
a = "Yes, a full-page cache can cause it. When the cache answers a visit without loading WordPress, that visit never starts WP-Cron, so even a busy site can miss a scheduled post."

[[faqs]]
q = "How do I know when WP-Cron stops again?"
a = "Ping a heartbeat monitor after each cron run. If the pings stop, the monitor alerts you, so a lost crontab line shows up within minutes instead of at the next missed post."
+++

> **TL;DR**
>
> "Missed schedule" means WP-Cron did not run when the post was due. WP-Cron has no clock of its own: it runs only when a request loads WordPress, and then only if the site can send a request to itself. A quiet site, a page cache, a blocked loopback or a switched-off WP-Cron all stop it. The fix is a real system cron, and a heartbeat that tells you when that cron stops.

## What the label means

When you schedule a post, WordPress does not start a timer. It adds a one-off event to a list, due at the publish time. WP-Cron is the part that runs that list.

The name suggests the Unix cron, but it works differently, and the [WordPress developer handbook](https://developer.wordpress.org/plugins/cron/) says so:

> WP-Cron does not run constantly as the system cron does; it is only triggered on page load.

So on each request that loads WordPress, it looks at the list. If an event is due, it sends a request to `wp-cron.php` on your own site, and that request runs the event. If no request loads WordPress, or the request to itself fails, nothing runs. The post stays scheduled after its time has passed, and the posts list marks it "Missed schedule".

The same list runs more than posts. Backup plugins, WooCommerce's background jobs, update checks and plugin emails all wait for it. So when a post misses, the backups may have stopped too, and the post is only the part you can see.

## The four usual causes

### Nobody visited at the right time

The post is due at 06:00 and the first visitor arrives at 09:30. WP-Cron starts with that visit, so the post goes out three and a half hours late. This is common on small sites, and on sites whose readers sleep in another time zone.

### The page cache answered every visit

A full-page cache keeps a copy of each page and serves it without loading WordPress, which is why it is fast. But a visit that the cache answers never reaches WordPress, so it never starts WP-Cron. A busy site can miss a post because its cache is working well, and only the visits that miss the cache run the list.

### The site cannot reach itself

WP-Cron runs events through a request from the server to its own address, called a loopback request. A firewall, a wrong DNS entry on the server, a password on a staging site or a security rule can block it. Every visit then tries to start WP-Cron, the request fails, and nobody sees an error. The Site Health screen under Tools reports this as "Your site could not complete a loopback request".

### WP-Cron was switched off and nothing replaced it

Speed guides and some hosts set `DISABLE_WP_CRON` to `true` in `wp-config.php`. That stops visits from starting WP-Cron, which is correct only if a system cron starts it instead. If that cron was never added, or it stayed on the old server after a move, nothing runs at all.

## How to find yours

Start with the Site Health screen. It shows a failed loopback request, and it also warns when a scheduled event is late.

If you have a shell, WP-CLI answers the rest. `wp cron test` "tests the WP Cron spawning system and reports back its status", according to the [WP-CLI docs](https://developer.wordpress.org/cli/commands/cron/test/). `wp cron event list` shows every event with its next run time, so overdue events are easy to see.

Without a shell, the [WP Crontrol](https://wordpress.org/plugins/wp-crontrol/) plugin shows the same list in the admin and can run an event by hand.

Then open `wp-config.php` and look for `DISABLE_WP_CRON`. If it is there and set to `true`, find out what is supposed to run WP-Cron instead.

## Fix it with a real cron

Switch WP-Cron off for visits and run it from the system cron on a fixed interval, as the [handbook](https://developer.wordpress.org/plugins/cron/hooking-wp-cron-into-the-system-task-scheduler/) describes:

```bash
# First add to wp-config.php: define( 'DISABLE_WP_CRON', true );
# Run as the site's user, not root: WP-CLI refuses to run as root.
*/5 * * * * cd /var/www/example.com && /usr/local/bin/wp cron event run --due-now --quiet
```

On shared hosting without WP-CLI, put a call to `wp-cron.php` in the host's cron box instead, at the shortest interval the host allows:

```bash
curl -fsS -o /dev/null "https://example.com/wp-cron.php?doing_wp_cron"
```

After that, quiet hours and the cache stop mattering. A post due at 06:00 goes out by 06:05.

## Then make sure the clock keeps running

Now everything depends on that one cron line, and it can stop just as quietly. A crontab gets edited, or the site moves to a new server and the line stays behind. No error appears anywhere, because a job that does not run cannot report a failure. I wrote about this kind of failure in [your cron job can fail for months and nothing will tell you](/blog/cron-jobs-fail-silently).

The answer is to turn the check around. After each run, the job calls a heartbeat URL. If the calls stop, the heartbeat monitor alerts you:

```bash
URL=https://app.uptimepage.dev/ping/your-token
*/5 * * * * cd /var/www/example.com && /usr/local/bin/wp cron event run --due-now --quiet; curl -fsS -o /dev/null "$URL/$?"
```

The `$?` passes WP-CLI's exit code, so a failed run alerts at once instead of waiting for the next ping to be late. Set the heartbeat period to the cron interval, five minutes here, with a few minutes of grace.

This is how I watch WP-Cron in [Uptimepage](/wordpress-site-monitoring), next to the HTTP, TLS and domain checks for the same site. Any heartbeat service works the same way. You want to hear that the cron stopped before the next scheduled post misses.

## Common questions

<details class="mk-faq">
<summary>Why does WordPress say missed schedule?</summary>
<div class="mk-faq__body">

WordPress says missed schedule when a scheduled post's time has passed and WP-Cron has not yet run the event that publishes it. WP-Cron only runs when a request loads WordPress, so a quiet hour or a failed loopback request leaves the post waiting.

</div>
</details>

<details class="mk-faq">
<summary>How do I fix missed schedule in WordPress?</summary>
<div class="mk-faq__body">

Run WP-Cron from a real system cron instead of from visits. Set DISABLE_WP_CRON to true in wp-config.php and add a cron job that runs due events every five minutes, with WP-CLI or by calling wp-cron.php.

</div>
</details>

<details class="mk-faq">
<summary>Can a caching plugin cause missed schedule?</summary>
<div class="mk-faq__body">

Yes, a full-page cache can cause it. When the cache answers a visit without loading WordPress, that visit never starts WP-Cron, so even a busy site can miss a scheduled post.

</div>
</details>

<details class="mk-faq">
<summary>How do I know when WP-Cron stops again?</summary>
<div class="mk-faq__body">

Ping a heartbeat monitor after each cron run. If the pings stop, the monitor alerts you, so a lost crontab line shows up within minutes instead of at the next missed post.

</div>
</details>
