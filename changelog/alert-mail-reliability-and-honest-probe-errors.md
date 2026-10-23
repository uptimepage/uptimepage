+++
title = "Alert mail over a fresh connection, and probe errors that say why"
date = "2026-09-21"
summary = "Alert mail goes out over a fresh connection, pages with large headers no longer read as down, probe errors name their cause, new orgs get an email channel."
+++

**Alert emails go out over a fresh connection.** On the morning of 21 September about half of alert emails failed their first send and arrived minutes late on a retry; a handful were dropped after the retries ran out. The sends were reusing a connection to the mail provider that had gone dead. Every alert mail now opens a fresh connection, and a retry can no longer race a first attempt that is still in flight, so an alert is sent once.

**Mail comes from hello@uptimepage.dev.** Alerts, sign-in links and invitations used to come from a no-reply address. Replies to hello@ reach a person.

**Pages with large headers are up again.** A site that answers with more than 16 KiB of response headers, such as finance pages with long preload lists, was recorded as down with a "transport error" while it was serving fine. The probe now accepts up to 256 KiB of headers over HTTP/2 and 1024 header lines over HTTP/1.1. Sorry to everyone who was paged for a healthy site. A test check's header preview now cuts a value over 512 bytes, and a cut body snippet ends in "…".

**Probe errors say what happened.** A request the server dropped after the TLS handshake used to show a bare "transport error". The monitor, the API and the MCP server now say "server reset the connection before responding", "server closed the connection before responding", "server sent an invalid HTTP response", or the HTTP/2 reason the server gave. When the probe itself refuses a response, the error says so instead of blaming the server.

**A new account's org gets its owner's email channel.** A signup through a magic link got an org with no notification channel, so its monitors paged nobody until one was added by hand; GitHub and Google signups already had the owner's address seeded. Every org a sign-in opens now gets it. An org created from the console is unchanged: add a channel to it yourself.

**Self-host.** The `[email]` block is checked at boot: `from_address` must be a bare `user@domain` and is required under `resend`, `provider` must be `resend` or `log`, and a no-reply sender is warned about. A half-configured mail setup now refuses to start instead of failing on every send. See [configuration](/docs/configuration).
