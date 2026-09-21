# Common dev workflows. Install: `brew install just` or `cargo install just`.
# Run `just` (no args) to list recipes.

set shell := ["bash", "-cu"]

# The linker lives in .cargo/config.toml (read by `just` AND bare `cargo` AND
# rust-analyzer — one shared build fingerprint, no thrash). No RUSTC_WRAPPER
# here: sccache deadlocks the cold lib compile on macOS and can't cache a path
# dependency anyway. CI sets it itself (ci.yml), where it does pay off.

# Default = list recipes.
default:
    @just --list

# Install the build accelerators: cargo-nextest and the fast linker (mold on
# Linux, lld on macOS). Idempotent. CI installs sccache via sccache-action.
setup:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v cargo-nextest >/dev/null 2>&1 || cargo install --locked cargo-nextest
    if [ "{{os()}}" = "macos" ]; then
      brew list lld >/dev/null 2>&1 || brew install lld
      cat <<'NOTE'
    macOS lld is opt-in (its brew path is machine-specific, so it can't be
    committed to .cargo/config.toml). For faster local links add to
    ~/.cargo/config.toml:
      [target.aarch64-apple-darwin]
      rustflags = ["-Clink-arg=-fuse-ld=$(brew --prefix lld)/bin/ld64.lld"]
    (substitute the real path; both cargo and rust-analyzer then share it.)
    NOTE
    elif command -v mold >/dev/null 2>&1; then
      :
    elif command -v apt-get >/dev/null 2>&1; then
      sudo apt-get update -q && sudo apt-get install -y mold
    else
      echo "install 'mold' via your package manager (.cargo/config.toml needs it on Linux)"
    fi
    git config core.hooksPath .githooks
    echo "pre-commit installed → .githooks/pre-commit (bypass with --no-verify)"
    echo "setup done — linker via .cargo/config.toml"

# Reclaim disk: sweep build artifacts not accessed today (cargo-sweep).
clean:
    command -v cargo-sweep >/dev/null 2>&1 || cargo install --locked cargo-sweep
    cargo sweep --time 0

# ── Local stack ─────────────────────────────────────────────────────────────

# Bring up postgres + clickhouse only. `cargo run` natively against them.
up:
    docker compose -f compose.dev.yml up -d
    @echo "pg + ch up. run: cargo run --bin uptimepage"

# Bring up the full dev stack incl. uptimepage with live reload.
up-app:
    docker compose -f compose.dev.yml --profile dev-app up -d --build

# Stop everything, keep volumes.
down:
    docker compose -f compose.dev.yml --profile dev-app down

# Stop + wipe DB volumes.
down-clean:
    docker compose -f compose.dev.yml --profile dev-app down -v

# Tail uptimepage logs (works for either dev-app or full docker-compose).
logs:
    docker compose -f compose.dev.yml logs -f uptimepage

# Stand up three real regional agents (regions eu-helsinki, apac-sg, us-east;
# eu-helsinki doubles as the control plane's co-located home region) against the
# running control plane: mints region + agent tokens via the operator API, then
# starts the agent containers.
# Needs the dev stack up (`just up-app` or native `just run`). Idempotent.
dev-regions:
    bash scripts/dev-regions.sh up

# Stop the regional agents, delete their agent + region rows, forget tokens.
dev-regions-down:
    bash scripts/dev-regions.sh down

# Tail the regional agent logs.
dev-regions-logs:
    docker compose -f compose.dev.yml -f compose.dev.agents.yml logs -f agent-eu agent-apac agent-us

# ── Build / run ─────────────────────────────────────────────────────────────

# Re-bundle assets/js on every change into the running dev server. Debug uses
# stable filenames + a per-request fingerprint, so a browser reload picks up JS
# edits with no cargo rebuild or restart. Run in a second terminal next to
# `just run`.
watch-js:
    bash scripts/build-js.sh watch

