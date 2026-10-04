## Public status page, its incident and archive pages, the subscribe flow and
## subscriber emails. Values are plain text: templates and emails escape them.

## Dates and numbers

month-1 = January
month-2 = February
month-3 = March
month-4 = April
month-5 = May
month-6 = June
month-7 = July
month-8 = August
month-9 = September
month-10 = October
month-11 = November
month-12 = December
month-short-1 = Jan
month-short-2 = Feb
month-short-3 = Mar
month-short-4 = Apr
month-short-5 = May
month-short-6 = Jun
month-short-7 = Jul
month-short-8 = Aug
month-short-9 = Sep
month-short-10 = Oct
month-short-11 = Nov
month-short-12 = Dec
date-month-year = { $month } { $year }
date-day = { $day } { $month } { $year }
date-stamp = { $day } { $month } { $year } { $time } UTC
decimal-separator = .
percent = { $value }%
duration-seconds = { $n }s
duration-minutes = { $n }m
duration-hours = { $n }h
duration-days = { $n }d
elapsed-ago = { $duration } ago
starts-in = (in { $duration })

## Page chrome

skip-to-content = Skip to main content
title-status-suffix = Live uptime and incident history
title-archive-suffix = Past outages
rss-link-title = Incidents (RSS)
rss-aria = Subscribe to incident updates via RSS
rss-title = { $page } Incidents
rss-description = Operational status
rss-phase-investigating = investigating
rss-phase-identified = identified
rss-phase-monitoring = monitoring
rss-phase-resolved = resolved
rss-phase-postmortem = postmortem
source-code = Source code (AGPL-3.0)
badge-operational = operational
badge-minor = minor disruption
badge-maintenance = maintenance
badge-partial = partial outage
badge-major = major outage
badge-degraded = degraded
badge-no-data = no data
powered-by = Powered by
licenses = Licenses
footer-updated = Updated
back-to-status = ← Back to status
og-status-description = Live and past status for { $name }: current uptime for every component, open and recent incidents, scheduled maintenance windows, and email or webhook updates.
og-incident-description = { $title }: current phase, when it started and ended, and every update posted on the { $page } page.
og-incident-description-affecting = { $title }, affecting { $component }: current phase, when it started and ended, and every update posted on the { $page } page.
og-archive-description = Every incident published on the { $page } page, grouped by month, with the components affected, when each one started and ended, and the updates posted.

## Status labels

overall-operational = All Systems Operational
overall-maintenance = Maintenance in progress
overall-minor = Minor Service Disruption
overall-partial = Partial System Outage
overall-major = Major System Outage
overall-aria-operational = All systems operational
overall-aria-maintenance = Maintenance in progress
overall-aria-minor = Minor service disruption
overall-aria-partial = Partial system outage
overall-aria-major = Major system outage
state-operational = Operational
state-degraded = Degraded
state-partial-outage = Partial outage
state-major-outage = Major outage
state-maintenance = Maintenance
state-no-data = No data
phase-investigating = Investigating
phase-identified = Identified
phase-monitoring = Monitoring
phase-resolved = Resolved
phase-postmortem = Postmortem
auto-title = { $component } { $status }
auto-title-down = down
auto-title-degraded = degraded
auto-title-error = error
auto-title-generic = Service disruption

## Status page

last-checked = Last checked
active-incidents-heading =
    { $count ->
        [one] Active incident
       *[other] Active incidents
    }
incident-started = Started
view-incident = View incident →
maintenance-heading = Scheduled maintenance
maintenance-in-progress = In progress
maintenance-upcoming = Upcoming
maintenance-starts = Starts
maintenance-ends = ends
maintenance-affects = affects
legend-label = Status colour key
group-other = Other
component-uptime-history = uptime history
component-uptime-history-sr = for { $name }, opens in a new tab
day-strip-label = Daily status history for { $name }, last 90 days. Arrow keys navigate days.
day-ago = { $days } days ago
strip-start = 90 days ago
strip-uptime = uptime
strip-today = Today
uptime-none = —
history-summary-no-data = { $days } days, no data
history-summary-clean = { $days } days, no incidents
history-summary-degraded = { $days } days, { $degraded } degraded
history-summary-outages =
    { $outages ->
        [one] { $days } days, { $outages } outage, { $degraded } degraded
       *[other] { $days } days, { $outages } outages, { $degraded } degraded
    }
