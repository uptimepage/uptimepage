# uptimepage-agent

A regional probe for an Uptimepage control plane. It pulls its region's monitors over HTTPS, runs the checks from wherever this cluster sits, and pushes results back.

Two reasons to run it:

- **Another region.** Checks leave from this cluster's egress, so you see what users there see.
- **A private location.** Services that never reach the public internet can still be checked, because the agent only ever makes outbound connections.

It runs no web server, opens no inbound ports, and stores nothing. It works against a hosted control plane and a self-hosted one alike.

## Install

Needs Kubernetes 1.29 or newer, declared in `kubeVersion`. Nothing in the manifests needs a newer API than that.

Mint a region and an agent token on the control plane first, then:

```bash
kubectl create namespace uptimepage
kubectl -n uptimepage create secret generic probe-us-east \
  --from-literal=agent-token='sm_agent_...'

helm install probe-us-east oci://ghcr.io/uptimepage/charts/uptimepage-agent \
  --namespace uptimepage \
  --set controlPlaneUrl=https://app.example.com \
  --set region=us-east \
  --set existingSecret=probe-us-east
```

`--set token=sm_agent_...` works too and lets the chart create the Secret, but Helm stores supplied values with the release, so `helm get values` hands the token to anyone who can read releases in that namespace. The token is a credential for your control plane; treat it like one.

Verify the chart before installing it. Releases are signed with cosign keyless signing:

```bash
cosign verify ghcr.io/uptimepage/charts/uptimepage-agent:0.3.2 \
  --certificate-identity-regexp '^https://github.com/uptimepage/uptimepage/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

The image tag must match the version the control plane runs. It defaults to the chart's appVersion, so upgrade both together.

## One agent per region

`replicaCount` is capped at 1 and the chart refuses to render above it. Two agents in the same region both pull the full monitor list, so every check runs twice: double the probe traffic against customer endpoints and double the stored results. Rollouts use `Recreate` for the same reason.

To probe from more places, install this chart again with a different `region`.

## Liveness

There is nothing to health-check locally. The agent exposes no API, and the image is distroless so there is no shell for an exec probe. Whether an agent is alive is decided on the control plane: results stop arriving, `agent_up` drops, and the region-silence alert fires. Per-monitor silence then tells affected customers rather than leaving it as a stale gauge nobody reads.

## ICMP checks

Ping opens an unprivileged `SOCK_DGRAM` socket, which needs the pod's GID inside `net.ipv4.ping_group_range`. Kubernetes does not widen it by default, so the chart sets it as a pod sysctl. It is on the kubelet's safe list, so no node configuration is needed. If a hardened cluster rejects it anyway, the pod schedules and then fails to launch: set `ping.sysctl=false` and `ping.addNetRaw=true` to use a raw socket instead. With neither, ping checks in this region report `error` with the reason and every other check type keeps working.

## Browser flow checks

`flow.enabled=true` needs an image built with `WITH_LIGHTPANDA=true`. The stock image does not carry the browser engine, and flow checks assigned to this region will error.

## Egress

Reaching arbitrary endpoints is the whole point, so `networkPolicy.enabled=true` still allows all egress and only restricts inbound traffic to the metrics port. If your cluster forces egress through a gateway, note that every check appears to come from the gateway's address, which matters to anyone allowlisting your probe IPs.
