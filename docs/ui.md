# Web UI

The app is where you add monitors, watch them, and respond when one goes down. On the hosted service it is <https://app.uptimepage.dev>; on your own instance it is whatever host and port you bound the API to.

Every screen here is a faster way to reach the [REST API](api.md), not a separate feature set. Monitors and notification channels can also be declared as code with [Terraform](terraform.md).

## Dashboard

The landing screen after sign-in: summary cards across the top, then one dense table listing every monitor.

The cards give you overall uptime, response time, check volume, and the current up/down split, next to a health rail showing the last 24 hours. The table below carries a row per monitor with its status, type, p50 and p95 response times, a latency sparkline, error rate, and uptime, and you can sort and filter it. It is built to stay readable at a thousand monitors, so the answer to "which one is slow" is a sort rather than a scroll.

## Monitors

**Monitors** lists everything you are checking, with filters by name, tag, and enabled state. Monitors created by an API token or by Terraform carry a small `api` or `terraform` chip, so it is obvious which ones a UI edit might get overwritten on the next apply.

Opening a monitor gives you its detail view: current status, uptime figures, response-time percentiles, a breakdown of where the time goes (DNS, connect, TLS, first byte), recent check results, and the incident history. Four time ranges (1 hour, 24 hours, 7 days, 30 days) switch the whole page at once, and if the monitor runs in several regions you can filter any chart down to one of them.

**Add monitor** takes a URL, an interval, and the regions to check from. HTTP is the default; [Monitor types](monitor-types.md) covers the other seven. Two sections are worth a look before you save:

- **Detection** decides when a failure counts. Set how many consecutive failures open an incident, and how many regions have to agree. See [Probe regions](hosted/regions.md) for what the region policies mean.
- **Notifications** binds the alert channels for this monitor, sets how often to remind you while it stays down, and whether recovery is announced. It only appears once your org has at least one channel.

Credentials are not typed into the monitor form. Headers reference org secrets instead, picked with the variable helper, so the literal value lives in one place. See [Variables and secrets](variables.md).

## Incidents

**Incidents** is the responder view: what is open, who acknowledged it, and what has happened since. You can declare an incident by hand for something your monitors cannot see, post updates as you learn more, and close it with a retrospective.

What responders see here is deliberately separate from what customers see on a status page. [Incident management](incidents.md) covers the model and the workflow.

## Settings

| Screen | What you do there |
|---|---|
| **Notifications** | Add and test alert channels: Slack, Discord, Teams, Google Chat, Mattermost, Telegram, WhatsApp, SMS, email, PagerDuty, ntfy, Pushover, Gotify, or any webhook. See [Notifications](notifications.md). |
| **Status pages** | Create and publish your public pages, set branding and logo, and choose which monitors appear under which names. See [Per-org status pages](per-org-status.md). |
| **Variables** | Org-scoped values and write-only secrets that monitors reference as `{{key}}`. See [Variables and secrets](variables.md). |
| **Team** | Invite people by email, set roles, resend or revoke pending invitations, and remove members. See [Team](team.md). |
| **Usage** | Current versus limit for every quota on your plan, as progress bars. The numbers here are the numbers you are enforced at. See [Plans and limits](hosted/plans-and-limits.md). |
| **Account** | The sign-in methods that open your account — add one, or remove one that is no longer yours. The Telegram, Pushover and Slack accounts that put your name on incidents you acknowledge from them, under linked apps. Also export your data as JSON, or delete your account. Deletion is reversible for 30 days: sign back in and confirm the restore on the page that appears. Signing in on its own does not cancel it. |
| **Sessions** | See every device signed in to your account, revoke a single session, or sign out everywhere. |
| **API tokens** | Create and revoke personal API tokens for the REST API and Terraform. See [Managing tokens](authentication.md#managing-tokens). |

On-call schedules and escalation policies get their own screens. On the hosted service they come with the Team plan; on another plan the screens say so, and anything an organization built before a downgrade keeps working there. A self-hosted install always has them; there the plan's limits only decide how many of each an organization may keep. The on-call page lists every schedule with who is on call now and who takes over next, plus a card of your own: your next shifts, a link that puts them in your calendar app, and the channels that page you. A saved schedule's page shows its calendar, where overrides are added, above the rotation.

Theme and 12- or 24-hour time format are per-user display preferences, not org settings, and follow you to every screen.

## Sharing a single monitor

A share link opens one monitor's full dashboard for anyone holding the URL, with no account and no operator controls. Credentials are redacted and the view is read-only. Useful for a chat channel or a customer who needs to watch one endpoint. See [Share links](share-links.md).

## Notes

The UI polls rather than holding a socket open, so leaving a dashboard on a wall display costs one cached query set per refresh regardless of how many screens are watching.

Times render in your browser's timezone, not the server's.
