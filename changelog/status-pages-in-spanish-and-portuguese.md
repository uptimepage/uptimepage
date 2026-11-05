+++
title = "Status pages in Spanish and Portuguese"
date = "2026-11-05"
summary = "A status page can now be in Spanish or European Portuguese: its labels, dates, subscribe flow and subscriber emails."
+++

Status pages now come in Spanish and European Portuguese as well as English and German. Pick Español or Português (Portugal) under Language in the page editor, set `public_locale` to `es` or `pt-PT` through the REST API, or set `locale` on `uptimepage_status_page` with Terraform provider 0.21.0 and later.

Both cover what German covers: status labels and headings, the subscribe dialog, private page sign-in, dates and durations, the title of an incident you never named, the status badge, and the emails your subscribers get. Each page formats dates its own way in every browser and sends its `Content-Language`. Text you write appears as written.

Docs: [language](/docs/public-status#language).
