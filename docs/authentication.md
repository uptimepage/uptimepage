# Authentication

uptimepage ships with an in-binary auth stack: GitHub, Google, Microsoft and
GitLab OAuth for the operator UI (the hosted service currently offers GitHub
and Google; the other two are for deployments that configure them), passkeys,
opaque per-user API tokens for the REST surface, and
magic-link sign-in (enabled by default) for users without an OAuth identity. The
binary always runs as multi-tenant SaaS — single-tenant deployments are
just SaaS with one signed-up user; see [Multi-tenancy](multi-tenancy.md)
for the full model.

## Concepts

- **User.** A row in `users`, keyed by id. Email is CITEXT. A user can
  belong to multiple orgs.
- **Session.** A 32-byte random id (43 base64url chars) stored in a
  `HttpOnly; Secure; SameSite=Lax` cookie, default `_sm_session`. Backed
  by a `sessions` row with idle + absolute timeouts.
- **API token.** An opaque bearer token (`sm_live_…`) presented in the
  `Authorization: Bearer …` header. Stored as an argon2id hash plus a
  16-char prefix for indexed lookup. Returned **once** at create time and
  never again.
- **Org.** Container for the user-visible data (targets, incidents,
  maintenance, …). Memberships carry a role: `Owner`, `Member`.
- **Invitation.** A pending row in `invitations` carrying an argon2id
  hash of a single-use token sent to a prospective member's email.
- **Magic-link token.** A single-use row in `magic_link_tokens`
  (`auth.magic_link.expiry_minutes`, default 15). Enabled by default;
  gated by `auth.enabled_methods`.

## Flows

### OAuth sign-in (GitHub, Google, Microsoft, GitLab)

Every provider shares one callback runner; only the upstream identity
fetch differs. The callback is split into three strict phases:

