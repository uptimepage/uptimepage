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
  a self-hosted instance, set `endpoint` to your host (the website at
  `https://uptimepage.dev` does not serve `/api/v1`).

## Resources & data sources

| Name | Kind | Manages |
|---|---|---|
| `uptimepage_target` | resource | Monitors: `http`, `tcp`, `ping`, `heartbeat`, `tls_cert`, `domain_expiry`, `dns`, `flow` checks, and `manual` monitors (provider 0.13.0 and later), whose state is set in the app and never read into config. A `dns` check can list an exact record set with `expected_records` (provider 0.15.0 and later). An `http`, `tcp` or `tls_cert` check can be pinned to IPv4 or IPv6 with `ip_version` (provider 0.17.0 and later). On a self-hosted install, applying a `flow` needs `max_flow_checks` raised on the plan first ([monitor types](monitor-types.md#flow)) |
| `uptimepage_notification_channel` | resource | Alert destinations — `webhook`, `slack`, `discord`, `msteams`, `google_chat`, `mattermost`, `telegram`, `whatsapp`, `email`, `pagerduty`, `ntfy`, `gotify`, `pushover`, `sms`. The one-tap `telegram_app`, `whatsapp_app`, `slack_app` and `discord_app` kinds are not manageable: their configs are minted by the link, Add to Slack and Add to Discord flows and the API refuses to create them from request bodies (`CHANNEL_KIND_MANAGED`) |
| `uptimepage_status_page` | resource | Status pages: slug, operator and display names, enabled state, the branding a visitor sees (about text, brand colour, style, website link, whether the powered-by mark shows), and whether search engines index the page. `locale` (`en` or `de`, provider 0.14.0 and later, against a server that has page languages) sets the page language; leave it out to choose the language in the console instead. Removing a `locale` line keeps the language it set, so set `en` to switch back. `access` (provider 0.16.0 and later) decides [who can open the page](public-status.md#who-can-open-a-page); making a page private needs a plan with private pages, opening one works on any plan. It holds `protected`, `session_days`, the `rules` list of addresses and domains, the email code's `enabled`, `passkeys_enabled` and `recheck_days`, and `password` (provider 0.18.0 and later), whose `value_wo` is write-only (Terraform 1.11 and later, never in plan or state) and is sent when `value_wo_version` changes. Read it from an ephemeral variable or resource; Terraform keeps no copy to show later, so the people who need it must get it from where it came from. Provider 0.18.0 moved `session_days` out of `email` to match the server; older providers that set `access` need 0.18.0. Leave it out to manage access in the console; the provider then neither reads nor changes it, and removing it leaves the page as it is. Viewers and blocks stay in the console. The logo is uploaded in the console; its URL and the status URL are read back, not set |
| `uptimepage_status_page_component` | resource | What a page publishes — one monitor placed on one page, with its public name, description, group and sort order. The public name is separate from the monitor's internal one. On a monitor a section shows (provider 0.19.0 and later), it adopts the section's row and manages only its settings |
| `uptimepage_monitor_group` | resource | [Monitor groups](#monitor-groups) (provider 0.19.0 and later): a label, or with `alert_as_one` one incident and one page for all its monitors, with its own channels, reminder and tags. Monitors join with `group_id` |
| `uptimepage_status_page_section` | resource | A monitor group shown on a status page as a live section whose members follow the group (provider 0.19.0 and later) |
| `uptimepage_target` | data source | Look up an existing target by id |
| `uptimepage_monitor_group` | data source | Look up a monitor group by name (provider 0.19.0 and later) |
| `uptimepage_heartbeat` | data source | The ping URL of a heartbeat monitor, for the job that reports in. The API mints it at creation, so it is read back rather than configured. Reading it needs `targets:write`, since the URL is itself a write capability. Any other check kind reads as not found |

For the full attribute reference and an example per check type, see the
[provider docs on the Terraform Registry](https://registry.terraform.io/providers/uptimepage/uptimepage/latest/docs).

## Monitor groups

Provider 0.19.0 adds `uptimepage_monitor_group`, and a monitor joins a [monitor group](groups.md) with `group_id`:

```hcl
resource "uptimepage_monitor_group" "dns" {
  name                   = "example.com DNS"
  alert_as_one           = true
  alerts                 = [{ channel_id = uptimepage_notification_channel.ops.id }]
  renotify_interval_secs = 1800
  alert_again            = true
  tags                   = ["dns", "p0"]
}

resource "uptimepage_target" "ns" {
  name     = "example.com NS"
  interval = 300
  group_id = uptimepage_monitor_group.dns.id
  check = {
    type = "dns"
    dns  = { domain = "example.com", record_type = "NS" }
  }
}
```

`group_id` conflicts with `group_name`, which still puts a monitor in a group by name and creates a label group when no group has that name, on any provider version. While the group alerts as one, its own `alerts`, reminder and tags page for its members' failures. A member keeps the `alerts`, `renotify_interval_secs` and `tags` written in its `.tf`: the server mutes them rather than changing them, so a plan shows no drift, and they apply again once the monitor leaves the group or the group stops alerting as one. The group has no escalation attribute, as targets have none. A group that already exists, made in the console or by `group_name`, is brought under Terraform with `terraform import uptimepage_monitor_group.dns <id>`, the id being the last part of its settings page URL; declaring it fresh is refused with `409 GROUP_NAME_TAKEN`. The `uptimepage_monitor_group` data source looks a group up by name instead, for one you leave to the console.

`uptimepage_status_page_section` shows a group on a status page:

```hcl
resource "uptimepage_status_page_section" "dns" {
  status_page_id = uptimepage_status_page.public.id
  group_id       = uptimepage_monitor_group.dns.id
  heading        = "DNS"
}
```

Its members follow the group onto and off the page, so they need no `uptimepage_status_page_component`. Add one only to set a member's public name, description or order: it adopts the row the section made, and destroying it clears those settings and leaves the monitor on the page. A section imports by `status_page_id:section_id`, as a component imports by `status_page_id:target_id`. Group writes need `targets:write`, and destroying one `targets:delete`; a section, like the rest of a page, needs an owner's `status_page:write`.

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
