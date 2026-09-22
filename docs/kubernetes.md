# Kubernetes

Two Helm charts live in `charts/`:

- **`uptimepage`** runs the control plane: the API, the UI, alerting, and, by default, probing.
- **`uptimepage-agent`** runs a probe on its own, in another cluster or inside a private network. It works against a self-hosted control plane and against the hosted service.

Both need Kubernetes 1.29 or newer, declared in `kubeVersion` so Helm refuses anything older instead of failing later in a way that is harder to read. Nothing in the manifests needs a newer API than that, and CI validates every rendered object against both 1.29 and the current release.

Upstream only patches the three most recent minors, so a cluster older than that is covered by your vendor's extended support rather than by the Kubernetes project. The charts install either way; that part is your call, not theirs.

Neither chart bundles a database. Point them at whatever Postgres and ClickHouse you already run.

## Install

Create the secrets first, then point the chart at them:

```bash
kubectl create namespace uptimepage

kubectl -n uptimepage create secret generic uptimepage-core \
  --from-literal=fingerprint-salt="$(openssl rand -base64 32)" \
  --from-literal=credentials-kek-base64="$(openssl rand -base64 32)"

kubectl -n uptimepage create secret generic uptimepage-db \
  --from-literal=postgres-url='postgres://uptimepage:pw@pg.internal:5432/uptimepage?sslmode=require' \
  --from-literal=clickhouse-password='pw'

helm install uptimepage oci://ghcr.io/uptimepage/charts/uptimepage \
  --namespace uptimepage \
  --set domain=status.example.com \
  --set clickhouse.url=https://ch.internal:8443 \
  --set secrets.existingSecret=uptimepage-core \
  --set postgresql.existingSecret=uptimepage-db \
  --set clickhouse.existingSecret=uptimepage-db
```

That is a complete install: one pod that serves the app and probes its own region. Add `--set ingress.enabled=true` when you want it reachable from outside the cluster, or port-forward to try it first.

The fingerprint salt is required and boot fails without it. The KEK encrypts stored monitor credentials and agent tokens, and must decode to exactly 32 bytes; without one, those values are stored in plaintext.

Passing the secrets through `--set` also works and lets the chart build the Secret itself, but Helm keeps the values you supply as part of the release. `helm get values uptimepage` then prints the salt, the KEK, and the database password in cleartext to anyone who can read releases in that namespace. Use it for a trial, not for a real install.

## Verifying what you installed

Both charts are signed with cosign keyless signing, so there is no public key to distribute:

