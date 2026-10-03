# Share links

A share link gives anyone a read-only window into a single monitor — no account, no login. Open `/m/{token}` and you get the same detail view a logged-in member sees: live status, uptime, latency and response-time charts, recent check results, and the incident history. Paste the link in a chat channel, drop it in a ticket, or send it to a customer who needs to watch one endpoint without access to your org.

It is distinct from a [status page](per-org-status.md): a status page is a branded, curated, multi-monitor public surface on its own subdomain; a share link is a capability URL to one monitor's full dashboard.

## What a viewer sees

Everything the operator detail view shows, with two deliberate differences:

- **Read-only.** No edit, delete, run-check-now, enable/disable, or navigation to the rest of the app. The page is its own shell with none of the operator chrome.
- **Credentials redacted.** The monitor's check configuration is shown (so a viewer can see what is being checked and how), but any `bearer_token` or `basic_auth` is replaced with `***`. The live credential never reaches the page.

The page auto-refreshes its live region and charts just like the operator view, scoped entirely to the token — it never calls an operator or API URL.

## The token

Minting a link returns a 256-bit random token; the URL is `/m/{token}`. The token *is* the capability — anyone holding it can view the monitor, and forwarding the link grants access. The controls are **revoke** (kill it now) and an optional **expiry** (kill it at a set time); a link with no expiry lives until revoked or the monitor is deleted.

The link is **re-copyable**, like a Google Docs or Dropbox share link: open the Share modal (or the list endpoint) any time to copy the same URL again. Lost the chat you posted it in? Copy it again — you only get a new token when you revoke and create one.

A [status page](public-status.md) can mint one for you: ticking a component's **Detail link** creates a share for that monitor and points the component name at it. Those mints skip the caps below, and carry no label — the list shows which pages use a link, read live from the binding, so a renamed page never leaves a stale label behind. Revoking such a link here removes the link from the page but leaves the component on it. Such a link opens on the page's own host, in the page's branding.

Limits come from the org's plan (`plans` columns, overridable per-org): the free (Standard) plan allows **1 active link per monitor** and shares on at most **2 distinct monitors** per org. Revoke a link to free a slot.

To make that possible the token is stored **encrypted at rest** with the app KEK (the same `Cipher` that protects `basic_auth`/`bearer_token`), so a raw database or backup dump without the key yields nothing usable. The public lookup matches on a separate one-way hash, so a hot link never triggers a decrypt. With no KEK configured the token is stored in plaintext (same fallback as target credentials); if a token was sealed under a key that is later removed, the link shows as un-copyable rather than broken.

A bad, expired, revoked, or deleted-monitor token all return the same `404` — there is no signal that distinguishes "wrong token" from "revoked token", so the surface cannot be enumerated.

## Managing links

From the API (member-level `targets:write`):

```bash
# Mint a link (optionally labelled, optionally expiring)
curl -X POST https://app.uptimepage.dev/api/v1/targets/$ID/shares \
  -H "Authorization: Bearer $UPTIMEPAGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"label":"Slack #ops","expires_at":"2026-12-31T00:00:00Z"}'
# → { "id": "...", "label": "Slack #ops", "token": "…", "view_count": 0, ... }
# build the link as /m/{token}

# List the monitor's live links (each carries its token for re-copy)
curl https://app.uptimepage.dev/api/v1/targets/$ID/shares \
  -H "Authorization: Bearer $UPTIMEPAGE_TOKEN"

# Revoke one
curl -X DELETE https://app.uptimepage.dev/api/v1/targets/$ID/shares/$SHARE_ID \
  -H "Authorization: Bearer $UPTIMEPAGE_TOKEN"
```

The same actions are available from the monitor's detail page in the UI. See the [REST API](api.md#operator-endpoints-share-links) for the endpoint contract.

## Where links live

Share links resolve on the operator app host, not on a per-tenant status subdomain. A monitor's deletion cascades to its shares, so removing a monitor revokes every link to it.

## Abuse

The surface is anonymous, so per-IP request throttling is handled at the reverse proxy. App-side, the live region is served from a short-lived shared cache and every data read inherits the same time-window and page-size limits as the operator API, bounding the cost of any single request.