no-components = No public components have been configured yet.
past-incidents-heading = Past incidents (30 days)
older-incidents = Older incidents →
popover-no-downtime = No downtime recorded on this day.
popover-related = Related

## Incidents

incident-ongoing = Ongoing
incident-resolved-suffix = (resolved)
incident-ended = Ended
incident-duration = Duration
incident-updates = Updates
incident-no-updates = No operator updates have been posted for this incident yet.
postmortem-heading = Postmortem
postmortem-summary = Summary
postmortem-root-cause = Root cause
postmortem-impact = Impact
postmortem-action-items = Action items
postmortem-published = Published
archive-heading = Incident history
archive-empty = No incidents recorded.

## Subscribe dialog

subscribe-title = Subscribe to updates
subscribe-close = Close
subscribe-lead = Get notified whenever this page posts an incident or maintenance update.
subscribe-method = Delivery method
subscribe-email = Email
subscribe-webhook = Webhook
subscribe-email-label = Email address
subscribe-webhook-label = Webhook URL
subscribe-webhook-hint = We POST a verification first, then JSON on each update.
subscribe-submit = Subscribe
subscribe-privacy = Used only to send this page's updates. Unsubscribe any time.
subscribe-feed-prompt = Prefer a feed?

## Subscribe flow

notice-copy = copy
notice-copied = copied
notice-copy-aria = Copy to clipboard
notice-back = ← Back to status page
notice-page-not-found = Page not found
notice-page-not-found-body = This status page isn't available.
notice-check-address = Check the address
notice-invalid-email = That doesn't look like a valid email address.
notice-blocked-email = That address can't be used to subscribe.
email-risk-disposable = That looks like a temporary email address. Use one you'll still be able to read when we send you an alert.
email-risk-no-mx = That domain doesn't accept email, so we'd have no way to reach you. Check the spelling.
notice-almost-there = Almost there
notice-check-inbox = Check your inbox for a confirmation link to finish subscribing.
notice-link-expired = Link expired
notice-link-expired-body = This confirmation link is invalid, expired, or already used.
notice-subscribed = You're subscribed
notice-subscribed-body = You'll be notified when the status of this page changes.
notice-check-url = Check the URL
notice-invalid-url = Enter a valid https:// webhook URL.
notice-unreachable = Couldn't reach your endpoint
notice-unreachable-body = We couldn't deliver a verification POST to that URL. Make sure it accepts HTTPS POST and returns 2xx, then try again.
notice-webhook-subscribed = Subscribed
notice-webhook-subscribed-body = Your endpoint is verified. Save this signing secret — it signs our requests so you can verify them:
unsubscribe-title = Unsubscribe
unsubscribe-prompt = Stop receiving status updates at this address? You can subscribe again anytime.
unsubscribed-title = Unsubscribed
unsubscribed-body = You won't receive any more updates from this status page. You can close this page.
unsubscribe-invalid-title = Link invalid
unsubscribe-invalid-body = This link is invalid or has expired.

## Subscriber emails

email-fallback-page-name = status page
email-confirm-subject = Confirm your subscription to { $page }
email-confirm-heading = Confirm your subscription
email-confirm-preheader = One click and status updates start arriving here.
email-confirm-intro = You asked to receive status updates for { $page } on { $site }.
email-confirm-lead = You asked to receive status updates for { $page }. Confirm this address and updates start arriving here.
email-confirm-cta = Confirm this address to start receiving notifications:
email-confirm-button = Confirm subscription
email-confirm-expiry = This link expires in { $hours } hours and can only be used once.
email-confirm-not-you = If you didn't request this, you can remove this address in one click:
email-confirm-footnote = Didn't request this? { $remove } and nothing will be sent to it.
email-confirm-remove = Remove this address
email-status-line = Status: { $phase }
email-maintenance-scheduled = Scheduled maintenance
email-maintenance-completed = Maintenance completed
email-maintenance-when = When: { $window }
email-maintenance-window = Window
email-maintenance-ran = Ran
email-view-page = View the status page:
email-view-page-button = View status page
email-unsubscribe = Unsubscribe
email-unsubscribe-text = Unsubscribe:
email-footnote = You're receiving this because you subscribed to { $page }. { $unsubscribe }.
