+++
# Written by Artem Senenko. Attribution required under AGPL-3.0 sections 7(b) and 7(c); see "Blog posts" in LICENSING.md.
title = "How to get alerted when someone changes your DNS records"
date = "2026-10-12"
slug = "monitor-dns-record-changes"
excerpt = "A changed DNS record can still resolve, so uptime checks stay green. Watch the exact record set and get an alert when a record is added, removed or changed."
tags = ["dns", "monitoring", "security", "email"]
draft = false
og_image = "/static/marketing/og-monitor-dns-record-changes.png"
cta_label = "Watch your DNS records"

[[faqs]]
q = "How fast will I know about a DNS change?"
a = "The check sees the change on its first run after the server you query returns the new answer. If you query your own nameserver, that is the next check. A public resolver can keep showing the old answer from its cache for a while. The alert comes once the failure is confirmed: by default two failed checks in a row, from more than half of the regions."

[[faqs]]
q = "Can one monitor watch several DNS names?"
a = "No, one monitor watches one name and one record type. Start with six: NS, MX, TXT and CAA on your main domain, and TXT on your DMARC and DKIM names. Then add one for each other DKIM selector and one for each fixed A or AAAA record set. A hostname with both needs two."

[[faqs]]
q = "Does an exact record set catch DNS hijacking?"
a = "It catches a change in the records you monitor, as the server you query sees them. To catch a change at the registrar, watch your NS records through a public resolver, because your own nameserver keeps the old list. It does not catch an attack that only one network sees, such as a poisoned resolver cache, and it can miss a change that is undone between two checks."

[[faqs]]
q = "How do I watch a DKIM key split into several strings?"
a = "Paste only the quoted value from your zone file, with all its quoted parts on one line. The check joins the parts into one value and compares it with the live answer."
+++

> **TL;DR**
>
> Most DNS changes do not take your site down. A new MX record or an edited SPF record usually still resolves, so your uptime checks stay green. To catch these changes, list every record a name should have. Then get an alert when the live answer is different in any way.

## A DNS change does not look like an outage

Uptime monitors wait for something to break. A DNS change often breaks nothing you can see.

Here is an example. Someone adds a second MX record to your domain. MX records tell other servers where to deliver your email. Maybe it was a test of a new email provider, and nobody removed it. Maybe someone stole the password to your DNS account.

Each MX record has a priority number, and senders try the lowest one first. If the new record has a lower number than yours, your email now goes to another server first. Your website works, your checks are green, and nobody notices for weeks.

Other changes work the same way:

- Someone changes your nameservers at the registrar. Now a different server answers for your whole domain.
- Someone edits your SPF record during a migration. SPF lists the servers that may send email as you, and your newsletter tool is no longer on it.
- Someone deletes your `_dmarc` record while cleaning up old records.
- Someone changes your CAA record. Now a different certificate authority can issue certificates for your domain.

These changes can leave your DNS lookups and website checks passing.

## This happens in real attacks