# Native run against `just up`. Debug-level by default for local dev;
# export RUST_LOG to override. Mirrors the dev-app container's filter so
# native and in-container logs match.
run:
    UPTIMEPAGE_STORAGE__ALLOW_DEFAULT_CREDENTIALS=true \
    UPTIMEPAGE_EMAIL__FROM_ADDRESS="${UPTIMEPAGE_EMAIL__FROM_ADDRESS:-hello@example.invalid}" \
    RUST_LOG="${RUST_LOG:-uptimepage=debug,sqlx=warn,hyper=warn,tower_http=info,info}" \
        cargo run --bin uptimepage

# Native run in dashboard mode (brain-only, no in-process probing) — mirrors
# prod. Pair with `just dev-regions` so a real agent covers eu-helsinki;
# otherwise nothing probes. (The dev-app container already runs this mode.)
run-dashboard:
    UPTIMEPAGE_STORAGE__ALLOW_DEFAULT_CREDENTIALS=true \
    UPTIMEPAGE_EMAIL__FROM_ADDRESS="${UPTIMEPAGE_EMAIL__FROM_ADDRESS:-hello@example.invalid}" \
    UPTIMEPAGE_SCHEDULER__ENABLED=false \
    UPTIMEPAGE_SCHEDULER__REGION=eu-helsinki \
    UPTIMEPAGE_SCHEDULER__DEFAULT_REGION=eu-helsinki \
    RUST_LOG="${RUST_LOG:-uptimepage=debug,sqlx=warn,hyper=warn,tower_http=info,info}" \
        cargo run --bin uptimepage

# Turnkey local browser-flow testing: native SaaS-mode run with the flow engine
# on and in-process probing, so this one process serves the API AND runs flow
# monitors. Needs `just up` and a Lightpanda binary — set LIGHTPANDA_BIN or drop
# it at ./lightpanda (github.com/lightpanda-io/browser/releases). Then, in
# another shell: `just dev-login` then `bash scripts/flow-create.sh`.
flow-dev:
    #!/usr/bin/env bash
    set -euo pipefail
    bin="${LIGHTPANDA_BIN:-./lightpanda}"
    if [ ! -x "$bin" ]; then
      echo "Lightpanda binary not found or not executable at: $bin" >&2
      echo "Download from github.com/lightpanda-io/browser/releases, chmod +x," >&2
      echo "then set LIGHTPANDA_BIN=/abs/path or drop it at ./lightpanda." >&2
      exit 1
    fi
    UPTIMEPAGE_STORAGE__ALLOW_DEFAULT_CREDENTIALS=true \
    UPTIMEPAGE_EMAIL__FROM_ADDRESS="${UPTIMEPAGE_EMAIL__FROM_ADDRESS:-hello@example.invalid}" \
    UPTIMEPAGE_TENANCY__PATH_BASED_PUBLIC_ROUTES=false \
    UPTIMEPAGE_TENANCY__SUBDOMAIN_PUBLIC_ROUTES=true \
    UPTIMEPAGE_PUBLIC_STATUS__BASE_DOMAIN=lvh.me \
    UPTIMEPAGE_AUTH__FINGERPRINT_SALT=dev-only-fingerprint-salt-not-for-prod \
    UPTIMEPAGE_AUTH__SESSION__COOKIE_SECURE=false \
    UPTIMEPAGE_SERVER__API_BIND=127.0.0.1:8080 \
    UPTIMEPAGE_SCHEDULER__ENABLED=true \
    UPTIMEPAGE_SCHEDULER__REGION=eu-helsinki \
    UPTIMEPAGE_SCHEDULER__DEFAULT_REGION=eu-helsinki \
    UPTIMEPAGE_FLOW__ENABLED=true \
    UPTIMEPAGE_FLOW__LIGHTPANDA_PATH="$bin" \
    RUST_LOG="${RUST_LOG:-uptimepage=debug,sqlx=warn,hyper=warn,tower_http=info,info}" \
        cargo run --bin uptimepage

