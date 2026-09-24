+++
title = "What is an SSL certificate chain? Leaf, intermediate, root"
date = "2026-09-24"
slug = "what-is-an-ssl-certificate-chain"
excerpt = "How a server certificate links to a trusted root through intermediates, why a missing intermediate breaks some clients and not others, and how to fix it."
tags = ["ssl", "tls", "certificates", "security", "networking"]
draft = false

[[faqs]]
q = "What is an SSL certificate chain?"
a = "It is the list of certificates that links your server's certificate to a root certificate the client already trusts. Each certificate in the list is signed by the next one. A client accepts your certificate only if it can follow those signatures up to a root in its own trust store."

[[faqs]]
q = "Should the server send the root certificate?"
a = "It does not need to. Both TLS 1.2 and TLS 1.3 allow the server to leave the root out, because the client has to have it already for the chain to be trusted. Sending it does no harm, but it adds bytes to every handshake."

[[faqs]]
q = "Why does my site work in the browser but fail in curl or my app?"
a = "Almost always the server is not sending the intermediate certificate. Some clients repair the chain themselves: Firefox preloads known intermediates, and Apple's system verifier fetched the missing one in my test. OpenSSL, Python and Node.js do not, so they fail with an error such as unable to verify the first certificate."

[[faqs]]
q = "Which Certbot file gives the full chain?"
a = "Use fullchain.pem. It holds your server certificate first, followed by the intermediates, and it is the file Nginx expects in ssl_certificate. The file cert.pem holds your certificate alone, and pointing the server at it is a common way to lose the intermediate."

[[faqs]]
q = "How many certificates should a chain have?"
a = "Usually two sent by the server: your certificate and one intermediate. Some chains have two intermediates, for example when a newer root is cross-signed by an older one. A chain of one certificate from a public CA almost always means the intermediate is missing."
+++

> **TL;DR**
>
> An SSL certificate chain is the list of certificates that connects your server's certificate to a root certificate the client already trusts. Your certificate is signed by an intermediate, and the intermediate is signed by a root. The server must send its own certificate and the intermediates. The client supplies the root from its trust store. If the server forgets the intermediate, some clients repair the chain on their own and others fail, which is why a site can work in a browser and break in curl.

## Three certificates, one path

A browser trusts your certificate because someone it already trusts signed it. That someone is almost never the root certificate directly. Certificate authorities keep root keys offline and sign day-to-day certificates with an intermediate instead.

So a normal chain has three levels:

| | Leaf | Intermediate | Root |
|---|---|---|---|
| Also called | server certificate, end-entity | issuing CA, sub-CA | trust anchor |
| Issued to | your hostname, e.g. `app.example.com` | the CA's issuing unit | the CA itself |
| Signed by | the intermediate | the root (or another intermediate) | itself |
| Typical lifetime | a few months | a few years | 20 years or more |
| Who provides it in the handshake | your server | your server | the client's trust store |

The last row matters most. The root ships with the operating system or the browser, and your server has to send everything below it.

## How a client walks the chain

