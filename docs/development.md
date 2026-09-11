# Development

Local setup for iterating on the service. For production deployment see
[deployment.md](deployment.md).

## Prerequisites

- Rust 1.95+ (edition 2024) via `rustup`
- Docker + Docker Compose (for Postgres + ClickHouse)
- Optional: [`just`](https://github.com/casey/just) (`brew install just`) — every
  workflow below has a one-word `just` recipe equivalent. Run `just` to list
  them.

## Two workflows

| | First build | Incremental | Notes |
|---|---|---|---|
| Host workflow | ~2 min | **~3 s** | `cargo run` natively; only deps in Docker. Best for iteration. |
| Docker dev (cargo-watch) | ~3 min | ~3 s | Source bind-mounted, rebuilds happen inside the container with a cached `target/`. Live reload. |
| Docker prod-shape | ~5 min | ~30 s | Rebuilds image via the `compose.build.yml` overlay. Matches the prod build. Use for CI-shaped smoke tests. |

### Host workflow (recommended for day-to-day)

Bring up just Postgres + ClickHouse:

```bash
docker compose -f compose.dev.yml up -d
```

Run the binary natively:

```bash
UPTIMEPAGE_STORAGE__ALLOW_DEFAULT_CREDENTIALS=true cargo run --bin uptimepage
```

`config/default.toml` already points at `localhost:5432` and `localhost:8123`, so those need no override. The dev stack runs on the shipped `monitor` credentials, which boot refuses unless you opt in as above, and `just run` sets it for you. Edit code → Ctrl-C → `cargo run` again.

Tear down (keeps DB volumes):

```bash
docker compose -f compose.dev.yml down
```

Wipe data too:

```bash
docker compose -f compose.dev.yml down -v
```

### Docker dev workflow (live reload inside a container)

Runs the binary inside a container that bind-mounts the repo and re-runs
`cargo run` via [`cargo-watch`](https://crates.io/crates/cargo-watch) on every
source change. The compiled `target/` and the linux Tailwind CLI live in named
volumes, so they persist across restarts and don't clash with the host build.

```bash
docker compose -f compose.dev.yml --profile dev-app up -d --build
docker compose -f compose.dev.yml logs -f uptimepage
```

First run takes ~3 min (toolchain + cargo-watch install + cold build + Tailwind
fetch). After that, edits to `src/`, `templates/`, or `static/css/input.css`
trigger an incremental rebuild + restart inside the container, typically
under 5 s.

Don't combine this with `cargo run` on the host — both bind 8080.

Stop just the app (keep pg + ch up):

```bash
docker compose -f compose.dev.yml stop uptimepage
```

### Docker prod-shape workflow (full stack via Dockerfile)

The root stack pulls the published image by default, so building from this checkout needs the build overlay:

```bash
docker compose -f docker-compose.yml -f compose.build.yml up -d --build uptimepage
```

The `Dockerfile` uses [`cargo-chef`](https://github.com/LukeMathWalker/cargo-chef)
to split dependency compile from app compile. The first build is slow; later
src-only edits skip the dep cook layer and finish in ~30 s.

If you have the host workflow running and want to switch to docker, stop the
native binary first to free port 8080 (or stop the docker service first to free
the host port).

## Verify it's up

```bash
curl http://localhost:8080/healthz   # liveness
curl http://localhost:8080/readyz    # readiness (DBs reachable)
```

Browse:

- `http://localhost:8080/` — operator dashboard
- `http://localhost:8080/status` — public status page
- `http://localhost:8080/docs` — Swagger UI

## Operator UI locally

The `dev-app` container runs the same SaaS code path as production. The
host workflow (`cargo run` against `config/default.toml`) does too — the
binary is multi-tenant SaaS in every environment; a single-tenant deploy
is just a SaaS deploy with one signed-up user.

Get an authenticated owner session without GitHub OAuth:

```bash
just up-app          # SaaS-mode stack; wait for "api listening"
just dev-login       # seeds user+org+owner+session, prints the cookie
```

Then, in the browser devtools Console at `http://localhost:8080`:

```js
document.cookie = "_sm_session=devsession-localtest-0000000000; path=/";
```

Reload — you're the owner of "Dev Org". The public page is at
`http://devorg.lvh.me:8080/` (`*.lvh.me` resolves to
`127.0.0.1`, no `/etc/hosts` edit). `just dev-login` also prints a `curl`
snippet that passes the cookie directly, for API-only checks.

After editing a migration in place (pre-launch policy), the dev DB trips
sqlx's "migration N modified" checksum guard — `just db-reset` drops and
recreates it (ClickHouse and the warm build cache are kept). `down -v` wipes
the seeded session; re-run `just dev-login`.

## Seed a target

```bash
curl -sS -X POST http://localhost:8080/api/v1/targets \
  -H 'content-type: application/json' \
  -d '{
    "name": "example",
    "check": {"type":"http","url":"https://example.com/","method":"GET",
              "timeout":10000,"follow_redirects":false,"max_redirects":0,
              "expected_status":{"kind":"exact","value":200},
              "headers":{},"verify_tls":true},
    "interval": 60, "enabled": true, "tags": []
  }'
```

A monitor reaches a public page by being added as a component of a status page, not by a flag on the monitor; see [Per-org status pages](per-org-status.md).

## Seed UI fixtures

For end-to-end UI smoke (every public-page render path, varied check_spec
kinds, notification channels, alert bindings, maintenance binding, adversarial
title) use the bulk fixture script after `just dev-login`:

```bash
just seed-fixtures
```

What it seeds (under the `seed-fixtures` tag, idempotent):

- **14 monitors** — 8 public (covering all 5 component states: Operational /
  Degraded / Partial outage / Major outage / Maintenance — plus the
  disabled-target and ungrouped render paths) and 6 internal exercising every
  `check_spec` kind (http / tcp / dns / tls_cert / domain_expiry).
- **161 incidents** — 150 resolved across 87 days (cleared the 50-incident
  cap so the "Older incidents →" archive link renders), 10 active in mixed
  phases (investigating / identified / monitoring), 1 adversarial-title
  incident covering the day-popover JSON-escape path.
- **90-day ClickHouse history** — per-target divergent shape via
  `cityHash64(tid)` (each component has a distinct uptime% and outage
  pattern), an explicit 87-89d "ancient outage" cluster on the first three
  targets, and a 6-day NoData gap on fix-email.
- **9 notification channels** — one per `ChannelConfig` variant (slack,
  webhook, whatsapp, discord, msteams, google_chat enabled; email enabled
  but unverified; telegram and telegram_app disabled), with alert bindings
  on fix-api / fix-db / fix-auth mixing `notify_recovery` on/off and
  single/multi-channel bindings.
- **4 maintenance windows** — 1 active (bound to fix-db), 2 upcoming, 1 past.

The script ends with a post-seed verification block that prints Postgres row
counts, per-component last-5-min counters with an expected-vs-actual state
matrix, an HTTP smoke against the public page, the adversarial-title escape
check, and a 90-day ASCII day-strip per component. **Exits non-zero on any
mismatch** — safe to chain in CI.

Env overrides: `SLUG=<org>` (default `devorg`), `RESET_CH=0` to skip
ClickHouse purge if you want to layer additional rows on top of a prior seed
(default `1`).

Then visit:
- Public status page: <http://devorg.lvh.me:8080/>
- Operator dashboard: <http://app.lvh.me:8080/>

## Logging

`docker-compose.yml` sets the default level to:

```
uptimepage=debug,sqlx=warn,hyper=warn,tower_http=info,info
```

For the host workflow, pass it directly:

```bash
RUST_LOG="uptimepage=debug,sqlx=warn" cargo run --bin uptimepage
```

`RUST_LOG` always wins over the config file. Anyhow errors are printed with
`{:#}` from the public-status cache, so the full context chain shows up
without re-running with backtraces.

Stream container logs:

```bash
docker compose logs -f uptimepage
```

## Faster builds

```bash
just setup        # once: cargo-nextest and the linker
                  # (mold on Linux; macOS prints an lld opt-in snippet)
cargo check --lib # the iteration gate (~8.5s after a one-file edit)
just check        # builds every test binary — pre-commit gate (~4min)
```

- **Toolchain**: `rust-toolchain.toml` pins 1.95 for *every* entrypoint
  (bare `cargo`, `just`, rust-analyzer, CI) — no more ad-hoc `cargo +1.95`.
- **Linker**: `.cargo/config.toml` selects `mold` for Linux targets, so
  `just`, bare `cargo`, and rust-analyzer share one build fingerprint (an
  env `RUSTFLAGS` that differed between them would double-build `target/`).
  A Linux build needs `mold` installed — `just setup`. macOS is opt-in
  (Apple clang needs lld's machine-specific absolute path; `just setup`
  prints the `~/.cargo/config.toml` snippet).
- **Incremental**: the local lever. Set `incremental = true` in
  `~/.cargo/config.toml` (machine-scoped). Worth ~4% on `just check`, which is
  link-bound, but 28s -> 8.5s on `cargo check --lib`. The dev-app container does
  the same via `CARGO_INCREMENTAL=1`.
- **sccache**: CI only (`mozilla-actions/sccache-action`, with
  `Swatinem/rust-cache` reduced to `cache-targets: false` so they don't
  double-store). Never set `RUSTC_WRAPPER` locally: it is mutually exclusive
  with incremental, only registry deps are cacheable (never the local crate),
  and on macOS it deadlocks the cold lib compile. Not in the release
  `Dockerfile` either — cargo-chef already layer-caches deps there and the
  sccache mount wouldn't survive CI.
- CI installs the linker via `rui314/setup-mold`; the dev-app container
  via `apk add mold`.

## Tests

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo test --release
cargo bench
```

Postgres-backed tests (e.g. `bulk_create_with_ragged_tags`) are `#[ignore]`'d
by default and no-op when `DATABASE_URL` is unset. Bring up the stack and opt
in. Validate schema/migration changes against a throwaway DB, not the stale
`monitor` one (the harness auto-applies migrations on first connect):

```bash
docker compose -f compose.dev.yml up -d
docker compose -f compose.dev.yml exec -T postgres createdb -U monitor ci_verify

# Whole ignored suite (slow — builds every test binary):
DATABASE_URL=postgres://monitor:monitor@127.0.0.1:5432/ci_verify \
  cargo test -- --ignored

# One suite (fast — scope to a binary; bare `nextest run` rebuilds +
# enumerates every integration test binary (about a hundred) and looks
# frozen for minutes):
DATABASE_URL=postgres://monitor:monitor@127.0.0.1:5432/ci_verify \
  cargo test --test status_page_settings_test -- --ignored --nocapture
```

## Database access

```bash
docker compose exec postgres psql -U monitor -d monitor
docker compose exec clickhouse clickhouse-client -u monitor --password monitor -d monitor
```

Same commands work against `compose.dev.yml`; the service names are identical.

## Web UI

The single binary serves both the `/api/v1/*` JSON surface and a
server-rendered HTML UI at `/`. Stack:

- **askama 0.16 + askama_web 0.16** — compile-time HTML templates under
  `templates/`. Type mismatches fail `cargo build`.
- **HTMX 2.0.9 + json-enc** — bundled under `static/js/`.
  Powers partial swaps (filter, paginate, delete) and JSON form submission.
  No SPA framework.
- **Tailwind CSS 4** — CSS-first config in
  `static/css/input.css` (`@source`, `@theme`,
  `@layer components`). No `tailwind.config.js`.
- **ECharts 6** — lazy-loaded from page-level `<script>` tags, only where
  charts exist (dashboard, target detail).

`build.rs` runs `./bin/tailwindcss --minify` before each `cargo build`. First
build fetches the standalone CLI (~30 MB) via `scripts/fetch-tailwind.sh`;
subsequent builds reuse it. After `cargo build --release` you have one
self-contained executable with every template, CSS byte, and vendored JS file
embedded via `rust-embed`.

### Routes

| Path | Owner |
|---|---|
| `GET /` | dashboard (auto-refreshes via HTMX every 5 s) |
| `GET /targets` | targets list + filters |
| `GET /targets/{id}` | target detail with charts and time-range nav |
| `GET /targets/new`, `/targets/{id}/edit` | forms posting JSON to `/api/v1/targets` |
| `GET /incidents`, `/incidents/{id}` | responder views; declare + update + close |
| `GET /settings/*` | notifications, pages, variables, team, usage, account |
| `GET /web/targets/list` | tbody fragment for filter/paginate swaps |
| `GET /web/partials/*` | chrome-free fragments for the polling regions |
| `GET /m/{token}` | read-only share of one monitor, sub-resources twinned under the token |
| `GET /docs` | Swagger UI generated from `/api/openapi.json` |
| `GET /static/*` | embedded assets (`css/`, `js/`, `img/`) |

Every UI mutation hits an existing `/api/v1/*` endpoint — there are no
`/web/*` write routes, which keeps the API the single source of truth and
makes a future SvelteKit port a templates-only rewrite. The `/m/{token}`
share surface is read-only and serves no write method.

The user-facing tour of these screens is [Web UI](ui.md); this section is
the implementation.

### Styling: the semantic layer

`static/css/input.css` is layered: design tokens (`@theme`, e.g.
`--color-ink`) → primitives (`.sticker-card`, `.sticker-btn`,
`.sticker-pill`) → **semantic classes** (`.page-title`, `.panel-label`,
`.kpi-value`, `.stat-tile`, `.status-badge--*`, `.btn-ghost`,
`.sticker-btn--primary/--danger`, `.nav-link`, `.day-cell`). Templates
reference **only** the semantic names — no raw colour/shape utility
clusters. State is one `--modifier` (`.status-badge--down`,
`.stat-tile--ok`). Re-skinning the internal app is then an `input.css`-only
edit with no template touched. When adding UI, reuse or extend a semantic
class rather than inlining `bg-*`/`rounded-*`/heading-scale clusters.

The public status page is deliberately exempt — it is a flat, brand-themed
surface with its own view-supplied palette (`public_status/view.rs`), not the
sticker system.

Tailwind 4 scans `templates/**/*.html` **and** `src/**/*.rs` for class names
(declared via `@source` in `input.css`), so utility classes written inside
Rust strings survive tree-shaking.

### Dashboard refresh model

Three regions, split so the polling never disturbs the charts:

1. **Chrome** (nav, page header) — rendered once.
2. **Auto-refresh region** (`<div id="dashboard-region">`) — KPI cards +
   system-health card. Polls `/web/partials/dashboard` every 5 s and swaps
   its own outer HTML so the trigger stays armed.
3. **Charts** — placed *outside* the refresh region so the ECharts
   instances persist across polls. The wrapper listens for
   `htmx:afterSettle` on the region and re-fetches
   `/api/v1/dashboard/summary` once per cycle, fanning out to both charts
   (one round-trip, not one per chart).

`dashboard_summary` caches its result in `state.dashboard_cache` for 5 s, so
polling load on Postgres + ClickHouse is bounded to one query set per 5 s
regardless of how many tabs are open.

### Credentials in forms

The monitor form has no inline credential inputs. HTTP `basic_auth` and
`bearer_token` are set through request headers referencing org secret
variables, chosen via the secret-variable auth picker
(`data-var-auth-picker`).

Stored credentials never render into the edit form. The API rejects the
`***` redaction sentinel on write; on PATCH an omitted credential keeps the
stored value, an empty value clears it, and a real value replaces it.
`tests/web_e2e_test.rs::edit_form_renders_existing_target_without_leaking_credentials`
pins this.

Flow monitors are the exception: a `fill` step's value renders in the edit
form (an authenticated, owner-only surface) so a login script can be
adjusted without re-typing, and is masked everywhere else — the detail
config panel and API through `redact_check`, the public share view through
`redact_check_for_public`. Referencing an org secret as `{{name}}` keeps the
literal out of the stored config entirely.

### Adding a new page

1. Add a template under `templates/` (extend `base.html`).
2. Add a `#[derive(Template, WebTemplate)]` struct and handler in
   `src/web/views/`.
3. Register the route in `src/web/routes.rs`.
4. Tailwind picks up new utility classes automatically via the
   `@source "../../templates/**/*.html"` directive.
5. Add a render test next to the view, and a case in
   `tests/web_e2e_test.rs` if the route is worth covering end to end.

### Migrating to a SPA later

Every `templates/*.html` maps one-to-one onto a component, every chart
module under `static/js/charts/` is already a pure
`(element, endpoint) → disposer` function, and there are zero `/web/*`
write endpoints to refactor. To swap frameworks: generate a typed client
from `/api/openapi.json`, port the templates page by page keeping
`/api/v1/*` unchanged, drop `src/web/views/` (keeping `src/web/assets.rs`
pointed at the new bundle), and delete `templates/` plus
`static/js/{htmx,json-enc,ui}`. The backend stays untouched.

### UI tests

- **Unit (render):** every view in `src/web/views/` ships a `#[test]` that
  renders the template with a fixtures struct and asserts on the output
  (presence of the HTMX hooks, redaction sentinels, table scaffolding).
- **End-to-end:** `tests/web_e2e_test.rs` drives
  the merged API+web router via `tower::ServiceExt::oneshot`, covering
  dashboard / list / detail / forms / 404 paths and verifying credential
  redaction never leaks real values into HTML.

```bash
cargo test --lib web::          # unit render tests
cargo test --test web_e2e_test  # e2e
```

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `503 STATUS_DATA_UNAVAILABLE` | Aggregator's first compute failed. Check `uptimepage::public_status::cache` ERROR log for the actual SQL/CH error. |
| `failed to spawn ./bin/tailwindcss` during `cargo build` | First-build fetch failed. Run `bash scripts/fetch-tailwind.sh` and confirm `bin/tailwindcss` is executable. |
| Page renders unstyled HTML | `static/css/app.css` empty or stale. Touch `static/css/input.css` and rebuild. |
| Charts render blank | A fetch to `/api/v1/dashboard/summary` or `/api/v1/targets/{id}/results` failed; the chart module logs `chart load failed` with the URL and status. |
| Dashboard never refreshes | `<script defer src="/static/js/htmx.min.js">` missing from the page source. It is loaded from `base.html`. |
| Edit form submitted credentials despite the toggle being off | Console error from `auth_field.js`. The submit handler reads `data-mode` off the credential `<fieldset>`; without those data attributes it falls back to "include". |
| `docker compose up --build` rebuilds nothing | The root stack has no build stanza; add `-f compose.build.yml`. Once built, that local tag is reused, so `docker compose pull` is needed to go back to the published image. |
| Native `cargo run` fails with `Connection refused` | `compose.dev.yml` isn't up, or you forgot to release port 8080 from a running container. |
