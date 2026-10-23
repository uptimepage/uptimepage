+++
title = "Organizations are managed from the console now"
date = "2026-09-05"
summary = "Create, rename, delete and restore organizations from /settings/organizations. The API existed, the button did not."
+++

Until now there was no way to create a second organization in the app. The API existed, the button did not.

`/settings/organizations` covers create, rename, delete and restore. `/settings/team` is only about people.

- Deleting an org asks you to type its name back.
- A deleted org's slug is held for the whole restore window, so nobody can grab it in between.
- If an org blocks your account deletion because you are its only owner, you can hand it a second owner instead of deleting it.
- Sessions pointed at a deleted org are moved, not stranded on a login loop.

Docs: [organizations](/docs/organizations).
