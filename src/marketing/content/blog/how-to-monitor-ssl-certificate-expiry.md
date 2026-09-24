+++
title = "How to monitor SSL certificate expiry when auto-renewal fails"
date = "2026-09-05"
updated = "2026-09-24"
slug = "how-to-monitor-ssl-certificate-expiry"
excerpt = "Check the certificate your server serves, catch failed renewals before expiry, and set up TLS alerts alongside HTTPS monitoring. Includes OpenSSL checks."
tags = ["ssl", "tls", "certificates", "monitoring", "reliability"]
draft = false
+++

> **TL;DR**
>
> To monitor SSL certificate expiry, read the certificate your public hostname serves and alert before its expiry date. Keep an HTTPS check with certificate verification enabled alongside it. Renewal can succeed on disk while your server keeps serving the old certificate, so a successful renewal log is only part of the evidence.

You install a certificate, enable auto-renewal, and close the ticket. Months later, a customer sends you a browser warning. The renewal job says it ran successfully.

That situation is possible without either side lying. The certificate authority issued a new certificate. Your web server, load balancer, or CDN still needs to serve it. Monitoring needs to reach that last step.

People still search for "SSL monitoring", though modern HTTPS uses TLS. Here, both mean watching the certificate presented when someone connects to your service.

## Check the certificate customers receive

Open the [SSL certificate checker](/tools/ssl-certificate-checker), enter your public hostname, and use port 443 for ordinary HTTPS. Check `app.example.com` and `api.example.com` separately if customers depend on both.

Read the expiry date, the names the certificate covers, and the issuer. Keep the resolved IP in mind: a lookup observes the endpoint it reached, which matters when several servers can answer for the same hostname.

For a terminal check, replace `app.example.com` with your hostname:

```bash
openssl s_client \
  -connect app.example.com:443 \
  -servername app.example.com \
  </dev/null |
  openssl x509 -noout -dates -issuer -subject
```