When a TLS handshake reaches the Certificate message, the client gets a list of certificates and tries to build a path from the first one to a root it trusts. [RFC 5280](https://www.rfc-editor.org/rfc/rfc5280), section 6, describes the full algorithm. In plain steps:

1. Take the leaf. Read its **Issuer** field.
2. Find a certificate whose **Subject** matches that issuer, in the list the server sent or in the local trust store.
3. Check that this certificate's public key verifies the signature on the one below it, and that it is allowed to act as a CA.
4. Repeat until the path reaches a root in the trust store.
5. Along the way, check dates on every certificate and check that the leaf covers the hostname the client asked for.

If step 2 finds nothing, the client has a certificate it cannot connect to anything it trusts. That is the error you see as "unable to get local issuer certificate".

## What the server is supposed to send

The TLS specifications are explicit about the order. [RFC 5246](https://www.rfc-editor.org/rfc/rfc5246#section-7.4.2), TLS 1.2, says:

> The sender's certificate MUST come first in the list. Each following certificate MUST directly certify the one preceding it.

It also allows the root to be left out, "under the assumption that the remote end must already possess it in order to validate it in any case."

[RFC 8446](https://www.rfc-editor.org/rfc/rfc8446#section-4.4.2), TLS 1.3, keeps the leaf-first rule but relaxes the rest to a SHOULD. It notes that some servers are "simply configured incorrectly, but these cases can nonetheless be validated properly," and asks clients to be prepared for it. In short: leaf first, then intermediates, root optional.

## See the chain your server sends

`openssl s_client` with `-showcerts` prints every certificate the server sent, in order. Here is the chain for `badssl.com`, trimmed to the subject (`s:`) and issuer (`i:`) lines:

```
$ openssl s_client -connect badssl.com:443 -servername badssl.com -showcerts </dev/null
 0 s:CN=*.badssl.com
   i:C=US, O=Let's Encrypt, CN=YR2
 1 s:C=US, O=Let's Encrypt, CN=YR2
   i:C=US, O=ISRG, CN=Root YR
 2 s:C=US, O=ISRG, CN=Root YR
   i:C=US, O=Internet Security Research Group, CN=ISRG Root X1
    Verify return code: 0 (ok)
```

Read it from the top. Certificate 0 is the leaf for `*.badssl.com`, issued by `YR2`. Certificate 1 is `YR2`, issued by `Root YR`. Certificate 2 is `Root YR`, but issued by `ISRG Root X1`, not by itself. That is a cross-sign: a newer root signed by an older, widely trusted one, so that clients which only know `ISRG Root X1` can still build a path. Every issuer line matches the subject line directly below it, which is what a healthy chain looks like.

The [SSL certificate checker](/tools/ssl-certificate-checker) shows the same count without a terminal: it reports how many certificates the server actually sent.

## The missing intermediate

The most common chain mistake is a server that sends only the leaf. The same `openssl` command against `incomplete-chain.badssl.com`, a test host set up that way on purpose, shows one certificate and a failure:

```
 0 s:CN=*.badssl.com
   i:C=US, O=Let's Encrypt, CN=YR2
verify error:num=20:unable to get local issuer certificate
verify error:num=21:unable to verify the first certificate
    Verify return code: 21 (unable to verify the first certificate)
```

The leaf says it was issued by `YR2`, but `YR2` is not in the handshake and not in the trust store, so the path stops at step 2.

The confusing part is that many clients still connect. I requested that host from several clients on one Mac on 2026-09-24:

| Client | Result |
|---|---|
| OpenSSL 3.6 `s_client` | fails: unable to verify the first certificate |
| Python 3 `urllib` (OpenSSL) | fails: `CERTIFICATE_VERIFY_FAILED` |
| Node.js 26 `https` | fails: `UNABLE_TO_VERIFY_LEAF_SIGNATURE` |
| macOS system curl (Apple TLS) | connects |
| Go 1.26 `net/http` on macOS | connects |

The two that connect hand verification to Apple's system verifier, which repaired the chain. The leaf carries an **Authority Information Access** extension ([RFC 5280, section 4.2.2.1](https://www.rfc-editor.org/rfc/rfc5280#section-4.2.2.1)) with a URL for its issuer, here `http://yr2.i.lencr.org/`, and a client can download the missing intermediate from it. Firefox uses a different fix. Mozilla [preloads every intermediate](https://blog.mozilla.org/security/2020/11/13/preloading-intermediate-ca-certificates-into-firefox/) disclosed to the Common CA Database, which it describes as a fix for "one of the most common server configuration problems: not specifying proper intermediate CA certificates."

That is why the bug hides. You test in a desktop browser, it works, and the failure shows up later in a mobile app, a webhook sender, a CI job or a customer's backend. The Go result is a warning too: the same program on Linux uses Go's own verifier, so a test that passes on your laptop can fail in production.

## Other chain mistakes

- Wrong file on the server. Certbot writes both `cert.pem` and `fullchain.pem`. Pointing the server at `cert.pem` sends the leaf alone.
- Wrong order. The intermediate comes before the leaf, usually because files were concatenated in the wrong order. TLS 1.3 asks clients to cope, and many do. Some older clients and libraries do not.
- The wrong intermediate. The bundle belongs to a different CA or an older issuing certificate, so no issuer line matches.
- An expired cross-sign. A chain can depend on a cross-signing certificate with its own expiry date. Let's Encrypt's chain through DST Root CA X3 [expired on September 30, 2021](https://letsencrypt.org/docs/dst-root-ca-x3-expiration-september-2021/), and older devices that did not trust the newer root stopped connecting to sites whose own certificates were still valid.
- Sending the root. This is allowed and it works, but it makes every handshake larger for no benefit.

## How to fix an incomplete chain

The fix is on the server: send the intermediate after your certificate, in one file.

If you use Certbot, point the server at `fullchain.pem`. The [Certbot documentation](https://eff-certbot.readthedocs.io/en/stable/using.html#where-are-my-certificates) describes it as "All certificates, including server certificate (aka leaf certificate or end-entity certificate). The server certificate is the first one in this file, followed by any intermediates."

```nginx
ssl_certificate     /etc/letsencrypt/live/example.com/fullchain.pem;
ssl_certificate_key /etc/letsencrypt/live/example.com/privkey.pem;
```

If your CA gave you a certificate and a separate bundle, concatenate them with your certificate first. The [Nginx documentation](https://nginx.org/en/docs/http/configuring_https_servers.html#chains) shows the same command:

```bash
cat www.example.com.crt bundle.crt > www.example.com.chained.crt
```

Reload the server, then run the `openssl s_client` command again. You want at least two certificates in the output and `Verify return code: 0 (ok)` at the end. Check every hostname and every load balancer or CDN that terminates TLS, because each one serves its own chain.

## Keep it from coming back

Chains break again on renewal, on a new load balancer, or when someone copies the wrong file during a migration. Two checks cover it:

- An HTTPS check with certificate verification on. Uptimepage's HTTP checks verify the chain against a root store with rustls, which does not fetch missing intermediates. An incomplete chain fails the check the same way it fails curl on Linux, instead of passing the way a browser would.
- A TLS certificate check for the expiry date. It reads the certificate without validating it on purpose, so an expired or broken certificate is still reported with its dates. The guide on [monitoring SSL certificate expiry](/blog/how-to-monitor-ssl-certificate-expiry) covers that side.

For a one-off look, paste the host into the [SSL certificate checker](/tools/ssl-certificate-checker). A chain count of one from a public CA is the thing to look for.

## Common questions

<details class="mk-faq">
<summary>What is an SSL certificate chain?</summary>
<div class="mk-faq__body">

It is the list of certificates that links your server's certificate to a root certificate the client already trusts. Each certificate in the list is signed by the next one. A client accepts your certificate only if it can follow those signatures up to a root in its own trust store.

</div>
</details>

<details class="mk-faq">
<summary>Should the server send the root certificate?</summary>
<div class="mk-faq__body">

It does not need to. Both TLS 1.2 and TLS 1.3 allow the server to leave the root out, because the client has to have it already for the chain to be trusted. Sending it does no harm, but it adds bytes to every handshake.

</div>
</details>

<details class="mk-faq">
<summary>Why does my site work in the browser but fail in curl or my app?</summary>
<div class="mk-faq__body">

Almost always the server is not sending the intermediate certificate. Some clients repair the chain themselves: Firefox preloads known intermediates, and Apple's system verifier fetched the missing one in my test. OpenSSL, Python and Node.js do not, so they fail with an error such as unable to verify the first certificate.

</div>
</details>

<details class="mk-faq">
<summary>Which Certbot file gives the full chain?</summary>
<div class="mk-faq__body">

Use fullchain.pem. It holds your server certificate first, followed by the intermediates, and it is the file Nginx expects in ssl_certificate. The file cert.pem holds your certificate alone, and pointing the server at it is a common way to lose the intermediate.

</div>
</details>

<details class="mk-faq">
<summary>How many certificates should a chain have?</summary>
<div class="mk-faq__body">

Usually two sent by the server: your certificate and one intermediate. Some chains have two intermediates, for example when a newer root is cross-signed by an older one. A chain of one certificate from a public CA almost always means the intermediate is missing.

</div>
</details>

## Sources

- IETF, [RFC 5280: Internet X.509 Public Key Infrastructure Certificate and CRL Profile](https://www.rfc-editor.org/rfc/rfc5280), May 2008. Section 4.2.2.1 (Authority Information Access) and section 6 (path validation).
- IETF, [RFC 5246: The Transport Layer Security (TLS) Protocol Version 1.2](https://www.rfc-editor.org/rfc/rfc5246), August 2008. Section 7.4.2.
- IETF, [RFC 8446: The Transport Layer Security (TLS) Protocol Version 1.3](https://www.rfc-editor.org/rfc/rfc8446), August 2018. Section 4.4.2.
- Mozilla Security Blog, [Preloading Intermediate CA Certificates into Firefox](https://blog.mozilla.org/security/2020/11/13/preloading-intermediate-ca-certificates-into-firefox/), November 2020.
- Let's Encrypt, [DST Root CA X3 Expiration (September 2021)](https://letsencrypt.org/docs/dst-root-ca-x3-expiration-september-2021/).
- EFF, [Certbot User Guide: Where are my certificates?](https://eff-certbot.readthedocs.io/en/stable/using.html#where-are-my-certificates)
- Nginx, [Configuring HTTPS servers: SSL certificate chains](https://nginx.org/en/docs/http/configuring_https_servers.html#chains).
- OpenSSL, [openssl-s_client manual](https://docs.openssl.org/3.0/man1/openssl-s_client/).
- badssl.com, [incomplete-chain.badssl.com](https://incomplete-chain.badssl.com/), a public test host that sends no intermediate.
