# Plans and limits

This page covers the hosted service at `uptimepage.dev`. Nothing here binds a self-hosted instance: it ships with the same code, an unattended first run seeds its owner onto the largest plan, and a `plan_overrides` row raises any limit you want either way. See [Quotas and rate limits](../quotas.md) for the mechanism both modes share.

## The plans

| Plan | Who gets it |
|---|---|
| Standard | Every new account, free, no card |
| Founding | The first 1,000 accounts, granted at signup and kept for life, free |
| Pro | Coming |
| Team | Coming |

Prices, monitor counts, check intervals, history windows, seats, and status-page limits are on the [pricing page](https://uptimepage.dev/pricing), which is the number you are actually enforced at.

## Limits belong to your account, not to one organization

Your plan sits on your account. Every limit on it is a total across all of the organizations you own: 50 monitors on Founding means 50 in all, whether they live in one organization or three. Organizations are workspaces for separating environments, clients or teams, and splitting your monitors across more of them never adds capacity.

How many you may open is itself a plan limit: one on Standard, three on Founding, five on Pro, ten on Team. Deleting an organization frees its slot after it leaves the recovery window, and restoring one is refused if you have filled the slot in the meantime.

Being invited into somebody else's organization is separate. You get access to their workspace, and their limits apply there. Nothing you own is added to theirs, and nothing of theirs is added to yours. A person who belongs to several of your organizations takes one seat, not one per organization.

Browser flow monitors are counted separately from everything else, because each run drives a real browser rather than sending a request:

| Plan | Flow monitors |
|---|---|
| Standard | 0 |
| Founding | 1 |
| Pro | 3 |
| Team | 10 |

They still count toward your monitor total, and they run no faster than every five minutes. Creating one past the cap returns `422 QUOTA_EXCEEDED` naming `max_flow_checks`. See [Flow](../monitor-types.md#flow) for what one can and cannot do.

Founding is granted automatically while spots remain. There is nothing to claim and no code to enter. Once an account holds it, it keeps those limits for as long as the account is open, at no cost, and we do not downgrade it later.

## What happens at a limit

Resource quotas (monitors, seats, status pages, channels, tokens) are enforced at the write, inside the same statement that creates the row, so parallel creates settle exactly at the limit and never above it. Crossing one returns `422` with `QUOTA_EXCEEDED` and a `details` object naming the quota, your current count, and the limit.

Request budgets are per minute, per account and per user, split into reads, writes, bulk operations, test runs, and check-now. Crossing one returns `429` with a `Retry-After` header. Checks themselves are never rate limited: the scheduler does not pass through that middleware, so a busy API does not slow your monitoring.

Two limits behave slightly differently. Pending invitations return `409 INVITATIONS_LIMIT`. A check interval below your plan floor returns `422 MIN_CHECK_INTERVAL`, and the floor is the higher of your plan's interval and the minimum for that monitor kind (twelve hours for domain expiry, an hour for TLS, five minutes for flow, a minute for heartbeat, ten seconds for the rest).

If your plan changes, the floor applies to monitors you already have, not only to the next one you create. A monitor set faster than your new floor is checked at the floor instead. We do not edit your monitor to do it: the interval you chose stays on it, so moving back up restores the old rate with nothing for you to redo. Regions follow the same rule: a monitor assigned to more regions than the plan allows is probed from the first ones, default-on regions first and then by id, and the assignment itself is kept. Text-message alerts work the same way. A plan without them refuses a new SMS channel with `403 SMS_ALERTS_DISABLED`, and one you already have keeps sending. The links we mail your subscribers point at your custom domain only on a plan that includes one; otherwise they point at the page's own subdomain until the plan comes back.

## Seeing where you stand

`GET /api/v1/orgs/{id}/usage` returns your plan plus current-versus-limit for every quota, the rate budgets, and the feature flags. The counts are your account's totals across every organization you own, which is what the limits apply to, so they can be larger than what the organization in the URL holds by itself. The same numbers render as progress bars under **Settings → Usage** in the app. The reported limit is the enforced limit by construction: both read the same plan row and the same count query.

## If you move to a smaller plan

Nothing is deleted. Whatever no longer fits is put on hold: monitors stop being checked and disappear from your status page, extra status pages stop answering, and everything keeps its settings exactly as you left them. Moving back up releases them as they were.

We hold the newest first, on the assumption that the ones you set up earliest are the ones you rely on. If that is wrong, **Settings → Usage** lets you tick exactly what to keep, and we hold the rest. Ticking fewer than your plan allows is fine: we do not fill the spare slot back up with something you just chose to let go. Once your plan covers everything again, all of it comes back, including what you had left unticked. Held items still count toward your limit, so freeing a slot means deleting something rather than leaving it parked.

## If a payment fails

A failed card never cuts your service the same day. The account stays on its full plan for a two-week grace window while we retry the card and email you, so a card that expires while you are away costs you nothing if you fix it in time. Pay at any point inside the window and it is as if nothing happened.

Only if the window closes unpaid does the plan drop — to the plan you were on before you paid, which for a founding account is founding, never lower. Even then nothing is deleted: whatever no longer fits is held, exactly as above, and paying again releases all of it.

## Raising a limit

Delete monitors you no longer need, or write to us at <hello@uptimepage.dev> and say what you are running into. Paid plans are not open for self-service yet, so limit changes today are a conversation rather than a checkout.

If you would rather have no limits at all, run your own instance. It is the same product under AGPL, with no plan, no seat count, and no account with us. Start at [Deployment](../deployment.md).
