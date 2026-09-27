+++
title = "On-call schedules and escalation policies"
date = "2026-09-27"
summary = "Rotations that hand off on their own, a calendar with overrides, your shifts in your calendar app, and escalation for when nobody answers."
+++

An incident on a monitor can now page whoever is on call. On the hosted service this is part of the Team plan, and a self-hosted install always has it.

**Schedules.** A schedule is one or more rotations, called layers. People hand off daily or weekly at the same local time, even across daylight saving, or on an interval you set. A layer can be on call at all hours or only in the hours you give it, such as Monday to Friday 09:00 to 17:00. The first layer on call at a moment is the one that pages, so working hours on one rotation and nights and weekends on another fit in one schedule.

**Calendar and overrides.** A saved schedule's page shows a calendar of who is on call each day. Click a start day and an end day, then pick who covers, to hand a holiday or a swap to someone else without touching the rotation.

**Your shifts.** Each person picks the channels that page them on the on-call page. Until they do, a page on their shift reaches no one, and the page says so. The same page lists their next shifts in the organization and offers a private link that Google Calendar, Apple Calendar or Outlook can subscribe to.

**Escalation.** A policy is a ladder of levels. Level 1 pages when an incident opens, and each level waits before the next one pages. A level can page notification channels, on-call schedules, or both. Bind a policy to a monitor, or make it the default for every monitor without its own. A monitor with a policy pages the policy's levels instead of its own channels, so put those channels on level 1 to keep them. Acknowledging or resolving the incident stops the escalation.

Docs: [on-call schedules](/docs/incidents#on-call-schedules), [paging and escalation](/docs/incidents#paging-and-escalation).
