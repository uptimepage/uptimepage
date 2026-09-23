# Privacy Policy

**Last updated:** 2026-09-23

This Privacy Policy explains how the uptimepage service ("we", "us") collects and processes personal data. It is intended to satisfy our obligations under the EU General Data Protection Regulation (GDPR) and similar laws.

## 1. Data Controller

Artem Senenko, Dimitriou Karatasou 5, 2024 Nicosia, Cyprus, is the data controller for personal data processed via the Service. The controller is a natural person; no company is currently registered.

**Contact:** [hello@uptimepage.dev](mailto:hello@uptimepage.dev), +357 97 425258

**For data-subject requests:** [hello@uptimepage.dev](mailto:hello@uptimepage.dev) (see §10)

We do not have a designated Data Protection Officer as we do not meet the thresholds under GDPR Article 37.

## 2. What Data We Collect

To have an account you must give us an email address and a way to sign in; without them we cannot create one. Everything else is optional: a feature whose data you leave out simply does not work, and nothing else changes.

We collect data in three ways:

**You provide:**
- Email address (via GitHub, Google, Microsoft or GitLab sign-in, or email sign-in)
- Display name (via GitHub, Google, Microsoft or GitLab sign-in)
- Passkey public keys and the name you give each one. The private key is created by your device and never leaves it, so we never receive it
- Organisation names, slugs, branding (display name, about text, logo)
- Target configurations (URLs, intervals, headers, optional credentials)
- Status-page customisation (incident narration, maintenance windows)

**If you buy a paid plan:** Paddle collects your payment details, billing address and tax details directly in its checkout. We never see or store your card number. From Paddle we receive a customer ID, a subscription ID, the plan and billing interval you chose, the subscription status, and the dates of the current period and of payments.

**We generate automatically:**
- Session identifiers (random)
- API tokens (you create; we store hashed)
- Check results (technical metrics: status codes, latencies, error codes)
- Login attempts (success/failure, method, hashed IP, hashed user agent)
- Sign-in method changes (which provider was added or removed, whether you asked for it or it was matched on your verified address, hashed IP, hashed user agent)
- Audit events (organisation membership changes, target changes)
- MCP write actions (which tool ran, what it acted on, and whether it succeeded or was refused)

**We collect via your browser:**
- Session cookie (`_sm_session`) — necessary for authentication
- Two short-lived cookies while an email sign-in is in progress (`_sm_ml_confirm`, `_sm_ml_code`), which bind that sign-in to the browser that started it
- Small functional cookies for your last sign-in method and your display settings. See the [Cookie Policy](/cookies) for the full list
- IP address (hashed before storage; never stored raw)

**Analytics (public marketing and sign-in pages only):** We run self-hosted, cookieless analytics (Umami) on our own EU infrastructure. It records aggregate page views, referrer, browser, operating system, device type, and coarse location (country and region). Visits are grouped by a hash of your IP address and user agent mixed with a secret value that rotates every month, so within one month repeat visits from the same network and browser count as one returning visitor. It sets no cookies, never stores your raw IP, cannot tell us who you are, and cannot follow you to other websites. The data stays on our infrastructure and is never sent to a third party.

On the sign-in page this also records which sign-in method you chose (GitHub, Google, email link, or passkey) and whether signing in succeeded, so we can tell how many people who set out to sign in actually got in. Once you are signed in, no page of the product is tracked: there is no analytics on your dashboard, monitors, incidents, or settings.

We do **not** use third-party analytics services that export your data (no Google Analytics, no Mixpanel, no tracking pixels).

## 3. Why We Process This Data

| Data | Purpose | Lawful basis (GDPR Art. 6) |
|---|---|---|
| Email, display name, OAuth identity, passkey public keys | Provide authentication | Contract |
| Targets, check results | Provide monitoring service | Contract |
| Browser flow runs and failure evidence | Show why a monitored journey broke | Contract |
| Heartbeat pings and the output your job sends with them | Show when a scheduled job ran and why it failed | Contract |
| Sessions, API tokens | Authenticate API requests | Contract |
| Subscription records and billing history | Provide the paid plan you bought, apply its limits, handle cancellations and failed payments | Contract |
| Hashed IP, login attempts | Detect security threats | Legitimate interest |
| Sign-in method changes | Let you see, and challenge, every credential that opens your account | Legitimate interest |
| Audit log | Compliance and accountability | Legitimate interest |
| MCP write actions | Account for changes an AI assistant made on your behalf | Legitimate interest |
| Aggregate analytics (marketing and sign-in pages) | Understand site usage and improve content and sign-in | Legitimate interest |

