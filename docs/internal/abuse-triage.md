# Abuse Triage

> **Operator document, not rendered on the docs site.** Workflow for handling reports that arrive
> at the abuse mailbox. The public-facing rules live in the
> [Abuse Policy](https://uptimepage.dev/abuse-policy); this is how you action them.

## Intake

Abuse reports arrive by email to the published abuse address. Triage each
within the SLA stated in the public Abuse Policy (acknowledge within two
business days; act faster on active harm — phishing, malware, ongoing
attack traffic).

For every report, open a private note with:

- Date received, reporter (or "anonymous")
- The accused org slug / target URL / account email
- Category (see below)
- Evidence (headers, screenshots, the offending status-page text/URL)

## Categories and first action

| Category | Signal | First action |
|---|---|---|
| Phishing / malware page | Status page or target URL hosts a fake login or malware | Suspend the org immediately, then notify |
| Spam relay via email | Org abuses invitation emails to spam | Disable invitations for the org, investigate |
| Attack traffic | Targets configured to hammer a third party | Suspend offending targets; this is the "monitoring as a weapon" case in the Abuse Policy |
| Illegal content | Reported unlawful content on a public page | Assess; suspend if clearly unlawful, otherwise request removal with a deadline |
| Trademark / IP dispute | Brand-owner complaint about a slug or logo | Route to the trademark process in the Abuse Policy; do not auto-suspend |
| Not abuse | Competitor noise, vague complaint | Close with a short reason; keep the note |

## Acting on an org

1. Confirm the violation against the Abuse Policy — cite the
   specific clause in your note.
2. Choose the **minimum** effective action: warn → restrict feature →
   suspend org → terminate. Suspension is reversible; termination after
   the appeal window is not.
3. Apply the action:
   - Suspend: soft-delete the org (enters the standard purge grace window)
     or disable the offending feature.
   - Notify the account owner using the account-suspension template in
     `communication-templates.md`, naming the violated clause and the
     14-day appeal window.
4. Record the action, timestamp and the notification sent in the note.

## Appeals

- Appeals come back to the same mailbox. Re-open the original note.
- Review with fresh eyes; if the appeal is credible, reverse promptly and
  record the reversal.
- If no appeal within 14 days and the violation stands, proceed to
  permanent deletion per the runbook.

## Retention

Keep abuse notes private and minimal. They are operational records, not
marketing data; purge them once the matter is closed and any appeal window
plus a short audit buffer has elapsed. Never paste raw reporter PII into
shared tooling.