build:
    cargo build --release --bins

# Compile gate — use instead of `cargo check`. `nextest run --no-run` builds
# the test-profile artifacts, so the follow-up `just test` reuses them with
# zero rebuild; `cargo check`'s metadata-only output does not satisfy a test
# build, forcing a full recompile on the next `cargo test`.
check:
    cargo nextest run --workspace --no-run

# Pre-push DB gate: the #[ignore] PG+CH integration tests against a FRESH
# ci_verify database, so a new migration is proven against the schema prod will
# actually apply it to (a stale dev DB hides fresh-schema breaks). Needs the
# dev stack up (`just up`). Classic runner — streams output and skips nextest's build-all
# enumeration stall. ClickHouse defaults to the dev monitor db (tests scope by
# org/target uuids, so the shared volume is fine).
check-db:
    docker exec -i uptimepage-postgres-1 psql -U monitor -d postgres -c "DROP DATABASE IF EXISTS ci_verify WITH (FORCE)"
    docker exec -i uptimepage-postgres-1 psql -U monitor -d postgres -c "CREATE DATABASE ci_verify"
    DATABASE_URL='postgres://monitor:monitor@127.0.0.1:5432/ci_verify' \
    CLICKHOUSE_URL='http://127.0.0.1:8123' \
        cargo test --workspace -- --ignored

# Seed an authenticated owner session (SaaS-mode dev). Prints the cookie +
# a curl snippet. Idempotent; needs the stack up.
dev-login:
    bash scripts/seed-dev-session.sh

# Native run with every OAuth provider switched on, so all four sign-in
# buttons and all four "add" buttons render. The credentials are placeholders:
# the dance starts and mints state, then the provider rejects the client_id —
# enough to exercise our half without registering four real apps. Email stays
# "log", so the "sign-in method added/removed" mails print here.
#
# Mirrors compose.dev.yml's auth env, so it shares the dev stack's session
# cookies and fingerprint salt (the boot guard refuses a different one).
# Needs port 8080: `docker compose -f compose.dev.yml stop uptimepage` first.
# Then `just dev-login` and `just dev-sign-in-methods` in another shell.
run-oauth:
    UPTIMEPAGE_DEV_EMAIL_LINKS=1 \
    UPTIMEPAGE_STORAGE__ALLOW_DEFAULT_CREDENTIALS=true \
    UPTIMEPAGE_EMAIL__FROM_ADDRESS="${UPTIMEPAGE_EMAIL__FROM_ADDRESS:-hello@example.invalid}" \
    UPTIMEPAGE_AUTH__FINGERPRINT_SALT="dev-only-fingerprint-salt-not-for-prod" \
    UPTIMEPAGE_AUTH__SESSION__COOKIE_SECURE=false \
    UPTIMEPAGE_TENANCY__PATH_BASED_PUBLIC_ROUTES=false \
    UPTIMEPAGE_TENANCY__SUBDOMAIN_PUBLIC_ROUTES=true \
    UPTIMEPAGE_PUBLIC_STATUS__BASE_DOMAIN=lvh.me \
    UPTIMEPAGE_AUTH__GITHUB__CLIENT_ID=dev UPTIMEPAGE_AUTH__GITHUB__CLIENT_SECRET=dev \
    UPTIMEPAGE_AUTH__GITHUB__REDIRECT_URL=http://app.lvh.me:8080/auth/github/callback \
    UPTIMEPAGE_AUTH__GOOGLE__CLIENT_ID=dev UPTIMEPAGE_AUTH__GOOGLE__CLIENT_SECRET=dev \
    UPTIMEPAGE_AUTH__GOOGLE__REDIRECT_URL=http://app.lvh.me:8080/auth/google/callback \
    UPTIMEPAGE_AUTH__MICROSOFT__CLIENT_ID=dev UPTIMEPAGE_AUTH__MICROSOFT__CLIENT_SECRET=dev \
    UPTIMEPAGE_AUTH__MICROSOFT__REDIRECT_URL=http://app.lvh.me:8080/auth/microsoft/callback \
    UPTIMEPAGE_AUTH__GITLAB__CLIENT_ID=dev UPTIMEPAGE_AUTH__GITLAB__CLIENT_SECRET=dev \
    UPTIMEPAGE_AUTH__GITLAB__REDIRECT_URL=http://app.lvh.me:8080/auth/gitlab/callback \
    RUST_LOG="${RUST_LOG:-uptimepage=debug,sqlx=warn,hyper=warn,tower_http=info,info}" \
        cargo run --bin uptimepage

