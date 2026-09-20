# Troubleshooting

## `/readyz` returns 503

The target store can't be reached. Check `storage.postgres.url` and that Postgres is up. The readiness probe pings the store; liveness (`/healthz`) does not.

## No metrics on `/metrics`

- Confirm `observability.metrics_enabled = true`
- Confirm `metrics_bind` isn't blocked by a local firewall
- `uptimepage_build_info` is emitted at startup so the endpoint is never truly empty — if it's also missing, the metrics exporter never bound

## Many `uptimepage_storage_dropped_results_total{reason="queue_full"}`

The result channel between worker pool and batcher is back-pressured.

- Raise `storage.clickhouse.buffer_size` (mpsc capacity)
- Raise `storage.clickhouse.batch_size` (fewer round-trips per batch)
- Lower `storage.clickhouse.batch_timeout_ms` (more frequent flushes)
- Or lower check frequency for the busiest targets (`interval` per target)

## Circuit breaker stuck open

Look at `uptimepage_checks_errors_total{kind}` filtered by host to find the failure mode, then wait `circuit_breaker.open_duration_secs` for the breaker to enter half-open and probe.

## A monitor shows `degraded` but every check looks fine

Some of its probe regions are failing and the count has not reached the monitor's region policy, so the status is held at degraded instead of down. Open the monitor and read the per-region table: the failing regions are named there. Nothing is paging, because an incident needs the same quorum.

If one region fails while the others stay clean for many monitors at once, suspect that probe's network path rather than your origins. Filter the console to that region to see its raw verdicts.

## Targets reporting `degraded` with `throttled: host concurrency cap`

One tenant has more concurrent monitors at the same `(host, port)` than `checker.per_host_max_inflight` allows (default 2). Over-cap checks are recorded `degraded` instead of running. No alert fires — the upstream is fine. Either spread the targets across more hosts, raise the cap, or rely on jitter to thin the burst. Watch `uptimepage_host_throttle_drops_total` to size the cap against real traffic.

## `domain_expiry` results show `served_stale: …`

The fresh RDAP probe failed (timeout, registry 5xx, network blip) but the executor served the most recent successful answer from `domain_expiry_state` instead of flipping the monitor red. The status reflects the cached `expiry_at`. For Up the `error` field stays empty (the customer-facing surface shows nothing unusual); for Degraded/Down it carries `served_stale: last_verified_age_secs=…; refresh_failed=<kind>` plus the cached details so operators can distinguish a stale serve from a fresh probe.

Inspect the failure kind via `uptimepage_domain_expiry_stale_served_total{kind}`:

- `kind="timeout"` — the lookup took longer than `check.timeout` (per-target). Covers a slow registry and a long wait in the per-TLD queue (`checker.rdap_max_inflight`, gates RDAP and WHOIS alike): many same-TLD checks landing at once, as after an agent restart, drain serially. Either bump the per-check timeout or wait — most registries recover in minutes.
- `kind="lookup_error"` — registry returned a non-2xx (often 404 or 5xx). If a specific TLD is stuck on 5xx, the registry is having an incident; rows keep streaming as `served_stale` until 7 days have passed.
- `kind="fresh_error"` — no usable last-good (first probe, or the cached row is older than 7d). A real `CheckStatus::Error` is emitted and is alert-eligible.

## `domain_expiry` results have flipped to real `Error` after days of `served_stale`

The cached row in `domain_expiry_state` is older than the 7-day staleness ceiling, so the executor stopped masking the registry outage. Either the registry has been down for that long (act on it), or this target's interval is so long that probes haven't run in a week. Check `last_success_at` in `domain_expiry_state` for the target.

## TLS errors against internal hosts

Set `verify_tls: false` on the offending target. The check executor picks between a verifying and a non-verifying hyper-util client based on the flag — both share the same DNS cache and connection-pool sizing.

## An HTTPS monitor reports `certificate chain incomplete` while the site loads in a browser

The server sent its own certificate and nothing else, so there is no path from it up to a trusted root. Browsers hide this. When a certificate names its issuer through the AIA extension, a browser fetches that issuer over HTTP and completes the chain itself. Monitors do not, and neither does `curl`, so a padlock is not evidence the chain is right. Confirm with `openssl s_client -connect host:443 -showcerts`, which prints every certificate the server actually sent.

Usually the fix is on the server: install the full chain, leaf first, then every intermediate up to but not including the root. One other cause produces the same message, because the leaf on its own cannot tell the two apart: the issuer may be a private CA the probe does not carry, in which case there is no intermediate to add. For an internal host behind a private CA, set `verify_tls: false` as above.

