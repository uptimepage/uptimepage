+++
# Written by Artem Senenko. Attribution required under AGPL-3.0 sections 7(b) and 7(c); see "Blog posts" in LICENSING.md.
title = "How to debug a redirect loop, one hop at a time"
date = "2026-09-05"
slug = "how-to-debug-redirect-loops"
excerpt = "Trace ERR_TOO_MANY_REDIRECTS with curl and response headers. Find conflicting HTTPS, hostname, and login rules, then monitor the path after fixing it."
tags = ["http", "redirects", "debugging", "monitoring", "cloudflare"]
draft = false
+++

> **TL;DR**
>
> To debug a redirect loop, request the failing URL and record each response status and `Location` header. Find where the chain sends you back, then identify the application or proxy rule responsible. Test with GET, keep redirects bounded, and compare with the browser if the failure depends on login state or cached behavior.

`ERR_TOO_MANY_REDIRECTS` tells you the browser gave up. It does not tell you which component sent the wrong response. Your application can be running normally while a CDN rule and a framework setting keep passing the request between them.

Start with the URL that fails, including its scheme, path, and query string. Testing the homepage is little help when the loop starts at `/account`.

## 1. Capture the first response

For a public URL, open the [HTTP header and redirect checker](/tools/http-header-checker). It sends GET requests, lists each hop's status and destination, and flags a return to a URL it already visited. It stops after at most ten requests. It does not carry your browser's session cookies or execute JavaScript.

From a terminal, replace the example URL with yours:

```bash
curl --silent --show-error \
  --max-time 15 \
  --dump-header - \
  --output /dev/null \
  'https://app.example.com/account'
```

This prints the response headers and discards the downloaded body. Without `--location`, curl stops at the first response. Look for a redirect status and `Location`, which names the next destination. These flags are documented in the [curl manual](https://curl.se/docs/manpage.html).

Use GET for this first test. `curl -I` sends HEAD, and an application can route it differently. A clean HEAD response does not establish that the GET request your browser makes is clean too.

## 2. Follow the chain and find the contradiction

Add redirect following with an explicit limit:

```bash
curl --silent --show-error \
  --location --max-redirs 10 \
  --proto-redir '=http,https' \
  --max-time 15 \
  --dump-header - \
  --output /dev/null \
  'https://app.example.com/account'
```

The [curl redirect guide](https://everything.curl.dev/http/redirects.html) explains how `--location` follows HTTP redirects. Here, `--max-redirs` limits redirects, while `--max-time` bounds the transfer. If curl exhausts the redirect limit, it exits with code 47. That proves the chain exceeded your limit, not necessarily that a URL repeated.

Write the requested URL beside each response. For example, this illustrative chain contains conflicting trailing-slash rules:

| Request | Status | Location |
|---|---|---|
| `https://app.example.com/account` | `301` | `/account/` |
| `https://app.example.com/account/` | `302` | `/account` |
| `https://app.example.com/account` | `301` | `/account/` |

The first rule requires the slash; the second removes it. Changing `301` to `302` will not resolve that disagreement. Choose the intended URL and fix the rule that sends the request away from it.

A relative `Location` such as `/account/` uses the current request's origin. Compare complete destinations, including the scheme and query string. A chain that keeps appending `next=` parameters may exhaust the limit without ever repeating an identical URL.

These patterns narrow the investigation:

| What repeats | Where to look first |
|---|---|
| `example.com` and `www.example.com` | Conflicting canonical-host rules |
| `/account` and `/account/` | Proxy rewrites and framework slash handling |
| The same HTTPS URL | How the proxy connects to the origin; how the app detects HTTPS |
| `/login` and a protected page | Session handling and authentication redirects |

Treat these as clues. Match the request time and path against your edge and application logs to identify which layer emitted the response. A `Server` header alone does not prove where a redirect originated; a proxy can relay one produced upstream.

## 3. Check both sides of the proxy

An HTTPS URL can redirect to itself even when the browser never requests plain HTTP.

With Cloudflare's Flexible mode, the browser connects to Cloudflare over HTTPS, but Cloudflare connects to the origin over HTTP. If the origin redirects that request to HTTPS, the browser follows the same public HTTPS URL. Cloudflare sends another HTTP request to the origin, which produces the same redirect.

Cloudflare documents this [Flexible-mode redirect loop](https://developers.cloudflare.com/ssl/troubleshooting/too-many-redirects/). Configure the origin for HTTPS and use an appropriate encrypted mode, preferably Full (strict) once the origin meets its certificate requirements. Also check for origin rules that redirect HTTPS back to HTTP. The connection mode and the redirect policy need to agree.

An application behind another reverse proxy can make a similar mistake when it sees only the internal HTTP connection. It needs a reliable indication of the original request scheme. Django's [`SECURE_PROXY_SSL_HEADER` documentation](https://docs.djangoproject.com/en/5.2/ref/settings/#secure-proxy-ssl-header) describes this case and its trust requirements.

Do not copy a forwarded-header setting blindly. The proxy must strip or overwrite client-supplied values, and the application must trust the right proxy boundary. Otherwise a caller can claim that an insecure request was secure.

Fix the boundary that misidentifies the request, then repeat the trace from the original public URL.

## When curl works but the browser loops

Keep the browser failure open. In Chrome DevTools, select Network and enable **Preserve log** before navigating again. It [keeps requests across page loads](https://developer.chrome.com/docs/devtools/network/reference#preserve-log). Inspect the document requests, their response headers, and the initiator when a navigation has no HTTP redirect response.

Compare that trace with curl. The browser may send a session cookie, use a cached redirect, or run JavaScript that starts another navigation. A public checker cannot reproduce those conditions just from a URL.

For a login loop, inspect whether the response sets the expected cookie and whether the next request sends it. Check its domain, path, and security attributes, along with the application's session logs. Keep session tokens and copied authenticated requests private. The [login-monitoring guide](/blog/monitor-the-login-not-the-login-page) covers why a successful request to the login page does not prove anyone can sign in.

HSTS is another difference: a browser can [upgrade HTTP to HTTPS before sending the request](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Strict-Transport-Security). That upgrade is not an HTTP redirect returned by your origin. If the server then redirects HTTPS back to HTTP, investigate that server rule rather than disabling HTTPS protection to make the symptom disappear.

A fresh browser profile can help isolate stored state, but a successful clean session is only another observation. Confirm that the affected session works after the fix too.

## Verify the route, then keep watching it

Repeat the bounded GET trace after changing the responsible rule. Check the original failing URL as well as the intended final URL. A good result reaches the expected page without returning to a previous step. For a login flow, complete the login in the affected browser too.

If the trace stops at a certificate error, you have a TLS failure to investigate before the next HTTP response is available. Read the certificate with the [SSL checker](/tools/ssl-certificate-checker); bypassing validation would hide a failure your users may still encounter.

In the header checker, **monitor this URL** carries the starting URL into Uptimepage's HTTP-monitor setup. Choose redirect behavior for the job: follow redirects when checking that a visitor reaches the public page; disable following and require the expected status when an API endpoint should answer directly. Add a body expectation where a generic `200` could be the wrong page. The [HTTP monitor reference](/docs/monitor-types#http) describes those controls.

For the rest of the setup, see [what to monitor beyond your homepage](/blog/do-i-need-an-uptime-monitor). Keep the redirect trace in the incident notes: which URL failed, which rule sent it back, and what the route returned after the fix. That gives the next person a useful starting point if the loop comes back.