[`s_client`](https://docs.openssl.org/3.0/man1/openssl-s_client/) opens the TLS connection. `-servername` sends the hostname through Server Name Indication (SNI), so a server hosting several sites can select the right certificate. [`x509`](https://docs.openssl.org/3.0/man1/openssl-x509/) prints its validity dates; `notAfter` is the expiry time.

This command inspects the certificate. It does not establish that the hostname and trust chain pass verification. If the connection fails or no certificate can be parsed, investigate that failure rather than interpreting missing output as a healthy result.

## When renewal succeeds but deployment fails

Consider this illustrative result:

| Where you look | Certificate lifetime remaining | What it tells you |
|---|---|---|
| Certificate file on the origin | 60 days | A newer certificate exists locally |
| Public hostname | 5 days | The endpoint you reached still serves an older certificate |

Renewing the local file again will not fix the gap between those rows. Check which component terminates TLS and which certificate it has loaded.

For an Nginx deployment using certificate files, inspect the configured certificate path and the reload step. Nginx documents that [configuration changes require a reload or restart](https://nginx.org/en/docs/beginners_guide.html#control), and a failed reload can leave the previous configuration running. Containers also need access to the renewed files; updating a path on the host does not help a container reading a different copy.

With Certbot, there is another detail worth checking: `certbot renew` returns success when nothing needs renewing too. Its exit status alone does not prove that it issued a certificate. A `--deploy-hook` runs after a successful renewal and is the place for deployment work your installer does not already handle. The [Certbot renewal documentation](https://eff-certbot.readthedocs.io/en/stable/using.html#renewing-certificates) explains that distinction.

After fixing deployment, connect to the public hostname again. The served certificate's expiry should move forward.

## Set alerts around your renewal schedule

Choose thresholds that leave time to repair a failed renewal. Avoid copying someone else's day counts without checking when your certificates normally renew.

For example, if your automation normally renews with roughly 30 days remaining, a warning below 21 days leaves room for normal retries while still giving you time to act. A critical threshold below 7 days creates a more urgent deadline. These are example settings, not a rule for every certificate. A certificate valid for only a few days needs much smaller thresholds and more frequent observation.

In Uptimepage:

1. Run the SSL checker and select **monitor this certificate** beside the result. It carries the hostname into setup. Check the port if you used something other than 443.
2. Set the warning and critical day counts for your renewal schedule. The warning count must be higher than the critical count.
3. Choose the check interval and regions. The TLS monitor permits intervals of one hour or longer; the form starts at twice a day.
4. Attach a notification channel that reaches the person responsible for renewal. Save the monitor and inspect its first result.

The [TLS monitor reference](/docs/monitor-types#tls-certificate) describes the settings. Warning marks the monitor degraded; critical marks it down. Give that distinction an operational meaning: who investigates a warning, and who gets interrupted when the remaining time becomes critical?

## Keep an HTTPS check beside the expiry check

A certificate can have weeks left and still fail for your customers. It may cover the wrong hostname, or the server may send a [chain the client cannot validate](/blog/what-is-an-ssl-certificate-chain), usually because the intermediate certificate is missing.

Uptimepage's TLS expiry monitor deliberately accepts the presented chain so it can read the date even from an expired or self-signed certificate. Its day-count result is not a trust verdict. The [TLS API reference](/docs/api#tls-certificate-expiry) documents this behavior.

Keep an HTTP monitor on the HTTPS URL with TLS verification enabled. It catches certificate validation failures as they happen; the expiry monitor gives advance warning about the date. An HTTPS check that validates certificates can detect expiry once it breaks the connection. It does not necessarily warn you days beforehand.

To see both views for one hostname before you set the monitors up, run it through the [website security checker](/tools/website-security-checker). It reads the certificate dates, makes a validated HTTPS request, follows the redirect from plain HTTP, and reports whether the headers keep browsers on HTTPS afterwards.

If a CDN terminates TLS, a public-hostname check reads the CDN's certificate. The origin can have a separate certificate and renewal process. Monitor each TLS endpoint you depend on from somewhere that can reach it, using the expected server name. A healthy edge certificate does not establish that the origin certificate is healthy.

## Test renewal and notification separately

On a host you administer with Certbot, this tests renewal using its normal Let's Encrypt staging configuration:

```bash
sudo certbot renew --dry-run
```

Check your Certbot configuration before running it: a custom ACME server can change the behavior. A dry run can invoke pre/post hooks and temporarily reload a web server; deploy hooks do not run by default. Certbot documents [`--run-deploy-hooks`](https://eff-certbot.readthedocs.io/en/stable/using.html) for testing those too. Review what your hooks do before enabling them.

A successful dry run still does not prove that an expiry alert will reach your team. Test that path on a disposable monitor watching a hostname you control. Keep it off your public status page and route it to a test notification channel. Temporarily set its thresholds above the observed days remaining, wait for the configured alert conditions, and confirm delivery. Restore the settings afterward and check recovery too.

Do not wait for an email from the certificate authority as your fallback. Let's Encrypt [ended certificate-expiration notification emails on June 4, 2025](https://letsencrypt.org/2025/06/26/expiration-notification-service-has-ended).

## Check the other expiry date too

Your domain registration has a separate renewal process. A fresh TLS certificate cannot protect you from a failed domain renewal; [an expired domain can even lead a status-code check to a parking page](/blog/domain-expired-but-site-still-up). Put a domain expiry monitor beside the TLS check.

For the broader monitoring setup, see [what to watch beyond your homepage](/blog/do-i-need-an-uptime-monitor). If the renewal job itself stops running, the same missing-run problem appears in [why cron jobs fail silently](/blog/cron-jobs-fail-silently).

Start with one hostname in the [SSL checker](/tools/ssl-certificate-checker). Read what it serves today, create its monitor, and test where the alert lands. That is a concrete job you can finish before closing the certificate ticket.
