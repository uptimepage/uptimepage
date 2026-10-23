+++
title = "DNS monitors can expect an exact record set"
date = "2026-10-12"
summary = "A DNS monitor can list every record it expects and go down when one is added, removed or changed. Names like _dmarc and PTR on an IP address work too."
+++

**An exact record set.** A DNS monitor used to pass when an answer contained a piece of text. It can now list every record the answer must hold, one per line, and goes down when a record is added, removed or changed: a stray MX, a dropped A record, an edited SPF line. Pick **exact set** under the answer match in the monitor form, or send `expected_records` through the API or the MCP tools. Up to 50 records.

**How records compare.** Order and repeats do not matter. Names compare without case or the trailing dot, and numbers and addresses by value, so `10 Mail.Example.com` matches `10 mail.example.com.` and `2001:0db8::1` matches `2001:db8::1`. TXT and CAA values compare as written, case included. Write a record the way the result's answers show it, or in zone file form. A value the record type could never answer with is refused when you save, so a typo cannot leave a monitor down for good.

**See what changed.** When the set differs, the result lists the records that are `missing` and the ones that are `unexpected`. Answers now hold only the record type you asked for; a CNAME chain the resolver followed shows under `aliases`. A contains match still searches the chain, so existing monitors judge as before.

**Underscore names.** Names such as `_dmarc.example.com`, `s1._domainkey.example.com` and `_sip._tcp.example.com` were refused when you created the monitor. DNS monitors accept them now.

**PTR on an IP address.** A PTR monitor on an IP address sent a forward lookup and failed on every check. It now looks up the address's reverse name. The other record types refuse an IP address when you save, since they could never pass.

**Terraform.** Provider 0.15.0 adds `expected_records` to the `dns` block of `uptimepage_target` and accepts underscore names.

Docs: [DNS monitors](/docs/monitor-types#dns), [DNS through the API](/docs/api#dns), [Terraform](/docs/terraform).
