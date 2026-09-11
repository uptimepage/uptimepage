# Notifications

A monitor going down is only useful if it reaches someone. A **notification channel** is a place alerts are delivered: a Slack channel, a phone number, an on-call rotation in PagerDuty, your own webhook. Channels belong to the org, and each monitor binds the ones that should hear about it.

Manage them under **Settings → Notifications**. The endpoint contract for everything below is in [REST API](api.md#notification-channels).

## The model

Three pieces, deliberately separate:

- **The channel** is the destination and its credentials. Created once, reused by any number of monitors.
- **The binding** attaches a channel to a monitor. A monitor with no bindings still opens incidents and still shows on a status page; it just pages nobody.
- **The policy** lives on the monitor, not the channel: how many failures before alerting, how often to remind, whether recovery is announced.

The point of the split is blast radius. A noisy marketing-site monitor and your payment API can share one Slack channel while alerting on completely different thresholds, and neither has to duplicate the webhook URL.

## Choosing a channel type

| Type | What you provide | Notes |
|---|---|---|
| Slack, Discord, Teams, Google Chat, Mattermost | An incoming webhook URL | Discord, Teams and Google Chat URLs are host-checked, so a wrong-vendor paste is refused up front. A Slack URL is only checked for `https`, so verify it with a test send. Mattermost has no single host to check — Cloud and on-prem installs each serve their own — so its URL is checked for `https` and for the `/hooks/<key>` path every incoming webhook ends in, and a URL copied from the REST API is refused. Slack, Discord and Mattermost also take an optional group ping, see below |
| Telegram | One-tap link, or your own bot token and chat id | The one-tap flow is available where the platform runs a central bot |
| WhatsApp | One-tap link, or Business Cloud API credentials and a template | Bring-your-own needs an approved one-parameter template |
| SMS | Credentials for your own gateway: Twilio, Vonage, Telnyx, Plivo, or Sinch | One message per alert, trimmed to bound per-segment cost |
| Email | One address | Must be verified before anything is delivered, see below |
| PagerDuty | An Events API v2 routing key | The only type that drives the destination's own incident lifecycle |
| ntfy, Pushover, Gotify | Server and topic, app and user key, or your own Gotify server and an application token | Urgency maps to the service's own priority levels |
| Webhook | An HTTPS URL, optional headers, optional signing secret | The escape hatch for anything not listed above |

PagerDuty is worth calling out: opens send a `trigger` and resolutions send a `resolve`, correlated by the incident id, so one incident here maps to exactly one PagerDuty alert that opens and closes with it rather than a pile of unrelated pages.

If you use the generic webhook with a signing secret, every delivery carries a timestamp and an HMAC-SHA256 signature so your receiver can verify the payload came from us and reject replays.

## Creating one

A new account starts with one already: the address you signed up with, wired up and verified, because the sign-in itself proved you own that inbox. It behaves like any other channel from there, so rename it, point it elsewhere, or delete it. Self-hosted installs get the same thing for the bootstrap owner, but only where a real mail provider is configured, since the `log` provider delivers nowhere.

Add the channel, then **test now** before saving. The test sends one clearly labelled synthetic alert through the real transport, so a wrong webhook URL or an expired token surfaces immediately instead of during your first outage. Testing works on a saved channel too, including a disabled one.

Two types need a second step:

- **Email** is verification-gated. The channel is created unverified and a single-use link, valid 24 hours, goes to the address. Until it is confirmed, every delivery fails. Changing the address resets the gate and sends a new mail. There is a daily cap on resends.
- **Slack and Discord** can be connected with a one-tap button where the operator has configured it: their consent screen picks the destination, and you land back on the ready-made channel. Otherwise paste a webhook URL.

Secrets are sealed at rest and never shown again. On edit they stay masked behind a replace toggle, and leaving the toggle off keeps the stored value untouched.

### Pinging a group in Slack, Discord or Mattermost

A message in a busy channel is easy to miss, so a Slack channel takes an optional **ping on alert** that leads the text: `@here`, `@channel`, a user-group id (`S…`) or a member id (`U…`, or `W…` on Enterprise Grid), space or comma separated, up to five. A plain `@sre` handle is inert in a webhook message, which is why the id is what the field wants; you find a group's id on its page under **Slack → People → User groups**, and a member's under their profile's **Copy member ID**. Only the events that need a human carry the ping: opened, reopened, escalated, and monitoring interrupted. Recovery and resumed messages stay silent, and a **test now** send drops `@here`/`@channel` so checking your config does not wake the room — a group or member ping still rides along, so a wrong id shows up as dead text on the test.

Discord takes the same field with its own tokens: `@everyone`, `@here`, a role id (`&123…`) or a member id (`123…`), which you copy after turning on **Advanced → Developer Mode** in Discord. Discord copies a role id and a member id in the same shape, so a role's has to be typed with the leading `&`; without it the message pings a member that does not exist, which shows up as dead text. Discord resolves no mention inside a card, so the ping rides the line above it, and the message allows exactly the roles and members you listed. Nothing else in it can ping, whatever a monitor happens to be named.

Mattermost is the simplest of the three, because it resolves a plain handle: `@channel`, `@here`, `@all`, or a username or group name, up to five. No ids to copy. Handles there are lowercase, so the field is lowercased on the way in and a pasted `@Bob` still reaches Bob. The trade is that Mattermost only resolves the name when the alert lands, so a handle belonging to nobody cannot be caught when you save it — the ping is simply absent from the message. The same lifecycle applies: recovery stays silent, and a **test now** send drops `@channel`/`@here`/`@all`. Nothing a monitor is named can ping the channel either, whatever it contains.

Changing the ping goes through the same replace-config toggle as the webhook URL, so re-enter the webhook when you edit it.

### What a chat alert looks like

Slack, Discord, Teams and Mattermost get a laid-out card rather than one line of text. The heading names the monitor with a colour or emoji for how bad it is, and under it sit the state, the start time rendered in each reader's own timezone, and, for a monitor watched from several regions, which ones have confirmed the failure and which have not yet (a region one check behind, not a healthy one). An open incident also carries the error the check saw, and one you declared by hand says so instead of claiming a detection. A resolved message reports how long the incident ran, and an interrupted one how long it has been quiet. Wherever the app knows its own public address, which self-hosters set in config, the card carries a link straight to the incident.

Each renders that same card in its own format: Block Kit on Slack, an embed with a coloured bar on Discord, an Adaptive Card on Teams, a message attachment with a coloured bar on Mattermost. The layout is fixed, so there is nothing to configure. On Slack the one-line version still rides along as the text a phone shows in its notification preview. Google Chat still gets plain text.

### Delegating the connect step

When the credentials belong to someone outside the org, say the Slack workspace admin or the person who owns the shared inbox, you do not need to chase them for secrets. **Settings → Notifications → delegate the connect step** mints a single-use `/c/{code}` link. Whoever opens it can connect exactly one channel to your workspace and nothing else, with no account needed; the link expires after 7 days and can be revoked before use. The same flow is scriptable through the delegate endpoints in the [REST API](api.md#notification-channels).

## Binding a monitor

The monitor form has a **Notifications** section listing your channels with a checkbox each. It only appears once the org has at least one channel, so create the channel first. When the org has exactly one, a new monitor ticks it for you; with several the form leaves the choice alone rather than guessing.

### Routing by tag

Ticking a box per monitor stops scaling once one team owns a dozen of them, so a channel can carry a **route by tag** rule instead: it also pages any monitor carrying one of those tags. One tag in common is enough, and case does not have to match. Tags are picked as chips from the org's own vocabulary, the same control the monitor form uses, and the rule sits outside the replace-config toggle, so it can be changed without re-entering the webhook.

The rule is resolved when an alert fires, not written into the monitors. Retag a monitor and its coverage moves with it; create a monitor already tagged `db` and the `db` channel pages it from its first check, with nothing to remember. A monitor covered only by a rule shows the channel marked **by tag** in its own form, and no longer warns that it alerts nobody.

Explicit bindings still work and stack with rules: a channel bound to a monitor and matching its tags pages once. Where an escalation policy applies to a monitor, the policy's own rungs decide who is paged, and both bindings and tag rules stand aside.

Alongside the bindings sit the controls that decide when they fire:

| Setting | Default | What it does |
|---|---|---|
| Consecutive failures | 2 | Failing checks before an incident opens. The same count of passing checks closes it, which is what damps flapping. |
| Region agreement | majority | How many probe regions must agree before it counts as down. See [Probe regions](hosted/regions.md). |
| Remind while down | every hour | How long before the first reminder while an outage stays unacknowledged; each further reminder waits twice as long, up to a day. Acknowledging or resolving stops the reminders; set it to off for none. |
| Announce recovery | on | Whether the "back up" message is sent. |

The reminder interval is the setting most worth tuning. An hour is right for a customer-facing API, and far too often for a nightly batch endpoint you already know is flaky. Reminders back off rather than repeating on a fixed cadence, so an incident nobody answers ends up nudging once a day rather than paging forever, and they do not re-ping a Slack channel.

## What gets delivered

One notification when an incident opens, backing-off reminders while it stays unacknowledged, and one on recovery if announcements are on. Alerts are driven by the incident engine, not by individual check failures, so a monitor failing sixty times in an hour produces one incident and one alert, not sixty.

Failed deliveries retry with exponential backoff and are dead-lettered after the attempt cap. Per-incident delivery state is visible through the API if you need to prove whether something was sent.

A channel whose deliveries keep using up every retry is flagged **not delivering** in the channel list, and the account owners get an email saying so, at most one a day per channel so a flapping endpoint cannot fill an inbox. The channel is not turned off: alerts keep being sent to it, because a silent endpoint costs a few wasted requests per incident while switching one off costs the next outage. Any delivery that lands, including a test send on an enabled channel, clears the flag, as does turning a channel off and back on. The threshold is `escalation.channel_failure_limit`, three exhausted deliveries by default. An email channel still waiting on its verification link is left out of this: it fails every delivery by design, and the list already marks it unverified.

A channel only ever fails where something tries to page it, so one bound to quiet monitors can sit dead for a long time before anything notices. The channel list carries `last delivered` for that: a channel that has not delivered in weeks is worth a look even while nothing is flagged. A channel that has never delivered shows no line at all.

Two more messages exist that are not incident alerts: **monitoring stopped** and **monitoring resumed**. They fire when every probe covering a monitor has gone silent longer than expected and when results start flowing again. They mean the service is unwatched, not that it is down: no incident opens, and the webhook payload carries the distinct reasons `nodata` and `dataresumed`. A platform-wide probe outage on our side is suppressed rather than fanned out as a flood of these.

## Acknowledging from the notification

A page on ntfy carries an **Acknowledge** button. One tap takes the incident, without leaving the notification: reminders stop and the escalation ladder halts. The notification clears only if the acknowledgement actually landed, so a tap that is refused stays on screen rather than telling you it worked. Tapping the notification itself still opens the incident. The incident stays open until someone resolves it or the monitor recovers. Pushover does the same through its own app — acknowledging an emergency-priority page there now acknowledges the incident here, which it did not do before, so a responder who took the page no longer keeps getting reminders about it. Acknowledging also cancels the emergency pages still repeating on any other Pushover channel bound to that incident.

The link carries no login. Holding the notification is the whole proof, so the timeline records the acknowledgement as coming from a notification rather than crediting a member, and the person who acknowledged first stays the one on record even if someone else acknowledges afterwards. Each link is signed, works for one incident, and lapses after a week.

It is also pinned to the outage it was sent for. If an incident resolves and then comes back, tapping an alert still sitting on a phone from the earlier round is refused: the outage running now is not the one that alert was about, and silencing it on the strength of an old page would be exactly the wrong outcome. The notification stays on screen, since the tap did nothing. Your phone shows a failed-request message rather than the reason, so open the alert to see whether it was superseded or something else went wrong; the page sent for the new round carries a button that works.

That has a consequence worth knowing if you publish to an open ntfy topic: on ntfy.sh a topic's name is its only access control, so anyone subscribed to it can read your alerts *and* use the acknowledge link in them. Acknowledging never resolves or hides anything and it cannot close an incident, but there is no way to un-acknowledge one either, so on a shared instance prefer a protected topic with an access token. The topic the form generates for you is already unguessable.

Gotify has no action buttons, so its notifications keep their one tap-through to the incident instead.

Scheduled maintenance windows do **not** silence channel alerting. They repaint the public status page and notify status-page subscribers, but a monitor that fails during a window still opens an incident and still pages its channels. To stay quiet through planned work, disable the monitor or unbind its channels for the duration.

## On-call

Where on-call schedules are enabled, paging works differently: an incident targets a person or a rotation, that resolves to whoever is on shift, and they are reached through the channels **they** opted into on the on-call page. A member who has chosen no channels resolves as on-call but cannot be paged. See [Incident management](incidents.md#paging-and-escalation).

## Deleting a channel

Deleting removes it from every monitor bound to it. The edit page lists those monitors so the blast radius is visible before you confirm. Monitors that lose their last channel keep running and keep opening incidents; they simply stop telling anyone.

A channel linked through a central Telegram bot can also be disabled from the other side. If the bot is removed from the chat or the chat sends `/stop`, the channel is disabled with a note explaining why, and re-enabling it clears the note.

Email channels have the same property through the one-click stop link (RFC 8058) that every alert mail carries. **Anyone who receives the mail can use it, and it disables that email channel for the whole org**, not just for the person who clicked; the channel shows a "recipient stopped delivery" note, and re-enabling clears it. Worth knowing before you forward alert mail around: a recipient tired of the noise can switch the channel off for everyone.

## Limits

The number of channels an org can hold is capped by its plan (`max_notification_channels`). Channel names must be unique within the org. Test sends count against the same per-minute budget as other test operations. See [Quotas and rate limits](quotas.md).

## Managing them as code

Notification channels are a Terraform resource, so a channel and the monitors bound to it can live in the same config as the rest of your infrastructure. The one-tap linked types are excluded, since their credentials belong to the platform's bot rather than to your org. See [Terraform](terraform.md).
