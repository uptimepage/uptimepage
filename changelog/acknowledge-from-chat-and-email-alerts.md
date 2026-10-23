+++
title = "Acknowledge from chat and email alerts, and see who did"
date = "2026-09-30"
summary = "Slack, Discord, Teams, Google Chat, Mattermost and email alerts carry an Acknowledge control, and the incident shows who acknowledged."
+++

You can now acknowledge an incident from its chat and email alerts, and the incident shows who acknowledged it.

**Who acknowledged.** Any member can acknowledge, on call or not. The dashboard banner, the console and the incident page list who acknowledged, in order. The first keeps the credit: they stay the acknowledger on record, and their time is what MTTA measures. Once you have acknowledged in your own name, the button there goes away for you, and a reopened incident starts a new list.

**From chat and email alerts.** Slack, Discord, Microsoft Teams, Google Chat and email alerts carry an Acknowledge button, and Mattermost alerts end with an Acknowledge link. It opens the incident's acknowledge page, where you sign in and take it in your own name. Opening the page changes nothing, so a link preview or a mail scanner cannot acknowledge for you, and an alert from an earlier outage takes nothing once the incident has come back.

**Inside Slack and Discord.** A channel connected with **add to Slack** or **add to Discord** from now on carries an Acknowledge button that works in the chat itself. A press acknowledges at once. Until you link your Slack or Discord account, it is recorded as coming from Slack or Discord, and the reply, which only you see, brings a link: open it, sign in, and later presses carry your name. The chat also gets a reply under the alert naming who took it, without mentioning anyone. A channel whose webhook was pasted, or one connected before this change, keeps the page link.

**Telegram and Pushover.** A page from the Uptimepage Telegram bot now carries an Acknowledge button, and a Pushover emergency acknowledgement reports who took it. Link Telegram from your account settings. Pushover links itself: acknowledge an emergency page from an account nobody has linked, and it brings you a one-time link, at most once a week. A name is never guessed from where a page was sent, since a shared chat or a Pushover group reaches more than one person.

**Turning the button off.** Every channel that carries the button has an Acknowledge button switch, on by default. Turn it off for a room shared with a customer or an open ntfy topic, and the buttons already sent stop working too. The API takes it as `acknowledge_button`.

On a self-hosted install, the buttons inside Slack and Discord need your own Slack and Discord apps, and the Telegram button needs a central Telegram bot, each set up as the configuration docs describe. Without the Slack and Discord apps, those channels keep the page link.

Docs: [acknowledging from the notification](/docs/notifications#acknowledging-from-the-notification), [the incident console](/docs/incidents#the-console), [Slack and Discord app setup](/docs/configuration#provider-oauth-connect-add-to-slack--add-to-discord).
