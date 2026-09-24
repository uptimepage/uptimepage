+++
title = "Monitoring as code: put your uptime checks in a pull request"
date = "2026-06-16"
updated = "2026-08-18"
slug = "monitoring-as-code"
excerpt = "Click-created monitors rot: nobody recalls why a threshold is set, and the reasoning leaves with its author. Terraform fixes that, and bites back in places."
tags = ["terraform", "infrastructure-as-code", "monitoring", "devops"]
draft = false
+++

Open your monitoring dashboard and count the checks nobody can explain. The one with the 47-second interval: why 47? The two still pointed at a staging box that was decommissioned in March. The HTTP check that's been amber so long the whole team reads amber as green. Somebody created each of these on purpose, once. Then they switched teams, or left, and the reasoning walked out the door with them.

That's what clicking buttons does to monitoring. The config is real, it's load-bearing, and it lives nowhere you can read it. You can't diff it, can't review it, can't ask `git blame` who set that timeout and what they were thinking. The monitor is supposed to be the thing you trust when everything else is on fire, and you've built it on a pile of undocumented clicks.

So put it in version control. Not because infrastructure-as-code is a virtue to collect, but because a monitor is exactly the kind of quiet, long-lived config that turns into a liability the moment it's invisible.

> **TL;DR**
>
> A monitor clicked into a dashboard has no diff, no history, and no record of why. Move it into a repo and every change is a reviewed pull request you can `git blame`; a new region becomes one `apply` instead of an afternoon of clicking. The price is a state file to maintain. Uptimepage supports this through Terraform, a REST API and MCP.

## What "as code" actually buys you

It is not the typing. Declaring a monitor in HCL is more keystrokes than clicking "new check," and anyone who tells you the typing is the point has never maintained more than three of them.

The point is the pull request. When a monitor lives in a repo, changing it becomes a thing a second person looks at *before* it's real. "Why are we dropping the interval to ten seconds on the payments check?" is a much better conversation to have in a PR than in a postmortem. You get a diff and you get history. You get to stand up a brand-new region and reproduce forty monitors in one `apply` instead of forty afternoons of clicking. And when the person who set the threshold leaves, the threshold and the reason for it stay behind in the file.

Here's the smallest thing that works, with the [Uptimepage Terraform provider](/terraform-uptime-monitoring):

```terraform
terraform {
  required_providers {
    uptimepage = {
      source = "uptimepage/uptimepage"
    }
  }
}

resource "uptimepage_target" "api" {
  name     = "api prod"
  interval = 60

  check = {
    type = "http"
    http = {
      url = "https://api.example.com/healthz"
      expected_status = {
        kind  = "exact"
        exact = 200
      }
    }
  }
}
```

Notice the `check` block is nested, not a flat pile of fields. You set `type = "http"` and then fill in an `http = { ... }`. That's a deliberate schema choice, and a good one: a flat resource with `url`, `port`, `host`, `record_type`, and `cert_days` all hanging off the top level would let you write nonsense, like a TCP check with an HTTP status matcher, and only find out at apply time. The nested shape makes the invalid states unrepresentable. You can only set the `http` fields when the type is `http`. It reads a little verbose, and it saves you from a category of mistake the flat version invites.

## The part the quickstart skips

Every Terraform tutorial stops at "and now run apply." Three things they don't tell you, in rough order of how badly they'll bite.

**State, not the dashboard.** Terraform records what it believes exists. Bump an interval in the web UI and Terraform doesn't know; the next `plan` cheerfully proposes to revert your hand-edit back to what the code says. This is working as designed: once a monitor is in code, *the code wins*, and clicking around the dashboard becomes drift that Terraform will quietly undo. Decide that up front. Run [`terraform plan -refresh-only`](https://developer.hashicorp.com/terraform/cli/commands/plan) to see drift before it surprises you.

**Delete the block, delete the monitor.** Remove those eight lines from your `.tf` file, run apply, and the actual check stops running. Silently. For most resources that's a shrug. For the thing watching your production API, "I cleaned up some config and we stopped monitoring payments for a week" is a real sentence people have said out loud. I have come closer to saying it than I'd like. Treat a removed monitor with the same suspicion as a dropped table, because the blast radius is the same: you don't notice until the thing you stopped watching breaks.