**Browser flow monitors:** when a flow monitor you configured fails, we keep what the page showed at that moment — the URL the browser ended on, the page title, its visible text, and anything the page logged to the browser console. Because the flow signs in, that text can come from a page behind your own login. It is stored to explain the failure and for nothing else, it is never put into an alert or notification, and any value the flow typed from a secret variable is removed before it is stored. It is deleted on a shorter clock than the run itself.

**Heartbeat monitors:** if your job POSTs a body to its ping URL, we keep the first few kilobytes of it as that run's output, so a failure can be read without going back to the machine that ran it. Whatever the job prints is what we store, so do not print secrets to it. It is never put into an alert or notification, and it is deleted on a shorter clock than the ping itself.

**MCP connector:** when you connect an AI assistant to our MCP server, it reads your monitoring because you asked it to, and every action that would change something is recorded, whether it succeeded, was refused, or you declined it. The record names the tool, identifies what it acted on, and states the outcome, so it can include a monitor's name and address, the tags and group a retune moved it to, and the names of the channels it alerts. A refused action is recorded too, which means a monitor name your assistant proposed can be kept even though you declined it. What you write for customers is not kept here: an incident's public title and description, the updates you post, and any note on acknowledging or resolving are not part of this record. Read-only calls are not recorded at all. We never receive or store your conversation with the assistant, only the tool calls it makes. The client you connect is one you chose and someone else operates, so the answers it asks for reach whoever runs it; that is your instruction to it, not a transfer we make (see §6).

**Status page subscribers:** if you subscribe to a status page run by one of our customers, that customer decides how your data is used and is its controller; we process it on their behalf. We store your email address or webhook URL, when you confirmed the subscription, and a record of each notification we send you. We use it only to send that page's incident and maintenance updates. Emails go out through Resend. Every email carries an unsubscribe link, and an address that bounces or reports a message as spam is removed. For any other request about your data, contact the organisation that runs the page, or write to us and we will pass it on.

We do not engage in automated decision-making with significant effects on you (no profiling, no scoring).

## 4. How Long We Keep It

| Category | Retention |
|---|---|
| Account data (email, OAuth, passkeys) | Until account deletion |
| Sessions | 90 days maximum |
| API tokens | Until you revoke them |
| Check results (raw per-check detail) | 30 days |
| Check result history (aggregated, hourly) | 13 months |
| Browser flow runs (which steps ran, and how long each took) | 30 days |
| Browser flow failure evidence (page URL, title, visible text, browser console) | 7 days |
| Heartbeat pings (when each signal arrived, its exit status, how long the run took) | 30 days |
| Output posted with a heartbeat ping | 7 days |
| Login attempts | 180 days |
| Sign-in method changes | 180 days |
| Audit log | 2 years |
| MCP write actions (tool, what it acted on, outcome, and the person and token behind it) | 2 years |
| Quota events | 90 days |
| Status page subscriptions (email address or webhook URL) | Until you unsubscribe, your address bounces, or the page owner removes you or deletes the page. Unconfirmed ones are deleted once the confirmation link expires |
| Status page notification records | 30 days |
| Subscription records and billing history (plan changes and why) | Until account deletion |
| Payment provider event IDs (to ignore duplicate deliveries) | 30 days |
| Server access logs | 30 days |
| Application error logs | 30 days |
| Aggregate analytics (marketing and sign-in pages) | Indefinite (aggregate only; no identifiers that single you out) |

Deleted accounts are recoverable for 30 days, after which data is permanently purged.

## 5. Who We Share It With

We use these third-party processors:

| Processor | Purpose | Location | Safeguard |
|---|---|---|---|
| Hetzner Online GmbH | Hosting and DNS | Finland (data centre); Germany (HQ) | DPA in place |
| Resend | Transactional emails | USA | Standard Contractual Clauses |
| GitHub | OAuth authentication | USA | Standard Contractual Clauses |
| Google | OAuth authentication | USA | Standard Contractual Clauses |
| Microsoft | OAuth authentication | USA | Standard Contractual Clauses |
| GitLab | OAuth authentication | USA | Standard Contractual Clauses |
| Fly.io | Probe infrastructure for non-EU check regions | USA | Standard Contractual Clauses |

