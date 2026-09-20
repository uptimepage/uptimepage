# Grafana dashboards

Dashboards for the `uptimepage_*` Prometheus series.

**Managed by Terraform** — `terraform/dashboard.tf` provisions the
overview into Grafana Cloud as code (single source of truth; a UI edit
is drift, reverted on next `apply`). This directory keeps the
**metric-name drift gate** and these docs; the JSON itself now lives in
the Terraform module so HCP remote-exec can `file()` it.

| File | Datasource | Scope |
|---|---|---|
| `terraform/dashboards/uptimepage-overview.json` | Prometheus | Operator metrics, single-instance. **No `org_id` label** — this is operator telemetry, not per-tenant uptime. |

## Datasource contract

The JSON is exported in Grafana's shareable form: it declares one input
`DS_PROMETHEUS` and every panel references `${DS_PROMETHEUS}`. On import
you bind it to whichever Prometheus datasource holds the scraped series
(local Prometheus, or Grafana Cloud Prometheus once an Alloy
`remote_write` agent is pointed at `:9090/metrics`). No datasource UID
is hard-coded, so the dashboard is portable across environments.

The full series list, label sets, and the summary-vs-histogram
exposition rule that the latency panels rely on are single-sourced in
[`docs/metrics.md`](../../docs/metrics.md#series) — that document is
canonical; this README does not restate it.

`__requires[].version` (`grafana 11.0.0`) is a **minimum-version
floor**, not the authoring version — it is intentionally low for
portability. Do not bump it on every Grafana upgrade; Grafana
auto-migrates `schemaVersion` forward on load.

Prod is Terraform-driven; the sections below are fallbacks for a
local/standalone Grafana that has no Terraform.

## Import — manual (UI)

1. Grafana → Dashboards → New → Import.
2. Upload `terraform/dashboards/uptimepage-overview.json` (or paste
   its contents).
3. When prompted for `DS_PROMETHEUS`, pick the Prometheus datasource
   that scrapes `:9090/metrics`.

## Import — provisioning (local only)

Mount `terraform/dashboards/` into Grafana and point a dashboard
provider at it.
Add to Grafana's provisioning (`/etc/grafana/provisioning/dashboards/uptimepage.yaml`):

```yaml
apiVersion: 1
providers:
  - name: uptimepage
    orgId: 1
    folder: uptimepage
    type: file
    disableDeletion: false
    updateIntervalSeconds: 30
    allowUiUpdates: false
    options:
      path: /var/lib/grafana/dashboards/uptimepage
      foldersFromFilesStructure: false
```

Provisioned dashboards need the datasource resolvable by name. Either
name the Prometheus datasource so the `${DS_PROMETHEUS}` input resolves,
or provision the datasource alongside (its `uid` then satisfies the
input). Do **not** edit provisioned JSON in the UI — `allowUiUpdates:
false` keeps the file the source of truth.

## Secrets

Nothing in this directory contains a credential and nothing here should.
Grafana Cloud / datasource credentials are supplied by the deployment
(datasource provisioning env, or the Alloy `remote_write` agent), never
committed to the repo.

## Updating the dashboards

Edit `terraform/dashboards/uptimepage-overview.json`. Every PromQL
expression must reference a metric registered in
`src/observability/metrics.rs` (`observability::metrics::names`) or
sampled in `src/scheduler/sampler.rs`. After any edit run the gate
(it reads the JSON from the Terraform module):

```bash
dashboards/grafana/check-metric-names.sh
```

It fails (non-zero) if a panel **or an alert rule in `terraform/alerts.tf`**
references a name absent from the binary, **or** if a registered metric
name has silently drifted out of `docs/metrics.md` — closing both
directions of the drift the metric table is prone to. The alert check
matters most: a misspelled name there returns no series, which the rules
read as `no_data = OK`, so the rule looks healthy and can never fire.
Wire it into pre-commit / CI to keep the doc table from rotting.
