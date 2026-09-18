+++
title = "Fixes from September worth knowing about"
date = "2026-09-17"
summary = "Heartbeats page faster, Telegram supergroup moves are followed, a late region is 'not confirmed' rather than 'up', and a region can opt out of defaults."
+++

**Heartbeat monitors.** Heartbeats created through the first version of the form were judged hours apart, so a missed ping was noticed late and a manual resolve reopened the same outage. Existing intervals were corrected to a tenth of the window, between one and five minutes, and the API now refuses a coarser one. If you have heartbeats, they page faster now.

**Telegram supergroups.** When a group is upgraded to a supergroup, Telegram gives it a new chat id and the old one dies. The channel now follows the move on the first failed send and saves the new id. Bring-your-own bots are covered too.

**Incident region breakdown.** With majority quorum the last region is usually one check behind when the incident opens, and alerts named it under "up". It is now "not confirmed", and the breakdown widens as regions confirm instead of staying frozen at the snapshot taken at open.

**Self-host.** A region can be opted out of new monitors' defaults with `regions.default_selected`. It stays pickable on the form, just starts unchecked. See [multi-region probes](/docs/multi-region).