When you buy a paid plan, Paddle sells it to you as Merchant of Record: Paddle.com Market Limited (United Kingdom), or for some customers its affiliate Paddle.com Inc. (United States). Paddle is a separate controller for the payment, billing and tax data it collects, and handles it under its own privacy policy (<https://www.paddle.com/legal/privacy>). We share with Paddle only what it needs to link a purchase to your account: your account ID and the plan you chose.

We do **not** sell or rent your data. We do not share it for marketing.

We may disclose data:
- To comply with legal obligations (court orders, valid law-enforcement requests)
- To protect rights, property, or safety
- With your explicit consent

## 6. International Transfers

Data is primarily stored in Finland (Hetzner data centre, Helsinki). Resend, GitHub, Google, Microsoft, GitLab and Fly.io are based in the United States; transfers to them are protected by Standard Contractual Clauses adopted by the European Commission. Paddle.com Market Limited is based in the United Kingdom, which the European Commission recognises as providing adequate protection.

An AI assistant you connect over MCP (see §3) reads your monitoring wherever that assistant runs, which may be outside the EU. You choose that client and its operator, and it retrieves only what it asks for on your instruction, so we do not treat it as a processor acting for us. If that matters to you, the connector is optional and revoking it in Settings stops it.

Monitoring checks can run from probe regions outside the EU. Those probes receive the check configuration they need to run (URL, headers, resolved credentials) and produce technical results (status codes, latencies, error text) that are sent back to our EU infrastructure; long-term storage stays in Finland.

**Access by authorities outside the EU.** This paragraph covers all data we hold for you, personal or not. The infrastructure that stores it is in Finland and subject to the law of the European Union and its Member States. The probes outside the EU, and the processors listed in Section 5 that are based outside the EU, are subject to the law of the country they run in, and hold only the data their task needs while they do it. To keep your data from authorities outside the EU where that would conflict with EU law:

- we store it only in the EU, and send a probe outside the EU only what a check in that region needs;
- credentials are encrypted at rest (Section 7);
- we disclose data to an authority only when a legal order binding on us under EU or Cyprus law requires it, we do not comply with a request that conflicts with EU law, and we tell you about any request unless the law forbids it.

## 7. Security

Technical measures include:
- TLS 1.2+ for all connections
- Encrypted credentials at rest (AES-256-GCM for target authentication secrets)
- Hashed passwords and tokens (Argon2id)
- Session cookies marked HttpOnly, Secure, SameSite=Lax
- IP addresses hashed before storage
- Application errors logged without request bodies
- Daily automated security patches via Docker image rebuilds

We will notify affected users without undue delay if we become aware of a personal-data breach affecting your data, and we will notify the competent supervisory authority within 72 hours where required.

## 8. Your Rights

Under GDPR, you have the right to:

- **Access** your personal data (Article 15) — see §10
- **Rectify** inaccurate data (Article 16) — update via /settings
- **Erase** your data (Article 17) — see §10 ("right to be forgotten")
- **Restrict** processing (Article 18) — contact us
- **Data portability** (Article 20) — see §10
- **Object** to processing based on legitimate interest (Article 21) — contact us
- **Withdraw consent** (Article 7(3)) — applies only if we relied on consent for processing
- **Lodge a complaint** with your local supervisory authority. Our supervisory authority is the Office of the Commissioner for Personal Data Protection, Cyprus (<https://www.dataprotection.gov.cy/>)

## 9. Cookies

We use a small number of first-party cookies: one to hold your session identifier, two short-lived ones that bind an email sign-in to the browser that started it, and a few that remember your last sign-in method and your display settings. All of them are either **strictly necessary** for the Service to function or remember a choice you made yourself, so none requires consent.

We do not use analytics, advertising, or third-party tracking cookies.

See our [Cookie Policy](/cookies) for details.

## 10. Data Subject Requests

Two channels — use whichever is convenient:

**Self-service (recommended):**

- **Export:** Visit /settings/account → "Export My Data". You receive a JSON file with the data associated with your account. Activity logs (sign-ins, sign-in method changes, audit events, MCP write actions) cover the last 90 days; ask us by email if you need the full retained history.
- **Deletion:** Visit /settings/account → "Delete My Account". The account is immediately suspended and permanently purged after 30 days.

**Email:** Send a request to [hello@uptimepage.dev](mailto:hello@uptimepage.dev). We will:

- Acknowledge receipt within 7 days
- Verify your identity (typically: email match with account email)
- Fulfil your request within 30 days

You can use the email channel if you are locked out of your account, if you are acting on behalf of someone else (e.g., deceased user), or if you have requirements beyond what the self-service tools provide.

## 11. Children

The Service is not directed to children under 16. We do not knowingly collect data from children under 16. If you become aware that a child has provided us with personal data without parental consent, please contact us so we can delete it.

## 12. Changes

We may update this Policy. Material changes will be announced via email 30 days in advance.

## 13. Contact

[hello@uptimepage.dev](mailto:hello@uptimepage.dev)