# Native run for the passkey ceremonies. Two constraints fight each other
# locally, and `localhost` is the only address that satisfies both:
#   - WebAuthn needs a secure context, so plain http is only allowed on
#     localhost. An http host with a real name (app.lvh.me) has no
#     navigator.credentials at all and the button never appears.
#   - the relying-party id must be a valid DOMAIN, so 127.0.0.1 is rejected
#     however it is reached.
# `api_bind` is therefore widened to dual-stack here: the default binds IPv4
# only, and a browser resolving `localhost` tries ::1 first and gives up.
#
# Emailed links print in full here (UPTIMEPAGE_DEV_EMAIL_LINKS), because the
# `log` provider otherwise truncates the token and the DB only keeps its hash,
# which leaves no way to finish a magic-link or invitation flow by hand.
#
# Open http://localhost:8080/login
run-passkeys:
    UPTIMEPAGE_DEV_EMAIL_LINKS=1 \
    UPTIMEPAGE_STORAGE__ALLOW_DEFAULT_CREDENTIALS=true \
    UPTIMEPAGE_EMAIL__FROM_ADDRESS="${UPTIMEPAGE_EMAIL__FROM_ADDRESS:-hello@example.invalid}" \
    UPTIMEPAGE_AUTH__FINGERPRINT_SALT="dev-only-fingerprint-salt-not-for-prod" \
    UPTIMEPAGE_AUTH__SESSION__COOKIE_SECURE=false \
    UPTIMEPAGE_SERVER__API_BIND="[::]:8080" \
    UPTIMEPAGE_AUTH__PUBLIC_BASE_URL=http://localhost:8080 \
    UPTIMEPAGE_AUTH__GITHUB__CLIENT_ID=dev UPTIMEPAGE_AUTH__GITHUB__CLIENT_SECRET=dev \
    UPTIMEPAGE_AUTH__GITHUB__REDIRECT_URL=http://localhost:8080/auth/github/callback \
    RUST_LOG="${RUST_LOG:-uptimepage=debug,sqlx=warn,hyper=warn,tower_http=info,info}" \
        cargo run --bin uptimepage

# Creates everything the plan allows through the API, then inserts the overflow
# directly — a create that would put an account over its cap is refused by
# design, so the over-cap state has to be made the way a shrinking plan leaves
# it. The account's plan is never touched, because it is cached for 300s and any
# plan juggling races that cache.
#
# Needs `just dev-login` first. Nothing is ever deleted by a hold.
#
# Put the dev account over its plan so the hold surfaces have something to show.
seed-holds:
    bash scripts/seed-dev-holds.sh

# Re-run just the reconcile, for when the plan cache had not turned over yet.
seed-holds-reconcile:
    RECONCILE_ONLY=1 bash scripts/seed-dev-holds.sh

# Delete what `seed-holds` made, then seed it again from scratch.
seed-holds-reset:
    RESET=1 bash scripts/seed-dev-holds.sh

# Give the dev operator three linked sign-in methods and a credential trail,
# so /settings/account has something to show. Needs `just dev-login` first.
dev-sign-in-methods:
    bash scripts/seed-sign-in-methods.sh