Two neighbouring reasons: `certificate self-signed` means the certificate is its own issuer, and `certificate not trusted` means what was sent does not reach a root we carry and the certificate gave no hint as to why.

## `400 Bad Request` on POST /targets — `target address ... is in a blocked range`

SSRF guard rejected the target. The URL or TCP host resolves to a private / loopback / link-local / reserved IP. Verify the resolved address is what you expect. To monitor private infrastructure deliberately, set `security.allow_private_targets = true` and ensure network segmentation prevents abuse.

## Check fails with `all resolved addresses for 'host' are in blocked ranges`

DNS returned only private IPs for a target the API previously accepted (hostname literal). Either the record changed or DNS rebinding is in play. The connect-time guard refuses to continue. Either fix DNS or, deliberately, enable `security.allow_private_targets`.

## `credential decryption failed` errors in logs

The KEK loaded at startup can no longer decrypt rows written with a different KEK. Either `security.credentials_kek_base64` was rotated without re-encrypting existing rows, or the wrong key was supplied. Compare the configured KEK against the one used to write the affected targets — there is no automatic rotation. Recovery options:

- Restore the original KEK.
- Delete and re-create the affected targets (the row decrypts cleanly when overwritten via `PATCH` or `POST` under the new key).

## Startup fails with `invalid credentials_kek_base64`

The supplied key is not 32 bytes after base64 decode, or the string is not valid base64. Generate a fresh key with `openssl rand -base64 32`. URL-safe and standard base64 both decode.

## `400 Bad Request` on PATCH /targets/{id} — `basic_auth contains redaction sentinel`

A client read the target back (where credentials are returned as `"***"`) and `PATCH`ed the full `check` body without re-supplying the real credential. Either send the real value, or omit `check` entirely from the `PATCH` body if only other fields are changing.

## `uptimepage_credential_link_refused_total` is above zero

A link callback arrived whose state named one account while the live session
was another, or none. The state alone is not allowed to authorise attaching a
credential, so the request was turned away and nothing changed. The log line
carries both ids:

```
link callback refused: the state names an account the live session is not
  link_user_id=… session_user_id=… provider=…
```

`reason=no_session` means the session expired while the user sat on the
provider's consent screen — benign, and they can simply try again; that series
is not worth alerting on. `reason=other_user` means a live session for somebody
else completed a dance minted for this account: a link state is being replayed
from somewhere it should not be. Correlate against `credential_events` and
`login_attempts` for that `link_user_id` over the same window.

## Someone reports a sign-in method they did not add

Every add and every removal writes a `credential_events` row that outlives the
identity itself, so the answer survives even after the credential is gone:

```sql
SELECT provider, action, origin, ip_hash, occurred_at
  FROM credential_events WHERE user_id = $1 ORDER BY occurred_at DESC;
```

`origin = 'signup'` is the credential the account was created with.
`origin = 'email_match'` means a provider attested an address that already had
an account and it was linked without anyone clicking add — check whether the
`ip_hash` matches their usual `login_attempts` rows. `origin = 'session'` means
someone signed in as them and added it deliberately, which points at the
session, not the provider.

## `429 Too Many Requests` on `/api/v1/*`

Per-IP rate limiter is active and the bucket is empty. Read the `Retry-After` header for the wait time, or raise `api.rate_limit.{per_second, burst}`. If every client appears to share one bucket, the service is sitting behind a reverse proxy and the peer IP is the proxy — disable the in-app limiter (`api.rate_limit.enabled = false`) and let the proxy enforce per-client limits instead.

## ClickHouse insert fails with `SchemaMismatch`

Almost always a Row-derive mismatch on UUID, Enum8, or DateTime64 column types:

- UUID columns require `#[serde(with = "clickhouse::serde::uuid")]` on the field
- Enum8 columns require an `i8` field, not `&str`
- DateTime64 filter binds in `WHERE` clauses need wrapping in `fromUnixTimestamp64Milli(?)` — raw `i64` won't coerce to DateTime64 in CH expressions

## Loadtest reports `connect` errors at high concurrency

Loopback ephemeral port exhaustion or kernel SYN backlog overflow. See [loadtest.md](loadtest.md) — set `MOCK_PORTS=64` or `RAMP_SECS=30`.

## `403 FLOW_CHECKS_DISABLED` when creating a flow monitor

