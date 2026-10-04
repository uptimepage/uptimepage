+++
title = "Your cron job can fail for months and nothing will tell you"
date = "2026-08-26"
updated = "2026-10-04"
slug = "cron-jobs-fail-silently"
excerpt = "A scheduled job that stops running produces no error and no alert. GitLab's nightly backup failed for months before anyone noticed. Why, with sources."
tags = ["cron", "heartbeat", "monitoring", "backups", "reliability"]
draft = false
og_image = "/static/marketing/og-cron-jobs-fail-silently.png"
cta_label = "Put a heartbeat on a job"

list_items = [
  "The job never started",
  "The job started and died halfway",
  "The job finished and did nothing",
  "The job ran twice",
  "The job ran, but hours late",
  "The job failed and the alert never arrived",
]

[[faqs]]
q = "Why do cron jobs fail silently?"
a = "Cron jobs fail silently because cron has no idea what your job was supposed to do. It runs the command, mails any output to the crontab owner, and forgets. A job that never starts produces no output, so there is nothing to mail and nothing to notice."

[[faqs]]
q = "What is a dead man's switch for cron?"
a = "A dead man's switch turns the check around: your job calls a URL when it finishes, and the alarm fires when that call does not arrive. You do not need anything to detect the failure and report it, because whatever stopped the job also stops the call."

[[faqs]]
q = "Does a Kubernetes CronJob solve this?"
a = "A Kubernetes CronJob does not solve it, and the Kubernetes documentation says so plainly. It states that a CronJob creates a Job object approximately once per execution time of its schedule, and that after a missed deadline the CronJob skips that instance of the Job."

[[faqs]]
q = "How long can a broken backup job go unnoticed?"
a = "As long as nobody looks, which for GitLab was long enough that every backup was missing on the day they needed one. Their postmortem records that the S3 bucket was empty, and there was no recent backup to be found anywhere."

[[faqs]]
q = "What should a heartbeat check watch for?"
a = "A heartbeat check should catch a late ping, not only a missing one. Give it the period you expect and a grace window for normal jitter, and it can say the job is late before it decides the job is dead."
+++

> **TL;DR**
>
> Most failures announce themselves. A scheduled job that does not run announces nothing, because not running produces no error, no log line and no exit code. Cron will not tell you, and neither will Kubernetes: its own docs say a CronJob runs *approximately* once per schedule. The fix is to turn the check around, so the job reports in and the alarm fires on silence.

## The one failure that does not announce itself

A web server that breaks returns a 500. An expired certificate makes every browser complain loudly. Fill a disk and every write throws. Monitoring is mostly the work of catching signals that something else already produced for you.

A scheduled job that stops running produces nothing to catch. No error, because no code ran. No exit code either, because there was no process, and nothing ever reached the logger. A missing backup looks exactly like a night when the backup worked, and it keeps looking that way until the morning you need the file.

I build [Uptimepage](/), so I spend a lot of time in the gap between "my monitor is green" and "my system works". This is the widest gap I know of, and close to the cheapest to fix.

## The proof: GitLab, 31 January 2017

GitLab lost about six hours of production data: roughly 5,000 projects, 5,000 comments and 700 user accounts. The trigger was human error during a replication problem. The reason it became permanent data loss was that the backups had been failing quietly for a long time.

