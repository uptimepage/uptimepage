# uptimepage

Self-hosted uptime monitoring, incidents, and status pages on Kubernetes.

## Requirements

- Kubernetes 1.29 or newer, declared in `kubeVersion` so Helm refuses anything older. Nothing in these manifests needs a newer API than that. Note that upstream only patches the three most recent minors, so a cluster below that line is your vendor's responsibility to keep secure, not ours. The chart still installs.
- **PostgreSQL 18 or newer.** The migrations call the native `uuidv7()`, so an older server fails partway through the first boot. An init container checks this before the app container starts.
- ClickHouse. A managed single-node instance or ClickHouse Cloud both work. A self-managed replicated cluster does not: the schema uses `MergeTree` and `AggregatingMergeTree` with no `ON CLUSTER`, so replicas would not share data.
- An ingress controller, if you want the app reachable from outside the cluster.

Neither database is bundled. Point the chart at whatever you already run, or install CloudNativePG and the Altinity ClickHouse operator first.

## Install

Create the secrets first, then point the chart at them:

```bash
kubectl create namespace uptimepage

kubectl -n uptimepage create secret generic uptimepage-core \
  --from-literal=fingerprint-salt="$(openssl rand -base64 32)" \
  --from-literal=credentials-kek-base64="$(openssl rand -base64 32)"

kubectl -n uptimepage create secret generic uptimepage-db \
  --from-literal=postgres-url='postgres://uptimepage:pw@pg.example.internal:5432/uptimepage?sslmode=require' \
  --from-literal=clickhouse-password='pw'

helm install uptimepage oci://ghcr.io/uptimepage/charts/uptimepage \
  --namespace uptimepage \
  --set domain=status.example.com \
  --set clickhouse.url=https://ch.example.internal:8443 \
  --set secrets.existingSecret=uptimepage-core \
  --set postgresql.existingSecret=uptimepage-db \
  --set clickhouse.existingSecret=uptimepage-db
```

Both generated values are load-bearing. The fingerprint salt is required and the app refuses to boot without it. The KEK encrypts stored monitor credentials and agent tokens, must decode to exactly 32 bytes, and leaving it out stores those values in plaintext.

You can pass everything through `--set` instead, and the chart will build the Secret for you. Know what that costs: Helm stores the values you supply as part of the release, so `helm get values uptimepage` prints the salt, the KEK, and the database password in cleartext to anyone who can read releases in that namespace, and they are in the rendered manifest too. Fine for a laptop, not for anything real.

## Verifying the chart

Releases are signed with cosign keyless signing, so there is no public key to distribute:

```bash
cosign verify ghcr.io/uptimepage/charts/uptimepage:0.6.0 \
  --certificate-identity-regexp '^https://github.com/uptimepage/uptimepage/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

The identity flags are the point of the exercise. Without them cosign accepts a signature from anybody.

## Topology

`mode: allInOne` (the default) runs one pod that serves the API and UI, evaluates alerts, and probes its own region. That is the whole install.

`mode: split` turns off in-process probing and runs one agent Deployment per region instead:

```yaml
mode: split
app:
  region: eu-helsinki
probes:
  - name: eu-helsinki
    region: eu-helsinki
    token: sm_agent_...
  - name: us-east
    region: us-east
    token: sm_agent_...