**Secrets in state.** If your check needs basic auth or a token, that value has to get to the provider somehow, and whatever you pass is [written to the state file in plaintext](https://developer.hashicorp.com/terraform/language/state/sensitive-data), for anyone with read access to the backend. The provider marks the password `sensitive`, and it's worth being precise about what that buys you: `sensitive` keeps the value out of plan output and logs. It does not keep it out of state. Terraform 1.11, back in February 2025, added the real fix, [write-only arguments](https://developer.hashicorp.com/terraform/language/resources/ephemeral/write-only): values that flow through to the provider on apply and are never persisted to state. Until the Uptimepage provider adopts them, treat the state file itself as a secret:

```terraform
variable "admin_password" {
  type      = string
  sensitive = true
}

resource "uptimepage_target" "admin" {
  name     = "admin panel"
  interval = 120

  check = {
    type = "http"
    http = {
      url = "https://admin.example.com/"
      expected_status = {
        kind = "range"
        range = {
          min = 200
          max = 299
        }
      }
      basic_auth = {
        username = "uptime"
        password = var.admin_password
      }
    }
  }
}
```

The password reaches the API and stays out of your terminal, but it still lands in state. Use a backend that encrypts at rest and be deliberate about who can read it. Once the provider picks up write-only arguments, the password will stop touching state entirely; until then, the encrypted backend is what actually protects it.

## Don't rebuild what you already have

If you've already got monitors created by hand, and you do, you don't have to re-enter them. [Config-driven import](https://developer.hashicorp.com/terraform/language/import) has been in Terraform since 1.5: write the resource block the way you want it, add an `import` block pointing at the existing monitor's id, and `apply` adopts it instead of creating a duplicate.

```terraform
import {
  to = uptimepage_target.api
  id = "the-existing-monitor-id"
}
```

Run `plan` and read it like a hawk. A clean import shows no changes. If the plan wants to modify things, your HCL doesn't match reality yet, so fix the code, not the monitor, until the diff is empty. Then delete the `import` block; it's done its job. One catch from the section above: a secret like that basic-auth password can't be read back from the API, so an import won't recover it. You set it in config yourself, once, and Terraform takes it from there.

## A workflow that won't page you

The shape that holds up: remote state with locking (so two people can't apply at once and corrupt it), `terraform plan` running automatically on every pull request so reviewers see the diff, and `apply` gated behind a merge to your main branch. Nobody runs `apply` from a laptop at 2 a.m. The same discipline you'd want around a database migration, pointed at the thing that tells you the database is down.

Monitors and status pages and notification channels are all just resources here (`uptimepage_status_page`, `uptimepage_notification_channel`, and friends), so the whole public face of your monitoring (which page shows what, who gets paged on which channel) ends up reviewable in the same PR as the checks themselves.

The same trick works in the other direction: an MCP server is itself a thing that goes down, and [monitoring one](/blog/monitor-an-mcp-server) is a monitor you can declare here alongside the rest.

And the same monitors you declare in code, an AI assistant can [read back over MCP](/blog/mcp-server): "what's broken right now, and since when?" answered in plain language, from the exact config that's sitting in your repo. Declared in a pull request on one side, queried by an assistant on the other. Same data, same scopes.

## Boring, in code too

We've [argued before](/blog/boring-uptime) that your monitor should be the dullest, most trustworthy thing you own. Configuration is part of that, and it's worth weighing when you pick a tool: of [the open-source and self-hosted options](/blog/best-self-hosted-uptime-monitoring-tools), the ones that keep their config in a file you can commit age better than the ones that hide it behind clicks. Worth checking who actually maintains the provider, too, since plenty of tools claim Terraform support and leave it to a volunteer; [the monitoring Terraform providers, compared](/compare/terraform-providers) sets out who publishes what. A monitor whose definition you can read, review, diff, and roll back is a more boring monitor than one assembled from clicks that nobody remembers, and boring, here, is the entire compliment.

Put the config in the repo. Let a second person read the diff. Your future self, squinting at a 47-second interval eighteen months from now, will at least be able to run `git blame` and find out it was you.