Their [public postmortem](https://about.gitlab.com/blog/postmortem-of-database-outage-of-january-31/) is worth reading in full. Here is what they found when they went looking for those backups:

> When we went to look for the pg_dump backups we found out they were not there. The S3 bucket was empty, and there was no recent backup to be found anywhere.

The cause was dull. The backup ran `pg_dump` 9.2 against a PostgreSQL 9.6 database, and across a major version `pg_dump` refuses and exits. It had been doing that every night for months.

They had also set the job up to complain when it failed:

> While notifications are enabled for any cronjobs that error, these notifications are sent by email. For GitLab.com we use DMARC. Unfortunately DMARC was not enabled for the cronjob emails, resulting in them being rejected by the receiver. This means we were never aware of the backups failing, until it was too late.

So the alerting worked. It fired every night for months into a mail path that dropped it on the floor, and nothing was checking that those messages still landed. The job's error reporting ran on the same infrastructure as the job, which is how it managed to fail in the same silence.

## Why cron cannot help you

The [cron manual](https://manpages.debian.org/bookworm/cron/cron.8.en.html) describes the entire reporting contract: any output is mailed to the owner of the crontab, or to the address in `MAILTO` if you set one. That is it. There is no notion of a run that should have happened, no record of the last successful run, no retry, and no way to ask whether the thing ran last night.

So cron jobs fail silently because cron has no idea what your job was supposed to do. It runs the command, mails any output to the crontab owner, and forgets. A job that never starts produces no output, so there is nothing to mail and nothing to notice.

That mail path is also a dependency with no monitoring of its own. A missing local mail transfer agent, a full disk, a typo in `MAILTO`, an SPF or DMARC policy at the far end: any one of them turns your failure alerts into nothing at all, which is exactly what happened above.

## "We use Kubernetes, so this is handled"

It is not, and the Kubernetes project is honest about it. From the [CronJob documentation](https://kubernetes.io/docs/concepts/workloads/controllers/cron-jobs/):

> A CronJob creates a Job object approximately once per execution time of its schedule.

> CronJobs have limitations and idiosyncrasies. For example, in certain circumstances, a single CronJob can create multiple concurrent Jobs.

And on missed runs:

> After missing the deadline, the CronJob skips that instance of the Job (future occurrences are still scheduled).

If the controller was down or busy for longer than your `startingDeadlineSeconds`, that run is skipped and the next one is scheduled as normal. The schedule still looks healthy afterwards. One night simply did not happen, and nothing on the CronJob object records that as a problem.

Managed schedulers make the same trade. Amazon's description of [EventBridge Scheduler](https://docs.aws.amazon.com/scheduler/latest/UserGuide/what-is-scheduler.html) offers "at-least-once event delivery", and "flexible time windows, allowing you to disperse your schedules and improve the reliability of your triggers for use cases that do not require precise scheduled invocation of targets". At-least-once means duplicates are expected behaviour. A flexible window means the time you asked for is a preference.

None of this is a criticism. Distributed schedulers give up exactness to buy reliability, and that is usually the right trade. It does mean the scheduler can only tell you it tried, never that the work got done, so something outside it has to keep score.

## What actually goes wrong

Six failure modes, roughly in the order I run into them. Only the second and the sixth produce an error at all, and in both cases nobody read it.

### The job never started

Somebody edited the crontab and a syntax error killed the line. Or the image changed and the entrypoint moved, or the node was drained and took the timer with it, or the user account the job ran as got removed during a cleanup. Whichever it was, nothing runs, so there is nothing to log and nothing to fail.

### The job started and died halfway

An out-of-memory kill, a node restart, a dropped connection in the middle of a long upload. Half a backup on disk is worse than no backup, because the file exists and its timestamp is fresh. Directory listings look completely normal.

### The job finished and did nothing

Exit code 0, empty or wrong output. A `pg_dump` pointed at the wrong database. An `rsync` from a path that was not mounted, which copies an empty directory very quickly and reports success. A cleanup script whose glob matched nothing. This one gets past almost every check people set up, because "it ran and it succeeded" is true.

### The job ran twice

Two schedulers, or a retry stacking on top of a slow run. You get duplicate invoices, double charges, two emails to every customer. Kubernetes says this outright: in certain circumstances a single CronJob can create multiple concurrent Jobs.

### The job ran, but hours late

A queue backed up, a lock was held too long, a flexible time window drifted. The nightly report lands at 11am instead of 6am, and everything downstream that assumed fresh data has been serving yesterday's numbers all morning. No alert fires, because the job did eventually succeed.

WordPress has this failure built in. WP-Cron only runs when a visit loads WordPress, so on a quiet site a post due at 06:00 waits for the first visitor, and the admin marks it ["Missed schedule"](/blog/wordpress-missed-schedule).

### The job failed and the alert never arrived

GitLab's case. The work failed and the alert was sent, and then the alert was dropped somewhere in transit. From the outside that is indistinguishable from everything working, which is what makes it the hardest one to catch.

<div class="mk-embed-blind"></div>

## Turn the check around

The fix is an old pattern with a grim name: a dead man's switch. A dead man's switch turns the check around: your job calls a URL when it finishes, and the alarm fires when that call does not arrive. You do not need anything to detect the failure and report it, because whatever stopped the job also stops the call.

```bash
0 3 * * * /usr/local/bin/backup.sh && curl -fsS https://example.com/ping/YOUR-TOKEN
```

The `&&` is doing the work: the ping only fires if the script exited zero. Four of the six failure modes above now collapse into one visible symptom, which is a ping that fails to arrive somewhere off your server.

It does not catch the other two, and it is worth being clear about that. An empty dump and a double run both exit 0, so the ping goes out and the monitor stays green. For those you have to make the script check its own work, then exit non-zero when the output is wrong, which brings them back into the four.

This is how monitoring stacks check themselves. Prometheus ships an alert called Watchdog whose whole job is to always be firing. Its [runbook](https://runbooks.prometheus-operator.dev/runbooks/general/watchdog/) explains why:

> This is an alert meant to ensure that the entire alerting pipeline is functional. This alert is always firing, therefore it should always be firing in Alertmanager and always fire against a receiver.

> If not firing then it should alert external systems that this alerting system is no longer working.

The people who build alerting systems do not rely on those systems to report their own failure. They send a steady signal somewhere else and treat a gap in it as the alarm. A backup job is worth the same care.

## What a heartbeat check needs to be useful

The one-line version above already beats nothing by a wide margin. A few things make it considerably better, and they are worth looking for in whatever tool you pick.

Start with the timing. A heartbeat check should catch a late ping, not only a missing one. Give it the period you expect and a grace window for normal jitter, and it can say the job is late before it decides the job is dead. Without that window you either [page on a few seconds of drift](/blog/stop-false-uptime-alerts) or you find out far too late. If you are not certain what times an expression really produces, a [cron expression generator](/tools/cron-expression-generator) will list them.

<div class="mk-embed-grace"></div>

Signal the start as well as the finish. Ping before the work and again after it, and a run that begins but never ends becomes visible. Without a start signal, a job hanging forever looks identical to a job that never launched, and you cannot tell whether there is a process to go and kill.

Send the exit code, and the output with it. A job that knows it failed should be able to say so straight away instead of waiting out its window. Attaching the tail of the log to that failure is usually the difference between reading the cause and going to find the machine that ran it.

Then treat the ping URL as a credential, because it is one. Anyone holding it can mark your job healthy, which means anyone holding it can hide a real outage. It also spreads by design, into crontabs, CI config and runbooks. Check that you can rotate it without deleting the monitor and losing its history, and rotate it when someone who had it leaves.

Finally, keep watching the front door. A heartbeat cannot tell you your website is down, ever. It only knows whether your job reported in. It belongs next to your [HTTP, TLS and domain checks](/blog/do-i-need-an-uptime-monitor) rather than in place of them.

## Common questions

<details class="mk-faq">
<summary>Why do cron jobs fail silently?</summary>
<div class="mk-faq__body">

Cron jobs fail silently because cron has no idea what your job was supposed to do. It runs the command, mails any output to the crontab owner, and forgets. A job that never starts produces no output, so there is nothing to mail and nothing to notice.

</div>
</details>

<details class="mk-faq">
<summary>What is a dead man's switch for cron?</summary>
<div class="mk-faq__body">

A dead man's switch turns the check around: your job calls a URL when it finishes, and the alarm fires when that call does not arrive. You do not need anything to detect the failure and report it, because whatever stopped the job also stops the call.

</div>
</details>

<details class="mk-faq">
<summary>Does a Kubernetes CronJob solve this?</summary>
<div class="mk-faq__body">

A Kubernetes CronJob does not solve it, and the Kubernetes documentation says so plainly. It states that a CronJob creates a Job object approximately once per execution time of its schedule, and that after a missed deadline the CronJob skips that instance of the Job.

</div>
</details>

<details class="mk-faq">
<summary>How long can a broken backup job go unnoticed?</summary>
<div class="mk-faq__body">

As long as nobody looks, which for GitLab was long enough that every backup was missing on the day they needed one. Their postmortem records that the S3 bucket was empty, and there was no recent backup to be found anywhere.

</div>
</details>

<details class="mk-faq">
<summary>What should a heartbeat check watch for?</summary>
<div class="mk-faq__body">

A heartbeat check should catch a late ping, not only a missing one. Give it the period you expect and a grace window for normal jitter, and it can say the job is late before it decides the job is dead.

</div>
</details>

## Sources

- GitLab, [Postmortem of database outage of January 31](https://about.gitlab.com/blog/postmortem-of-database-outage-of-january-31/), 10 February 2017.
- Kubernetes, [CronJob](https://kubernetes.io/docs/concepts/workloads/controllers/cron-jobs/), concepts documentation.
- Debian, [cron(8) manual page](https://manpages.debian.org/bookworm/cron/cron.8.en.html).
- Amazon Web Services, [What is Amazon EventBridge Scheduler?](https://docs.aws.amazon.com/scheduler/latest/UserGuide/what-is-scheduler.html)
- prometheus-operator, [Watchdog runbook](https://runbooks.prometheus-operator.dev/runbooks/general/watchdog/).

## Give your jobs a way to speak up

I build [Uptimepage](/) because I wanted these checks for my own services. Heartbeat checks are one of the eight kinds it runs, alongside HTTP, TCP, ping, DNS, TLS expiry, domain expiry and browser flows, from several regions, with a status page for your customers. The heartbeat side does the things listed above: a period with a grace window, start and failure signals, the exit code and the job's output, and a ping URL you can rotate without losing the monitor's history. There is more in the [monitor types documentation](/docs/monitor-types).

It is open source under AGPL, so you can run it on your own server, or start on the hosted free tier and skip that part.

Whatever you use, the question is the one GitLab could not answer on 31 January 2017. What told you the job ran last night? If the honest answer is that you would have assumed it ran unless something said otherwise, you are trusting an assumption that has already cost other people their backups.
