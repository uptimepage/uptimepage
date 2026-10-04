# Terraform

Manage your monitors, notification channels and public status pages as code
with the official Terraform provider,
**[`uptimepage/uptimepage`](https://registry.terraform.io/providers/uptimepage/uptimepage)**.

The Terraform Registry page is the **full reference** — every resource,
attribute, and data source, regenerated from the provider on each release. This
page is a quick start; it links out rather than duplicating that reference.

## Quick start

```hcl
terraform {
  required_providers {
    uptimepage = {
      source = "uptimepage/uptimepage"
    }
  }
}

provider "uptimepage" {
  token = var.uptimepage_token # or set UPTIMEPAGE_TOKEN
  org   = "your-org-slug"      # or set UPTIMEPAGE_ORG
  # endpoint defaults to https://app.uptimepage.dev; set it for a self-hosted instance
}

resource "uptimepage_target" "api" {
  name     = "api prod"
  interval = 60
  check = {
    type = "http"
    http = {
      url             = "https://example.com/healthz"
      expected_status = { kind = "exact", exact = 200 }
    }
  }
}
```

## Credentials

- **Token** — create one at **Settings → API tokens** (`/settings/api-tokens`;
  requires a verified email). Supply it via the `token` attribute or the
  `UPTIMEPAGE_TOKEN` environment variable. The full token is shown **once**.
  Grant the **least scope** the provider needs: `targets:write` +
  `channels:write` covers monitors and channels, and `status_page:write` covers
  status pages and their components (`write` implies `read`, and Terraform only
  deletes during `terraform destroy`). Add the matching `:delete` scopes only if
  you run `destroy`. Status-page writes are **owner-only**, so that token has to
  belong to an org owner; a member's token gets `403` on page changes. For defence in depth, **bind the
  token to the org** you manage so a leak can't reach your other orgs.
- **Org** — API tokens are user-scoped, so every request must name an
  organization. Set `org` (the org **slug**) or `UPTIMEPAGE_ORG`; it is sent as
  the `X-Uptimepage-Org` header. Without it the API returns `400 ORG_REQUIRED`.
  Find your slug from `GET /api/v1/orgs` or your dashboard URL. A token bound to
  an org requires `org` to match it (else `403 ORG_HEADER_MISMATCH`).
- **Endpoint** — defaults to the hosted API at `https://app.uptimepage.dev`. For
  a self-hosted instance, set `endpoint` to your host (the apex marketing domain
  does not serve `/api/v1`).

## Resources & data sources

| Name | Kind | Manages |
|---|---|---|
| `uptimepage_target` | resource | Monitors: `http`, `tcp`, `ping`, `heartbeat`, `tls_cert`, `domain_expiry`, `dns`, `flow` checks, and `manual` monitors (provider 0.13.0 and later), whose state is set in the app and never read into config. On a self-hosted install, applying a `flow` needs `max_flow_checks` raised on the plan first ([monitor types](monitor-types.md#flow)) |
| `uptimepage_notification_channel` | resource | Alert destinations — `webhook`, `slack`, `discord`, `msteams`, `google_chat`, `mattermost`, `telegram`, `whatsapp`, `email`, `pagerduty`, `ntfy`, `gotify`, `pushover`, `sms`. The one-tap `telegram_app`, `whatsapp_app`, `slack_app` and `discord_app` kinds are not manageable: their configs are minted by the link, Add to Slack and Add to Discord flows and the API refuses to create them from request bodies (`CHANNEL_KIND_MANAGED`) |
| `uptimepage_status_page` | resource | Public status pages: slug, operator and display names, enabled state, the branding a visitor sees (about text, brand colour, style, website link, whether the powered-by mark shows), and whether search engines index the page. `locale` (`en` or `de`, provider 0.14.0 and later, against a server that has page languages) sets the page language; leave it out to choose the language in the console instead. Removing a `locale` line keeps the language it set, so set `en` to switch back. The logo is uploaded in the console; its URL and the status URL are read back, not set |
| `uptimepage_status_page_component` | resource | What a page publishes — one monitor placed on one page, with its public name, description, group and sort order. The public name is separate from the monitor's internal one |
| `uptimepage_target` | data source | Look up an existing target by id |
| `uptimepage_heartbeat` | data source | The ping URL of a heartbeat monitor, for the job that reports in. The API mints it at creation, so it is read back rather than configured. Reading it needs `targets:write`, since the URL is itself a write capability. Any other check kind reads as not found |

For the full attribute reference and an example per check type, see the
[provider docs on the Terraform Registry](https://registry.terraform.io/providers/uptimepage/uptimepage/latest/docs).

## Managed-by badge

Resources the provider creates or updates carry a `terraform` source marker
(the provider identifies itself on every request). The web UI shows a small
`terraform` chip next to those monitors and channels, plus a banner on the
monitor detail page, so anyone browsing knows the resource is managed as code.

The marker is informational — the UI does not lock the resource. But an edit
made in the UI flips its badge to `ui` and **will be overwritten the next time
you run `terraform apply`**, since your `.tf` files remain the source of truth.
Change managed resources in Terraform, not the UI.

## Source

Provider source and issue tracker:
<https://github.com/uptimepage/terraform-provider-uptimepage>.