In January 2019, the US Cybersecurity and Infrastructure Security Agency (CISA) published [Emergency Directive 19-01](https://www.cisa.gov/news-events/directives/ed-19-01-mitigate-dns-infrastructure-tampering). Attackers had changed DNS records on several US government domains.

They obtained credentials for a DNS account. They changed A, MX and NS records to point to their own servers. Then they could read web and email traffic as it passed through.

CISA's first order to the agencies was simple: check your public DNS records, and make sure each one points to the right place. Start with NS records.

A check like that tells you the records were right on one day. A monitor does the same check every few minutes, so you find out on the day of the change.

## Why "does it resolve?" is not enough

Most DNS checks ask one of two questions.

The first is "Does the name resolve?" This catches a deleted record or an [expired domain](/blog/domain-expired-but-site-still-up). It does not catch a changed record, because a changed record still resolves.

The second is "Does the answer contain this value?" For example, you enter your mail server, and the check fails if it disappears. This is better. But the check passes as long as your value is somewhere in the answer. So it misses:

- an extra MX or NS record next to yours;
- an SPF record changed around the part you check;
- one A record replaced, when you have several.

To catch these, the check must know every record that should be there. Then it can fail on anything else.

## Watch the exact record set

An exact record set is a list of every record that one name should have, for one record type. The check reads the live answer and compares it with your list:

- A record in your list but not in the answer is **missing**.
- A record in the answer but not in your list is **unexpected**.
- A changed record shows up as both. The old value is missing and the new value is unexpected.

Any difference fails the check. The order of the records does not matter, and neither does the TTL.

To write the list, start from the records your domain has now. The [DNS lookup tool](/tools/dns-lookup) shows them from Cloudflare and Google side by side. Check that every record is one you expect. If the two resolvers disagree, a change is still spreading, so wait until they match.

In Uptimepage, create a DNS monitor and choose **exact set** as the answer match. Write one record per line, the way a DNS answer shows it:

```text
10 mail.example.com.
20 alt.example.com.
```

Say someone adds a third MX record. The next check fails, and the result shows what changed:

```json
{
  "domain": "example.com",
  "record_type": "MX",
  "missing": [],
  "unexpected": ["5 mx.attacker.example."],
  "answers": ["10 mail.example.com.", "20 alt.example.com.", "5 mx.attacker.example."],
  "expected_contains": null,
  "matched": false
}
```

The new MX record has priority 5, lower than yours, so senders try it first.

The check ignores small differences that would only cause false alarms:

- Upper and lower case in names do not matter, so `10 Mail.Example.com` matches `10 mail.example.com.`
- The dot at the end of a name is optional.
- Numbers and IP addresses are compared by value, so `2001:0db8::1` matches `2001:db8::1`.

TXT values must match exactly, including case. In SPF and DKIM, every character counts.

Enter only the record value, without the name, TTL or type. For TXT records, you can paste quoted strings from your zone file, with all parts on one line.

## Which records to watch

You do not need this for every record. Start with the ones where a quiet change hurts the most.

| Record | Name | What it controls |
|---|---|---|
| NS | `example.com` | Which servers answer for your whole domain |
| MX | `example.com` | Where your email goes |
| TXT | `example.com` | Your SPF record: who may send email as you |
| TXT | `_dmarc.example.com` | Your DMARC policy: what receivers do with fake email from your domain |
| TXT | `selector._domainkey.example.com` | Your DKIM public key, which receivers use to check your email signatures |
| CAA | `example.com` | Which certificate authorities may issue certificates for you |
| A, AAAA | your server hostnames | Only if your servers have fixed IP addresses |

Your main domain often has other TXT records too, such as verification records from other services. Put all of them in the list. A new one means someone connected a new service to your domain, and you want to know about that.

One monitor watches one name and one record type. Start with six monitors, one for each of the first six rows. Then add one for each other DKIM selector you use, and one for each fixed A or AAAA record set. A hostname with both an A and an AAAA record needs two monitors.

If you set up only one, make it NS. CISA put NS first too. Whoever changes your NS records controls all your other records.

## Query your own nameserver

Normally a DNS lookup goes through a resolver, such as `1.1.1.1` or the one from your internet provider. The resolver keeps each answer for a while. This time is called the TTL. If your MX record has a TTL of one hour, the resolver can show the old answer for about an hour after a change. Sometimes it is longer: a resolver that cannot reach your nameserver may [keep serving the old answer](https://www.rfc-editor.org/rfc/rfc8767.html#section-1) until it can.

You can skip this delay. Set the monitor's resolver to the IP address of your own nameserver, the one that holds your real records (your authoritative nameserver). Then every check sees your records as they are right now.

There is one exception: NS records. If someone changes your nameservers at the registrar, your own nameserver does not know. It still answers with the old, correct list. So watch NS through a public resolver, such as `1.1.1.1` or `8.8.8.8`. A public resolver follows the registrar, like your visitors do. You can [check what a public resolver sees](/tools/dns-lookup) for any name before you create the monitor.

## When not to use an exact set

Some records change on purpose. An exact set on them only gives you false alarms.

Names behind a CDN or a load balancer get their IP addresses from a pool. The answer can change over time and depending on where you query from. I wrote about this in [why DNS returns different IP addresses](/blog/why-dns-returns-different-ip-addresses). An exact A set on such a name would fail all the time. Watch the CNAME record that points to the CDN instead.

SOA records change every time you edit your zone, because their serial number goes up. `_acme-challenge` TXT records come and go with each certificate renewal. Do not use an exact set for these either.

## Keep the list next to your DNS

If you manage DNS with Terraform, put the monitor in the same repository:

```hcl
resource "uptimepage_target" "mx" {
  name     = "mx example.com"
  interval = 300

  check = {
    type = "dns"
    dns = {
      domain           = "example.com"
      record_type      = "MX"
      expected_records = ["10 mail.example.com.", "20 alt.example.com."]
    }
  }
}
```

Now a pull request that changes an MX record must change the monitor too. An MX change that nobody reviewed fails the check. You need Terraform provider 0.15.0 or later. Read more in [monitoring as code](/blog/monitoring-as-code). All DNS options are in the [DNS monitor docs](/docs/monitor-types#dns).

## Common questions

<details class="mk-faq">
<summary>How fast will I know about a DNS change?</summary>
<div class="mk-faq__body">

The check sees the change on its first run after the server you query returns the new answer. If you query your own nameserver, that is the next check. A public resolver can keep showing the old answer from its cache for a while. The alert comes once the failure is confirmed: by default two failed checks in a row, from more than half of the regions.

</div>
</details>

<details class="mk-faq">
<summary>Can one monitor watch several DNS names?</summary>
<div class="mk-faq__body">

No, one monitor watches one name and one record type. Start with six: NS, MX, TXT and CAA on your main domain, and TXT on your DMARC and DKIM names. Then add one for each other DKIM selector and one for each fixed A or AAAA record set. A hostname with both needs two.

</div>
</details>

<details class="mk-faq">
<summary>Does an exact record set catch DNS hijacking?</summary>
<div class="mk-faq__body">

It catches a change in the records you monitor, as the server you query sees them. To catch a change at the registrar, watch your NS records through a public resolver, because your own nameserver keeps the old list. It does not catch an attack that only one network sees, such as a poisoned resolver cache, and it can miss a change that is undone between two checks.

</div>
</details>

<details class="mk-faq">
<summary>How do I watch a DKIM key split into several strings?</summary>
<div class="mk-faq__body">

Paste only the quoted value from your zone file, with all its quoted parts on one line. The check joins the parts into one value and compares it with the live answer.

</div>
</details>