```

Mint each token on the control plane first. Agents reach it over the in-cluster Service by default, so a split install does not depend on the ingress being up.

Each entry also takes `resources`, `nodeSelector`, `tolerations`, `affinity`, `topologySpreadConstraints`, `podAnnotations`, `podLabels`, and `priorityClassName`. Setting `replicaCount: 0` parks a region without removing it from the release.

For agents outside this cluster, use the separate `uptimepage-agent` chart.

## The single-replica constraint

`app.replicaCount` is capped at 1 and the chart refuses to render above it. The control plane has no leader election: the incident writer, alert engine, notification dispatcher, and retention loops all run in every process, so a second pod would send every alert twice. Rollouts use the `Recreate` strategy for the same reason, which means a short gap during upgrades rather than an overlap.

This is a property of the app, not a chart limitation. Scale probing horizontally with `probes` instead.

## Ingress

A test or "check now" request waits up to 145 seconds for its result (a flow test with a 120-second timeout), so the proxy read timeout has to clear that or the request fails silently. The chart sets ingress-nginx annotations from `ingress.proxyTimeoutSeconds` (150 by default). Since 0.6.0 the schema rejects a value below 150, so an upgrade that pins an older, lower value fails until you raise it. On another controller, set the equivalent yourself through `ingress.annotations`.

Public status pages are served at `/status/<slug>` on the app host by default. To serve them at `<slug>.example.com`, set `tenancy.subdomainPublicRoutes=true` and `ingress.wildcard.enabled=true`, and issue the wildcard certificate through a DNS-01 issuer.

`security.trustedProxies` must cover your ingress controller's pod network, which the default RFC1918 ranges usually do. Get it wrong and every request appears to come from one address, which makes the abuse guard throttle all clients together.

## ICMP checks

Ping checks open an unprivileged `SOCK_DGRAM` socket, which needs the pod's GID inside `net.ipv4.ping_group_range`. Kubernetes does not widen that by default the way Docker does, so the chart sets it as a pod sysctl. It is on the kubelet's safe list, so no node configuration is needed.

If a hardened cluster rejects it anyway, the pod schedules and then fails to launch. Set `ping.sysctl=false` and `ping.addNetRaw=true` to use a raw socket instead, or `ping.sysctl=false` alone to give up ping: those checks then report `error` with the reason and every other check type keeps working.

## Secrets

Set `secrets.existingSecret` to manage them yourself with External Secrets, Vault, or sops. The chart reads these keys:

| Key | Required | Holds |
| --- | --- | --- |
| `fingerprint-salt` | yes | session and login-attempt hashing salt |
| `credentials-kek-base64` | no | KEK for stored credentials and agent tokens |
| `operator-admin-token` | no | empty makes `/operator/*` return 404 |
| `github-client-secret` | with GitHub OAuth | |
| `google-client-secret` | with Google OAuth | |
| `microsoft-client-secret` | with Microsoft OAuth | |
| `gitlab-client-secret` | with GitLab OAuth | |
| `resend-api-key` | with `email.provider=resend` | |

Database credentials are separate: `postgresql.existingSecret` holds the whole DSN under `postgres-url`, and `clickhouse.existingSecret` holds the password under `clickhouse-password`. All three settings can point at the same Secret, since none of the keys collide.

## Managed database notes

- **Do not put RDS Proxy in front of Postgres.** The pool runs `SET idle_in_transaction_session_timeout` on every connection, and a session-level `SET` pins the proxy connection for its lifetime, so you pay for the proxy and get no multiplexing. Use the direct endpoint.
- `citext` and `pg_trgm` are created by the migrations, so the app's role needs permission to `CREATE EXTENSION` on first boot. Pre-create both as an admin if you would rather not grant it.
- The ClickHouse database is created by an init container, because no migration does it and the client binds the database name at construction time. Disable with `preflight.createClickhouseDatabase=false` if you create it yourself.
- ClickHouse Cloud converts `MergeTree` to `SharedMergeTree` automatically, so the schema applies unchanged.

## Configuration that env vars cannot express

The app parses only two list-valued settings from the environment (`dns.servers` and `security.trusted_proxies`). Anything else that takes a list, notably `auth.enabled_methods`, has to come from a config file. Mount one through `app.extraVolumes` and `app.extraVolumeMounts`, then point `UPTIMEPAGE_CONFIG_PATH` at it with `app.extraEnv`.

## Metrics

`metrics.serviceMonitor.enabled=true` creates ServiceMonitors for the control plane and for each probe, with the region copied onto a `region` label. `/metrics` has no authentication, which is why the chart never routes it through the ingress. Keep it on the cluster network.