The org's plan allows zero flow checks. `plans.max_flow_checks` is both the per-plan cap and the feature's kill switch, and it is seeded at 0. Raise it on the plan the org sits on. Turning the engine on with `flow.enabled` does not lift this, and lifting this does not make a flow run anywhere — see [Browser flow monitors](configuration.md#browser-flow-monitors).

## Flow monitor errors with `flow engine not configured on this node`

The check reached a process where `flow.enabled` is false or the engine binary is missing at `flow.lightpanda_path`. This is an `Error`, not a `Down`: nothing was learned about the target.

Normally the routing prevents it, because a flow monitor's regions are clamped to the flow-capable set when it is saved and an agent that reports no engine never receives one in its config pull. Seeing this means a node's config changed after the monitor was saved. Either turn the engine back on there, or re-save the monitor so its regions are clamped again.

## Flow monitor errors with `step N/M <op>: run budget spent`

The whole-run budget ran out while that step was waiting, or just before it started. `timeout` caps the entire run, not each step, and a browser reaching a slow origin can spend it before the last assertion. Raise `timeout`, or shorten the flow.

It reads like a step failure but is recorded `Error`, not `Down`: the target never got to answer. The step trace shows how far the run got and where the time went, and the page snapshot is collected the same as for a real failure — check whether the named step was waiting for something that never renders, or whether the steps before it were simply slow.

A `wait_for` p95 climbing toward `step_timeout` on `uptimepage_flow_step_duration_ms` is the same signal before it becomes an outage.

## Flow monitor errors with `run budget spent before the first step ran`

The budget went on starting the browser and loading the start URL, leaving nothing for the steps. No step is named because none ran.

Usually the start URL itself is slow, so probe it with an HTTP monitor first. A node under load starts the browser more slowly too — check `uptimepage_flow_runs_total{outcome="budget"}` against one node rather than reading a single run.

## Flow monitor errors instead of failing, with no step named

The run broke rather than reaching a verdict. None of these collect a page snapshot: after the transport breaks, the page no longer says anything trustworthy about the target.

- CDP broke mid-run. The trace still shows the steps that passed before the break; the step it broke on gets no outcome, because the page never answered either way.
- It crossed `flow.mem_limit_mb` and was killed. One page pulling in a heavy bundle is enough. Raise the ceiling for that node, or set `flow.max_response_mb` so a single oversized asset is refused before it costs the whole budget.
- The browser process never started (`spawn lightpanda`, `engine did not start after retries`, `no free port`). Repeated cases point at the node, not the target.
- `engine stopped responding after its N ms budget` — a CDP call never returned, so the run sailed past its own deadline and the outer backstop ended it.

The last three end the run before or outside the step list, so they carry no trace at all.

All are recorded `Error` on purpose. The target did not fail; this node declined, or was unable, to keep paying for the answer.

## A flow `fill` step passes but the form behaves as if nothing was typed

The step sets the field's value and fires `input` and `change`. An ordinary form accepts that. A framework that tracks its own value setter can ignore an assignment made this way, so the step reports success against a field the application still considers empty. The submit then does nothing and the `assert_url` step is what actually fails.

The failure evidence is where you confirm it: the URL is still the login path and the page text usually says the credentials were rejected. There is no workaround inside the flow — this is a limit of how the engine types. See [What a flow cannot do](monitor-types.md#what-a-flow-cannot-do).

## A flow `fill` or `click` fails immediately on a slow page

`step_timeout` does not apply to them. `fill` and `click` look for their element once and fail on the spot; only `wait_for`, `assert_text` and `assert_url` poll until the timeout runs out. Raising `step_timeout` will not help a field that renders late.

Put a `wait_for` on that selector ahead of the step that needs it.

## A heartbeat monitor flaps overnight for a job that ran fine

The declared period is shorter than the job's real cadence. A heartbeat goes down at `period + grace`, so if the job genuinely runs less often than you declared, every ping buys one window and then expires before the next one arrives. The monitor alternates down and up on each run, and the job was never broken.

A real case: `period=600` with `grace=300` on a job that actually ran about every 80 minutes. Every up interval lasted exactly period plus grace. The evaluator was correct and the declaration was wrong.

The monitor page tells you this once it has enough pings. It takes the median gap between successful pings inside a 14-day window and needs at least five gaps before it will judge, so an hourly job has a verdict within hours and a nightly one after about five days. A job too slow to fit five gaps into the fortnight gets no verdict at all. The same three values are on the API as `declared_period_secs`, `observed_period_secs` and `cadence_advice`. The advice is judged against `period + grace` rather than the bare period, because a job sitting inside its grace window is not late and does not page anyone.

Set the period to how often the job really runs and keep the grace wide enough for normal jitter. The opposite error is quieter and worse: a period much longer than the real cadence leaves a dead job unnoticed for far longer than it needs to be, and `cadence_advice` flags that direction too. See [Heartbeat](monitor-types.md#heartbeat).