1. **Phase A** — `DELETE … RETURNING` consumes the `oauth_states` row in
   one statement (provider-bound: a state minted for one provider cannot
   complete another's callback). No upstream call has happened yet, so
   the DB connection is released before any HTTP.
2. **Phase B** — exchange `code` for an access token, then fetch the
   profile: GitHub `/user` + `/user/emails` (verified primary only),
   Google OIDC userinfo (email accepted only with `email_verified`),
   Microsoft's id_token claims (see [Microsoft email trust](#microsoft-email-trust)),
   GitLab's id_token claims (see [GitLab instances](#gitlab-instances)).
   No DB connection is held.
3. **Phase C** — a fresh transaction materialises the user + identity,
   links a new provider to an existing account on verified-email match
   (a soft-deleted account matches, and stays deleted), auto-creates a
   signup org if this is a new sign-up, and commits. The user's default
   org (oldest active membership) is resolved after commit for the
   session row. See [Sign-in methods](#sign-in-methods).

After commit, the previous session cookie (if any) is destroyed for
session-fixation defence, a fresh session row is INSERTed, the cookie is
set, and the user is redirected. A sign-in on an account scheduled for
deletion redirects to `/account/restore` and nothing else: signing in
proves who is asking, and cancelling the deletion is a separate,
deliberate `POST /api/v1/me/restore`. Failure modes:

- Invalid or expired state → 400 `INVALID_STATE`, logged to
  `login_attempts`.
- User denied consent / provider sent no code → redirect back to
  `/login`, logged with `failure_reason = "oauth_denied"` (or
  `"missing_code"`).
- Upstream failure → 500, logged with
  `failure_reason = "oauth_upstream_failed"` (rows from before 2026-06
  carry the old `"github_upstream_failed"`).
- Disabled (`enabled_methods`) or incompletely configured provider →
  404 `AUTH_METHOD_UNAVAILABLE` on both start and callback; the
  listed-but-misconfigured case logs a warning.

### Sign-in methods

Every linked provider account is a credential that opens the Uptimepage
account on its own. Signing in with a provider whose attested email
already belongs to an account links it there rather than creating a
second account — someone who signed up with GitHub can sign in with
Google later and land where they expect.

That only holds because the email is *attested*, never merely claimed:
GitHub's verified primary, Google's `email_verified`, Microsoft's
`xms_edov` (see [Microsoft email trust](#microsoft-email-trust)),
GitLab's `email_verified` from the pinned issuer. A provider that cannot
attest gets `NO_VERIFIED_EMAIL` and links nothing.

And it is never silent. A link that did not exist before sends the
account mail naming the provider, and `/settings/account` lists every
credential with when it was added, when it was last used, and a way to
remove it. Removal sends its own mail. Both write a `credential_events`
row — mail is best-effort, so the record does not depend on it. The row
outlives the identity, carries the same salted `ip_hash` /
`user_agent_hash` as `login_attempts`, and its `origin` says how the
credential arrived: `signup` (the one the account was created with),
`email_match` (a provider let itself in on an attested address and
nobody clicked add), or `session` (the account holder added it while
signed in) — the first question any investigation asks. It rides the
GDPR export as `credential_changes`. Both tables are keyed on the user,
so a hard purge after account deletion takes the credential trail with
the account; `login_attempts` keeps its rows with a null user.

The one exception is an account inside its deletion grace window: a
provider can still resolve to the tombstone by attested address, and the
link is recorded, but no mail is sent — a tombstoned account is signed
out everywhere else and its address is not ours to write to. The
`credential_events` row is what answers for it on restore.

Removing a method also revokes the account's other sessions, keeping
only the one that asked. The credential is not what an attacker holds —
the session it opened is, and it would otherwise outlive the removal. An address that a provider attests
wrongly therefore costs a notification the owner can act on, not a quiet
takeover.

**Adding a method the email cannot reach.** A work GitLab account under
a different address never matches, so it is added deliberately:
`POST /auth/{provider}/link` mints a dance carrying
`oauth_states.link_user_id`. The callback compares that against the live
session **before** the token exchange and bounces to `/login` on a
mismatch — the state alone must not authorise a link, because a leaked
one would otherwise let whoever holds it attach their own provider
account to somebody else's. It is a POST so the CSRF guard covers it.

**Removing one.** `DELETE /api/v1/me/sign-in-methods/{provider}`
(optionally `?provider_user_id=…` when the same vendor is linked twice).
It refuses only when it would leave the account with no way in at all —
which, on a deployment offering magic-link sign-in, it never does, so a
user whose only provider is compromised can drop it without first
granting another provider a credential on the account they are securing.

### Microsoft email trust

Entra's `email` claim is a directory attribute a tenant admin can set to
any string, including an address they do not own. Phase C links a new
provider onto an existing account by verified email, so trusting that
claim blindly would hand any admin a takeover path. Microsoft is
therefore read from the id_token the token endpoint returns, and an
address only becomes a verified email when one of these holds:

- `xms_edov` is `true` — the tenant proved it owns the address's domain.
- The token's `tid` is the personal-account tenant
  (`9188040d-6c67-4c5b-b112-36a304b66dad`) **and** the domain is one
  Microsoft owns (`outlook.com`, `hotmail.com`, `live.com`, `msn.com`,
  `passport.com`, `windowslive.com`). Country variants such as
  `hotmail.co.uk` are not on the list: matching them needs a prefix rule
  that `hotmail.attacker.test` would satisfy too.

Anything else still signs in against an already-linked identity but can
neither link to an existing account nor create a new one — that callback
bounces back to `/login` and writes a `no_verified_email` row to
`login_attempts`, rather than erroring.

`xms_edov` is an **optional claim**: add both `email` and `xms_edov` to
the app registration's ID-token optional claims, or no work account can
sign up. The callback logs a warning naming the claim whenever it sees an
unattested address. A tenant that emits the claim in an unexpected shape
costs that one link, never the whole sign-in.

The identity key is `{tid}/{oid}`, falling back to `sub`. `sub` alone is
pairwise per app, so the pair survives a change of app registration and
keeps a guest in another tenant distinct from the same person at home.

The signature on the id_token is not verified: the bytes came straight
back from Microsoft's token endpoint over TLS, so a JWKS fetch would only
re-prove what the channel already proved. Nothing accepts a token that
arrived any other way.

### GitLab instances

GitLab speaks OIDC, so the identity rides the id_token and no profile call
follows. `auth.gitlab.base_url` names the instance the OAuth application is
registered on: `https://gitlab.com`, or a self-managed instance's own https
origin. Plain HTTP is refused at boot — the client secret rides that origin in
a POST body.

An instance on a private address (RFC1918, loopback) works without touching
`security.allow_private_targets`: the OAuth token exchange runs on a client
that skips the SSRF guard, because the origin comes from operator config
rather than from a request. That flag stays off, so user-created monitors and
webhooks still cannot reach the private range.

A GitLab user id is unique only within the instance that issued it, so the
identity key is `{iss}/{sub}` rather than `sub` alone. The token's `iss` is
compared against the configured base URL before anything else and a mismatch
fails the callback: taken on trust, a self-managed instance could mint ids
that collide with gitlab.com's and land its users on someone else's account.
Because the issuer is half the key, changing `base_url` after sign-ups orphans
every account created against the old one.

`email` becomes a verified email only when `email_verified` is `true`. On
gitlab.com that always holds; a self-managed instance can be configured to
skip confirmation, and those addresses can neither link to an existing
account nor create a new one. The callback logs a warning and bounces to
`/login` with a `no_verified_email` row, same as the Microsoft path.

Register the application under **User settings → Applications** (or the group
/ instance admin equivalent) with the `openid`, `email` and `profile` scopes,
the redirect URI set to `<public base>/auth/gitlab/callback`, and **Confidential**
left checked — the flow authenticates with the client secret, not PKCE.

The id_token signature is unverified for the same reason it is on the
Microsoft path: the bytes came straight back from the token endpoint over TLS.

### Passkeys

A passkey is the one credential this deployment mints itself. Every other way
in belongs to a vendor who can refuse to register the app or lock the owner
out, which is not hypothetical: the Microsoft and GitLab buttons are dark on
the hosted service for exactly that reason.

Sign-in is **discoverable**, so the page never asks for an address first.
Asking would answer whether that address has a passkey, which is the
enumeration oracle the magic-link path was built to avoid. The assertion
carries the account id as its user handle, and nothing is trusted from it
until the signature verifies against a credential that account actually holds.

**A passkey cannot create an account.** It carries no email address, so it is
only ever added to an account that already exists. A new account starts at an
OAuth provider, or at a magic link where `auth.open_signup` is on.

Because sign-in is discoverable, the credential has to live on the
authenticator itself. A browser that reports back that it saved a server-side
credential instead is refused with `400 PASSKEY_NOT_DISCOVERABLE`, since
storing it would put a credential in the count that keeps the last way in and
could never answer a sign-in. The report is an unsigned hint, so only an
explicit "no" is refused.

**User verification is required**, so the device asks for a biometric or PIN
every time rather than mere presence.

Signing in with a passkey starts from the button on the sign-in page, and the
challenge is minted when it is pressed, so a visit that only passes through
writes no ceremony row. There is no offer in the email field's autofill list:
the browser would surface it on every view of the page, and nothing tells us
whether the visitor holds a passkey to surface.

**Passkeys live in a browser's credential store, not on a machine.** Chrome
keeps its own; Safari uses iCloud Keychain. So a passkey added in one browser
does not appear in another on the same computer, which is not a fault in the
deployment and nothing a relying party can change. The second browser offers a
QR code or a security key instead, both of which reach a credential held
elsewhere. Someone who wants one passkey everywhere needs a manager that spans
browsers; everyone else adds one per browser, which is part of why the cap is
ten rather than one.

The relying-party id is the **host of `auth.public_base_url`**, derived rather
than configured so it cannot drift from the origin the browser reports. An
authenticator binds every credential it mints to that exact string, so
**changing that host retires every passkey on the deployment**. Nothing can
migrate them. Each row records the `rp_id` it was made for, the account page
labels the ones that no longer answer, and startup logs a warning naming how
many there are. Treat the app's hostname as permanent once passkeys are in use.

WebAuthn needs a secure context, so passkeys work over https or on
`localhost` and nowhere else. A self-hosted deployment served over plain http
on a real hostname will simply never show the button.

At most ten per account (`MAX_PER_USER`), counted under a per-user advisory
lock so two ceremonies finishing at once cannot both pass the check. A flat
limit rather than a plan tier: a passkey is not a metered resource, and the
only job is to stop one account writing rows without end.

Ceremony state lives in `webauthn_states` for five minutes and is deleted as
it is read, so a replayed answer finds nothing. A registration is bound to the
session that started it and is refused if another account answers it. An
abandoned ceremony is swept on a five-minute loop.

The signature counter is checked but **not enforced**: a counter that was
advancing and stops is the spec's cloned-authenticator signal, so it raises
`uptimepage_passkey_counter_stalled_total` and a warning rather than refusing
the sign-in. Refusing would lock someone out over a firmware quirk, and the
assertion itself is already cryptographically sound. Synced passkeys carry no
counter at all, so this only ever fires for hardware keys.

Attestation is off. A passkey is trusted because the owner holds it, not
because a certificate authority vouches for the authenticator model.

Adding or removing one sends the same mail, writes the same `credential_events`
row and increments the same counter as a linked provider, and removal revokes
the account's other sessions for the same reason.

### API token auth

Bearer tokens skip the cookie path entirely. The middleware checks the
`Authorization: Bearer …` header against the `api_tokens` table via the
indexed `token_prefix` (first 16 chars of the raw token), then
argon2-verifies the survivor. `last_used_at` is updated through the same
60-second debounce as session cookies.

CSRF protection does not apply: cross-origin browsers don't auto-attach
the `Authorization` header, so there is no forgery surface.

To manage resources with a token as code, see [Terraform](terraform.md). To let an LLM client query and act on an org with a token, see the [MCP server](mcp.md).

#### Scopes

Every token carries a set of `resource:action` scopes. A request is rejected with `403 INSUFFICIENT_SCOPE` unless the token holds the scope its endpoint requires. `full_access` is a superset that grants all of them; unknown scope strings are ignored (forward-compatible).

| Resource | `read` | `write` | `delete` | `execute` |
|---|---|---|---|---|
| `targets` | list / get / results / uptime / latency / incident history | create / update / bulk | delete, bulk-delete | run a check now, test-probe a config |
| `channels` | list / get | create / update | delete | send a test notification |
| `incidents` | incident list / detail / delivery log / metrics / postmortem (the public timeline needs no token) | narrate / post update, acknowledge / resolve | — | — |
| `maintenance` | list / get | create / update | delete | — |
| `status_page` | read settings | update settings, upload logo | remove logo | — |
| `variables` | list / get (secret values redacted) | create / rotate | delete (blocked while referenced) | — |
| `oncall` | view escalation policies and on-call schedules | manage them (owner-only) | — | — |

`write` implies `read` for the same resource. `delete` and `execute` are **independent** — they are *not* granted by `write`, so a config-management token (`*:write`) can change resources but cannot destroy them or trigger side effects. Grant `delete`/`execute` explicitly when you need them.

#### Org binding

A token is user-scoped, so each request names an org via the `X-Uptimepage-Org: <slug>` header. A token can additionally be **bound** to one org at creation:

- **Bound** — the header is optional; if sent it must name the bound org, else `403 ORG_HEADER_MISMATCH`. The token can never act on the user's other orgs.
- **Unbound** — the header is required (`400 ORG_REQUIRED` if absent). A malformed/unknown slug is `400 ORG_HEADER_INVALID` on either kind.

#### Expiry

A token may carry an expiry (1–365 days); an expired token authenticates as invalid. Tokens without an expiry never lapse — prefer a bounded lifetime.

#### Audience

A token the [MCP server](mcp.md) mints through OAuth is stamped with the MCP endpoint as its audience (RFC 8707). The REST API refuses it with `401 TOKEN_AUDIENCE`, whatever scopes it carries: an MCP consent grants an LLM client the MCP tools, not the API. Tokens minted in the UI carry no audience and work on both.

#### Managing tokens

Token management — create, list, rename, revoke — is **browser-session only**: these endpoints read the session cookie and reject bearer tokens, so a token can never mint another token (which would escape its own scopes) or reach account/org administration. Mint tokens in the UI at **Settings → API tokens** (a verified email is required).

### Magic-link sign-in (gated)

Available only when `auth.enabled_methods` contains `"magic_link"`:

1. `POST /auth/magic-link/request {email}` — generates a 32-byte token,
   hashes it, INSERTs into `magic_link_tokens` with a 15-minute expiry,
   and emails the verify URL via the configured `EmailSender`.
   Anti-enumeration: the response is identical for known, unknown, and
   malformed emails — `{"sent": true}`.
2. `GET /auth/magic-link/verify?token=…` — atomically marks the row
   `used_at = now()`, destroys any pre-login session, mints a new
   session, auto-accepts a carried invitation, and redirects by
   priority: `/account/restore` (the account is scheduled for deletion;
   signing in does not cancel that, so the choice gets its own page) →
   `/?joined=<slug>` → `/?invite=missed` (carried invitation failed to
   redeem) → carried `redirect_after` → `/`. An invalid, used, or
   expired token renders an HTML "link expired" page with status 410 —
   one indistinguishable state, no JSON error envelope.

The schema and email template ship in v1 even when the flow is gated, so
flipping the config doesn't require a migration.

### Invitations

Owners issue invitations to email addresses. The recipient gets emailed
accept/decline links embedding the raw token (single-use, hashed at rest
with the same argon2id parameters as API tokens).

- `GET /invitations/accept?token=…` — with a session, redeems right
  there (clicking the emailed link is the consent; email must match);
  without one, bounces to `/login?invitation=…` and every sign-in
  method carries the invitation through and auto-accepts after login.
  The session's active org rotates to the joined org and the dashboard
  shows a "welcome to <org>" banner (`/?joined=<slug>`). A carried
  invitation that can't be redeemed (mismatched email, seat race,
  revoked) never breaks the login — the dashboard shows a generic
  "invitation couldn't be applied" banner instead.
- `GET /invitations/decline?token=…` — render-only confirm page (mail
  scanners prefetch links, so the GET never mutates); its button POSTs
  the decline.
- A magic link requested for an **unknown** email that carries a valid
  invitation for that same address bootstraps the account at verify
  time: user created (verified, consent stamped, no personal org) and
  joined directly into the inviter's org.
- The same mail carries a **code** for a reader whose inbox is on another
  device: six Crockford base32 characters, entered back in the tab that
  asked. It is bound to that browser by a `_sm_ml_code` cookie, so a code
  guessed from anywhere else is refused whatever it says. **One attempt** —
  a well-formed miss retires the code and leaves the link in the same mail
  redeemable, which is what makes a single attempt survivable. Malformed
  input, whitespace and separators spend nothing, since a paste accident is
  not a guess.
- **Only the newest credential is live.** Sending a new mail retires every
  earlier link and code for that address. Retirement is recorded separately
  from redemption, so "did anyone actually click this?" stays answerable,
  and the trail records which of the two opened the session.
- With `auth.open_signup` on (the default), a magic link requested for
  an **unknown** email and carrying no invitation opens an account with
  an org of its own. With it off the deployment is invite-only and that
  case still gets the indistinguishable invalid-link page. Either way
  the link is sent to every address, so nothing about the answer says
  whether an account existed.
- A seat-race loser's invitation is un-consumed (`accepted_at`
  reverted), so "try again once a seat frees up" stays true.

## Endpoints

| Method | Path | Auth | Description |
|---|---|---|---|
| `GET`  | `/login`                       | none    | Login page (HTML) |
| `GET`  | `/auth/github/login`           | none    | Initiate GitHub OAuth |
| `GET`  | `/auth/github/callback`        | none    | Handle OAuth callback |
| `POST` | `/auth/logout`                 | session | Destroy current session |
| `POST` | `/auth/logout-all`             | session | Destroy all sessions for current user |
| `POST` | `/auth/magic-link/request`     | none    | Request magic link + code (gated) |
| `POST` | `/auth/magic-link/code`        | none    | Redeem the emailed code in the browser that asked (gated) |
| `GET`  | `/auth/magic-link/verify`      | none    | Verify magic-link token (gated) |
| `GET`  | `/auth/google/login`           | none    | Initiate Google OAuth |
| `GET`  | `/auth/google/callback`        | none    | Handle Google OAuth callback |
| `GET`  | `/auth/microsoft/login`        | none    | Initiate Microsoft OAuth |
| `GET`  | `/auth/microsoft/callback`     | none    | Handle Microsoft OAuth callback |
| `GET`  | `/auth/gitlab/login`           | none    | Initiate GitLab OAuth |
| `GET`  | `/auth/gitlab/callback`        | none    | Handle GitLab OAuth callback |
| `POST` | `/auth/{provider}/link`        | session | Add that provider to the signed-in account |
| `POST` | `/auth/passkey/login/start`    | none    | Begin a passkey sign-in, carrying any `invitation` / `redirect_after` (gated) |
| `POST` | `/auth/passkey/login/finish`   | none    | Complete it and open a session (gated) |
| `POST` | `/auth/passkey/register/start` | session | Begin adding a passkey (gated) |
| `POST` | `/auth/passkey/register/finish`| session | Store the credential just minted (gated) |
| `GET`  | `/invitations/accept`          | optional session | Emailed accept link (HTML; redeems with session, else login bounce) |
| `GET`  | `/invitations/decline`         | none    | Emailed decline link (HTML confirm page; POST does the decline) |
| `GET`  | `/api/v1/me`                   | session/token | Current user info |
| `DELETE` | `/api/v1/me`                 | session | Delete account (soft, 30-day grace) |
| `POST` | `/api/v1/me/restore`           | session for a soft-deleted account | Cancel a pending account deletion |
| `DELETE` | `/api/v1/me/sign-in-methods/{provider}` | session | Remove a linked provider |
| `DELETE` | `/api/v1/me/passkeys/{id}`   | session | Remove one passkey |
| `GET`  | `/api/v1/me/sessions`          | session | List active sessions |
| `DELETE` | `/api/v1/me/sessions/{id}`   | session | Revoke a session |
| `GET`  | `/api/v1/me/api-tokens`        | session | List tokens (prefix only) |
| `POST` | `/api/v1/me/api-tokens`        | session | Create token (returned once) |
| `PATCH`| `/api/v1/me/api-tokens/{id}`   | session | Rename token |
| `DELETE`| `/api/v1/me/api-tokens/{id}`  | session | Revoke token |
| `POST` | `/api/v1/orgs/{org_id}/invitations` | session, owner | Issue invitation |
| `GET`  | `/api/v1/orgs/{org_id}/invitations` | session, owner | List pending |
| `DELETE`| `/api/v1/orgs/{org_id}/invitations/{id}` | session, owner | Revoke |
| `POST` | `/api/v1/invitations/accept`   | session | Accept (token in body) |
| `POST` | `/api/v1/invitations/decline`  | none    | Decline (token in body) |

## Security model

- **CSRF.** State-changing cookie-authenticated requests must carry
  `X-Requested-With: uptimepage`. Bearer requests skip. The header
  is comparison-checked in constant time via `subtle::ConstantTimeEq`.
- **Session fixation.** Both the OAuth callback and the magic-link
  verify endpoint destroy any pre-existing session bound to the browser
  before minting the new one.
- **Hashed PII.** IP addresses and User-Agent strings in
  `sessions`, `login_attempts`, and `magic_link_tokens` are stored as
  HMAC-SHA256(salt, value) — the salt lives in
  `auth.fingerprint_salt` / `auth_salt_history`. Rotating the salt
  refuses to boot without an explicit override env var to make
  audit-log breakage loud.
- **Argon2id parameters.** Default parameters from the `argon2` crate
  (`Argon2::default()`). Tokens carry 256 bits of entropy, so the
  factor of safety is in the token, not the params.
- **Anti-enumeration.** Magic-link request and invitation lookup return
  the same response whether the underlying row exists.
- **Per-email send throttle.** `auth.magic_link.rate_limit_seconds`
  (default 60) caps a single address to one outgoing email per window
  regardless of source IP. The check runs inside the spawned send
  task so it never branches the response path. Concurrent requests for
  the same address all still INSERT (preserving anti-enum work) but
  only the earliest row in the window — ordered by `(created_at, id)`
  — actually mails the user. Set to `0` to disable.

## Background workers

- `oauth_state_cleanup` — `DELETE FROM oauth_states WHERE expires_at <
  now()` every 10 minutes.
- `invitations::purge_old` — daily cleanup of accepted/declined/expired
  rows older than a configurable window.
- `magic_link_cleanup` — every 6 hours when `magic_link` is in
  `auth.enabled_methods`. Drops rows seven days after they expire, not
  as they expire: until then `redeemed_via` and `superseded_at` still
  answer "did anyone open this, and by which half of the mail?". When
  the method is disabled the routes 404 and no rows are ever inserted,
  so the ticker stays asleep.

## Sign-in audit

Every authentication attempt — success or failure — writes a row to
`login_attempts`:

- `method` ∈ `'github_oauth' | 'google_oauth' | 'microsoft_oauth' |
  'gitlab_oauth' | 'passkey' | 'api_token' | 'magic_link'`
- `success` boolean
- `failure_reason` text (`'invalid_state'`, `'token_expired'`,
  `'invalid_token'`, …)
- `ip_hash`, `user_agent_hash` for forensic correlation without storing
  raw PII

The "recent activity" panel on the user's settings page reads from this
table.

## Deployment shape

Every authenticated request carries an active org id; data writes scope
through repositories that enforce isolation. The cross-tenant test
suite confirms a user can't read or mutate another org's rows via slug
URL or session token. Single-tenant deployments work the same way —
they just have one user and one org. See
[docs/multi-tenancy.md](multi-tenancy.md) for the data model and
isolation guarantees.