# Seed a substantial fixture set: 14 monitors (8 public + 6 internal with
# varied check_spec) + 161 incidents (150 resolved across 87d, 10 active in
# mixed phases, 1 adversarial-title) + 90d ClickHouse history (per-target
# divergent shape, ancient 87-89d outage cluster, 6-day NoData gap on
# fix-email) + 3 notification channels + alert bindings + an active
# maintenance window bound to fix-db. Drives all 5 public component states
# (Operational / Degraded / Partial / Major / Maintenance) plus the
# disabled-target and ungrouped render paths. Idempotent: tagged rows
# wiped before re-insert; CH purged when RESET_CH=1 (default). Ends with a
# post-seed verification block — exits non-zero on any expected-vs-actual
# mismatch. Requires `just dev-login` first so the org exists.
seed-fixtures:
    bash scripts/seed-fixtures.sh

# Seed two monitors for eyeballing the detail latency + breakdown charts:
# `lat-demo` (dense 30d, phase-rich, latency ramp + p95/p99 spikes) and
# `lat-demo-short` (~30min of data — the "new monitor, data < smallest range"
# case). Switching range must re-scale the x-axis and reshape the series.
# Idempotent: tagged rows wiped (PG + CH) before re-insert. Needs `just
# dev-login` first, and a clean CH (`just down-clean && just up-app`) so the
# rollup carries the per-phase columns.
seed-latency-demo:
    bash scripts/seed-latency-demo.sh

# Seed six heartbeat monitors covering every state the ping card renders: a
# healthy one, both cadence-advice cases, a job that reported failure with its
# own output, an open run past its max runtime, and one with too little history
# to advise on. Tokens are minted by the running app, so this needs `just
# up-app` + `just dev-login` first. Idempotent: tagged rows wiped (PG + CH).
seed-heartbeats:
    bash scripts/seed-heartbeats.sh

# Seed a monitor that fails ~1 check in 5 from every region, always in single
# checks, plus a clean twin to compare it against. No incident ever opens, so
# this is the state where uptime falls and the incidents tab stays empty.
# Stop the dev-region agents first. Idempotent: tagged rows wiped (PG + CH).
seed-flapping:
    bash scripts/seed-flapping.sh

# Seed a photogenic operator tenant (40 monitors, 90d up history, resolved
# incidents) for shooting the marketing screenshot gallery. Stop the dev-region
# agents first; see the script header for the shoot-time stale-window override.
seed-marketing:
    bash scripts/seed-marketing.sh

# Reset the dev Postgres DB (keeps ClickHouse + the warm build cache).
# Use after editing a migration — pre-launch policy edits migrations in
# place, which trips sqlx's "migration N modified" checksum guard.
db-reset:
    docker compose -f compose.dev.yml exec -T postgres \
        psql -U monitor -d postgres \
        -c "DROP DATABASE IF EXISTS monitor WITH (FORCE);" \
        -c "CREATE DATABASE monitor OWNER monitor;"
    docker compose -f compose.dev.yml restart uptimepage 2>/dev/null || true
    @echo "DB reset. App reconnects + re-applies migrations on a fresh schema."

# ── Tests ───────────────────────────────────────────────────────────────────

# Fast: unit + non-network integration tests, no external services needed.
test:
    cargo test

# All tests including pg- and ch-backed ones. Requires `just up` first.
test-all:
    DATABASE_URL=postgres://monitor:monitor@127.0.0.1:5432/monitor \
    CLICKHOUSE_URL=http://127.0.0.1:8123 \
        cargo test -- --include-ignored

# Just the CH aggregator integration tests.
test-ch:
    DATABASE_URL=postgres://monitor:monitor@127.0.0.1:5432/monitor \
    CLICKHOUSE_URL=http://127.0.0.1:8123 \
        cargo test --test clickhouse_aggregator_test -- --ignored

# One integration-test binary, scoped compile; a bare `-E` still builds all ~48.
test-one BIN *ARGS:
    cargo nextest run --test {{BIN}} {{ARGS}}

