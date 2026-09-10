# Data retention

This page covers the hosted service at `uptimepage.dev`. On your own instance every window below is yours to set, in the `plans` row and the ClickHouse table TTLs.

The [Privacy Policy](https://uptimepage.dev/privacy) is the binding statement. This page explains the mechanics behind it.

## Check data

There are three layers, and a chart reads whichever one covers the range you asked for.

| Layer | What it holds | Kept |
|---|---|---|
| Raw results | One row per check: status, latency phases, response code, error text | 30 days |
| Minute rollup | Per-minute aggregates used by the dashboard and monitor charts | 30 days |
| Hourly rollup | Per-hour aggregates behind long-range uptime and history | 13 months |

The raw window is stamped on each row when it is written, from your plan. That means a plan change applies to data written after it, and nothing already stored is retroactively shortened or extended.

Your plan's history window is what the operator UI and API will show you, and it sits on top of these layers. Standard shows 30 days, Founding and Pro show 90 days, and Team goes to 13 months; the [pricing page](https://uptimepage.dev/pricing) carries the current figure per plan. The hourly rollup is what makes the longer windows cheap: a year of history is hours, not hundreds of millions of raw rows.

The public status page is separate: its per-component history strip always covers 90 days on every plan, painted from confirmed incident windows and the hourly rollup, not from your plan's history window.

Uptime over a window that has no data at all is reported as unknown rather than as 100 percent. A gap is not a success.

## Everything else

| Data | Kept |
|---|---|
| Account, monitors, status pages, channels | Until you delete them |
| Sessions | 90 days maximum |
| API tokens | Until you revoke them |
| Login attempts | 180 days |
| Sign-in method changes | 180 days |
| Quota and rate-limit events | 90 days |
| Audit log | 2 years |
| Server access and error logs | 30 days |
| Heartbeat pings (when each signal arrived, exit status, run duration) | 30 days |
| Output your job posts with a heartbeat ping | 7 days |
| Browser flow runs (which steps ran, how long each took) | 30 days |
| Browser flow failure evidence (page URL, title, visible text, console) | 7 days |

Incidents and their timelines live with your organization and are kept until you delete the organization, so a retrospective stays readable long after the raw checks behind it have expired.

## Deleting things

Deleting a monitor removes it from the UI and the API immediately. Its check history ages out on the windows above rather than being erased on the spot.

Deleting your account suspends it immediately, stops your monitoring, and purges it permanently after 30 days. Within those 30 days you can restore it: sign back in and confirm on the page that appears. Signing in on its own does not cancel the deletion. See [Account settings](../ui.md) for where both actions live.

One blocker to know about: you cannot delete your account while an organization that bills to it, or that you are the sole owner of, still has other members (the API answers `422 OWNS_SHARED_ORGS`). Remove the members or delete the organization first; an org where you are the only member is deleted along with the account.

## Getting your data out

**Settings → Account → Export My Data** returns a JSON file covering your account: profile, sessions and tokens metadata, and the organizations you own with their monitors and settings. Organizations where you are a member but not the owner are not included; ask that org's owner for its data. Per-monitor and per-incident JSON exports are available from the API, public incidents have an RSS feed, and status pages expose an SVG badge. Nothing here is a paid add-on, and there is no export you have to ask us for.
