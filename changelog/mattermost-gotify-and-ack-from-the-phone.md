+++
title = "Mattermost and Gotify channels, and acknowledging from the phone"
date = "2026-09-08"
summary = "Two new notification channels, and an incident can be acknowledged straight from an ntfy or Pushover notification."
+++

Two new notification channels.

**Mattermost.** Incoming webhook, works on Mattermost Cloud or your own server. Mentions use the server's own username rules, so directory-synced handles like `svc.oncall.emea` work.

**Gotify.** Point a channel at your Gotify server with an application token. Priority follows the client scale: 8 for a high-urgency open, 5 for other opens, 3 for a resolve. The incident link is the notification's click URL.

**Acknowledging from the notification itself.** An ntfy page carries an Acknowledge button, and acknowledging a Pushover emergency page in the Pushover app acknowledges the incident here too. That stops the repeats still going out on other channels. The link is signed, expires after a week, and is tied to the exact episode it was sent for, so a stale alert on a phone cannot silence a later outage.

Docs: [notifications](/docs/notifications).