```bash
cosign verify ghcr.io/uptimepage/charts/uptimepage:0.6.0 \
  --certificate-identity-regexp '^https://github.com/uptimepage/uptimepage/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

The two identity flags are what make the check mean anything. Omit them and cosign will happily accept a signature made by anyone at all.

## Database requirements

**Postgres 18 or newer.** The migrations use the native `uuidv7()`, so an older server dies partway through the first boot. An init container checks the version before the app container starts, so you get one clear error instead of a crash loop.

The migrations also create the `citext` and `pg_trgm` extensions, which means the app's role needs permission to `CREATE EXTENSION` on first boot. If you would rather not grant that, create both as an admin beforehand.

Do not put RDS Proxy in front. The connection pool runs `SET idle_in_transaction_session_timeout` on every connection, and a session-level `SET` pins the proxy connection for its lifetime, so the proxy costs money and multiplexes nothing. Use the direct endpoint.

**ClickHouse** works as a managed single node or on ClickHouse Cloud, which converts `MergeTree` to `SharedMergeTree` on its own. A self-managed replicated cluster is not supported: the schema has no `ON CLUSTER` statements, so replicas would not share data. An init container creates the database, because no migration does and the client binds the database name when it is constructed.

## One control plane, many probes

`app.replicaCount` is capped at 1 and the chart refuses to render above it. The control plane has no leader election. The incident writer, alert engine, notification dispatcher, and retention loops all run in every process, so a second pod sends every alert twice and writes every incident twice. Rollouts use the `Recreate` strategy for the same reason, which trades a few seconds of downtime for never running two of them at once.

Probing is what scales out. Switch to `mode: split` and give each region its own agent:

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

Mint each token on the control plane first. In-cluster agents reach it over the Service, so they keep working when the ingress does not. One agent per region, for the same reason as above: two would run every check twice.

## Ingress

The chart replaces what Caddy does in the Docker Compose stack, so your ingress controller handles TLS.

A test or "check now" request waits up to 145 seconds for its result, the longest being a flow test with a 120-second timeout. A proxy read timeout below that cuts the connection and the request fails with no error anywhere. The chart writes ingress-nginx annotations from `ingress.proxyTimeoutSeconds`, which defaults to 150. Since chart 0.6.0 the schema rejects a value below 150, so an upgrade that pins an older, lower value fails until you raise it. On another controller, set the equivalent through `ingress.annotations`.

`security.trustedProxies` has to cover the ingress controller's pod network. The default RFC1918 ranges usually do. Get it wrong and every request looks like it came from a single address, so the abuse guard throttles all of your users at once.

Public status pages are served at `/status/<slug>` by default. For `<slug>.example.com` instead, set `tenancy.subdomainPublicRoutes=true` and `ingress.wildcard.enabled=true`, and issue the wildcard certificate through a DNS-01 issuer.

## ICMP checks

Ping opens an unprivileged `SOCK_DGRAM` socket, which needs the pod's GID inside `net.ipv4.ping_group_range`. Docker widens that range for you and Kubernetes does not, so both charts set it as a pod sysctl. It is on the kubelet's safe list, so no node configuration is needed.

If a hardened cluster rejects it anyway, the pod schedules and then fails to launch. Set `ping.sysctl=false` and `ping.addNetRaw=true` to use a raw socket instead, or turn the sysctl off and accept that ping checks report `error` with the reason while every other check type keeps working.

## Egress and probe addresses

Checks leave from whatever node the probe lands on, so anyone allowlisting your probe addresses sees the node's or the NAT gateway's address. Pin egress if that matters to the people you monitor.

`networkPolicy.enabled=true` restricts inbound traffic only. Probe egress stays open on purpose, because reaching arbitrary endpoints is the entire job and narrowing it turns healthy targets into false outages.

## Secrets

Set `secrets.existingSecret` to manage credentials with External Secrets, Vault, or sops. That Secret owns the whole core set:

| Key | Required | Holds |
| --- | --- | --- |
| `fingerprint-salt` | yes | session and login-attempt hashing salt |
| `credentials-kek-base64` | no | KEK for stored credentials and agent tokens |
| `operator-admin-token` | no | empty makes `/operator/*` return 404 |
| `github-client-secret` | with GitHub OAuth | |
| `google-client-secret` | with Google OAuth | |
| `resend-api-key` | with `email.provider=resend` | |

Database credentials are separate, so they can come from a different source: `postgresql.existingSecret` holds the whole DSN under `postgres-url`, and `clickhouse.existingSecret` holds the password under `clickhouse-password`.

## Settings env vars cannot express

The app reads configuration from environment variables, but only two list-valued settings parse that way: `dns.servers` and `security.trusted_proxies`. Anything else that takes a list, `auth.enabled_methods` in particular, has to come from a config file. Mount one with `app.extraVolumes` and `app.extraVolumeMounts`, then point `UPTIMEPAGE_CONFIG_PATH` at it through `app.extraEnv`.

## Metrics

`metrics.serviceMonitor.enabled=true` creates ServiceMonitors for the control plane and for every probe, with each probe's region copied onto a `region` label. `/metrics` is unauthenticated, which is why the chart never routes it through the ingress. Scrape it on the cluster network.

## Agents elsewhere

Install `uptimepage-agent` in any cluster that should probe on your behalf:

```bash
helm install probe-us-east oci://ghcr.io/uptimepage/charts/uptimepage-agent \
  --namespace uptimepage --create-namespace \
  --set controlPlaneUrl=https://status.example.com \
  --set region=us-east \
  --set token=sm_agent_...
```

It makes outbound connections only, so it can sit inside a private network and check services that never face the internet. Keep its image version matched to the control plane's.

There is nothing to health-check locally, since the agent serves no API. A dead agent is noticed centrally: results stop arriving, `agent_up` drops, and the region-silence alert fires.
