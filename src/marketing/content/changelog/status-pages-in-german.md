+++
title = "Status pages in German"
date = "2026-10-04"
summary = "A status page now has a language. Set it to German and its labels, dates, subscribe flow and subscriber emails follow."
+++

Each status page now has a language. English stays the default, and German is the first one added. Set it under Language in the page editor, or as `public_locale` on the page through the REST API.

**What changes.** Everything the page writes itself: status labels and headings, the subscribe dialog, dates and durations, the title of an incident you never named, the status badge, and the confirmation, incident and maintenance emails your subscribers get. A German page formats dates the German way in every browser and sends `Content-Language: de`.

**What stays.** Text you write appears as written: page, component and group names, the about text, incident titles and updates, and maintenance windows. Email times stay in UTC.

**One language per page.** A page with two audiences is two pages, each with its own subscribers.

**Terraform.** Provider 0.14.0 sets the page language with `locale` on `uptimepage_status_page`. Leave it out to choose the language in the console instead. Removing the line keeps the language it set, so set `en` to switch back.

Docs: [public status page](/docs/public-status#language), [per-org status pages](/docs/per-org-status), [Terraform](/docs/terraform).