# Same against the dev stack, #[ignore] PG+CH tests enabled (`just up` first).
test-one-db BIN *ARGS:
    DATABASE_URL=postgres://monitor:monitor@127.0.0.1:5432/monitor \
    CLICKHOUSE_URL=http://127.0.0.1:8123 \
        cargo nextest run --test {{BIN}} --run-ignored all {{ARGS}}

# ── Benchmarks ──────────────────────────────────────────────────────────────

# Uses a throwaway `ci_verify` DB so the dev DB is untouched (harness
# auto-applies migrations on a fresh schema). Needs `just up`. Skips without
# DATABASE_URL so plain `cargo bench`/CI never runs it.
# DB-backed status-page perf benches — run before a release to catch schema/perf drift.
bench-db:
    docker compose -f compose.dev.yml exec -T postgres \
        psql -U monitor -d postgres \
        -c "DROP DATABASE IF EXISTS ci_verify WITH (FORCE);" \
        -c "CREATE DATABASE ci_verify OWNER monitor;"
    docker compose -f compose.dev.yml exec -T clickhouse \
        clickhouse-client -u monitor --password monitor \
        --query "DROP DATABASE IF EXISTS ci_verify"
    docker compose -f compose.dev.yml exec -T clickhouse \
        clickhouse-client -u monitor --password monitor \
        --query "CREATE DATABASE ci_verify"
    DATABASE_URL=postgres://monitor:monitor@127.0.0.1:5432/ci_verify \
    CLICKHOUSE_URL=http://127.0.0.1:8123 \
    CLICKHOUSE_DATABASE=ci_verify \
        cargo bench --bench public_status_ttfb --bench public_status_concurrent

# ── Lints ───────────────────────────────────────────────────────────────────

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --all-targets -- -D warnings

# Run everything CI runs.
ci: fmt-check clippy test

# Point git at the in-repo hook directory (.githooks/) for this clone.
# Runs cargo fmt --check + scripts/check_tenant_isolation.sh on every commit.
install-hooks:
    git config core.hooksPath .githooks
    @echo "pre-commit installed → .githooks/pre-commit (bypass with --no-verify)"

# ── Database probes ─────────────────────────────────────────────────────────

psql:
    docker compose -f compose.dev.yml exec postgres psql -U monitor -d monitor

clickhouse:
    docker compose -f compose.dev.yml exec clickhouse \
        clickhouse-client -u monitor --password monitor -d monitor

# ── Smoke ───────────────────────────────────────────────────────────────────

# Quick check that the public surface is alive on localhost:8080.
smoke:
    @echo "health:"   ; curl -sS -o /dev/null -w "  %{http_code}\n" http://localhost:8080/healthz
    @echo "ready:"    ; curl -sS -o /dev/null -w "  %{http_code}\n" http://localhost:8080/readyz
    @echo "status:"   ; curl -sS -o /dev/null -w "  %{http_code}\n" http://localhost:8080/api/public/v1/status
    @echo "badge:"    ; curl -sS -o /dev/null -w "  %{http_code}\n" http://localhost:8080/api/public/v1/badge.svg
    @echo "rss:"      ; curl -sS -o /dev/null -w "  %{http_code}\n" http://localhost:8080/api/public/v1/incidents.rss
    @echo "html:"     ; curl -sS -o /dev/null -w "  %{http_code}\n" http://localhost:8080/status

# Smoke-test the operator surface (regions + agents): self-cleaning, asserts
# every status code. Needs the app running with UPTIMEPAGE_OPERATOR__ADMIN_TOKEN
# set; pass the same value as the arg or via OPERATOR_TOKEN.
#   just smoke-operator <admin-token>
smoke-operator token=env_var_or_default("OPERATOR_TOKEN", ""):
    OPERATOR_TOKEN={{token}} bash scripts/smoke-operator.sh
