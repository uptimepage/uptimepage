# Multi-tenancy

uptimepage runs as a multi-tenant SaaS from a single binary. The active org is always resolved from the authenticated session; there is no compile-time "self-host vs SaaS" mode and no ambient default org.

A single-tenant deployment is just a SaaS deployment where you sign up as the first user — the OAuth callback creates the user, an auto-provisioned org and the owner membership in one transaction. Teams who would rather skip the OAuth round-trip can seed `users` + `organizations` + `memberships` directly with a one-shot SQL script.

## The org model

Three tables form the access-control core:

```
organizations ── memberships ── users
                     │
                     └── role: 'owner' | 'member'
```

Every tenant-scoped table (`targets`, `incidents`, `incident_updates`, `maintenance_windows`, `maintenance_window_components`, `notification_channels`, …) carries `org_id NOT NULL` and an `ON DELETE CASCADE` foreign key to `organizations`. ClickHouse `check_results` and `check_results_1m` are ordered by `(org_id, target_id, region, timestamp)` so single-org queries never full-scan the table.

### Slugs

Org slugs are case-insensitive (`CITEXT`), 3–30 characters, must start with a lowercase letter, and otherwise contain `[a-z0-9-]` only — no leading or trailing hyphen and no consecutive hyphens. A static reserved list (`api`, `admin`, `login`, …) is rejected at creation.

The placeholder slug a brand-new user's first org gets at signup takes the shape `{adj}-{noun}-{6char}` from inline word lists in `src/domain/word_lists.rs`. The signup transaction returns `Ok(None)` on a slug collision so the caller wraps the generate-and-insert pair in a 5-attempt retry loop; the birthday-paradox tail above 5 retries is astronomically small. Users typically rename the slug after signup from settings; the org's default status page is created with the same slug, which the owner can change independently in the page editor.

### Accounts own orgs, and the plan sits on the account

Every org belongs to an **account** (`organizations.account_id`), and the plan lives there (`accounts.plan_id`). One account per user today, opened at signup — or lazily, the first time a user who only ever joined someone else's org creates one of their own.

The account is what a quota is counted against. Its orgs share one pool of monitors, status pages, seats, channels and every other cap; a second org is a workspace, not a second allowance. `plans.max_orgs` (free 1, founding 3, pro 5, team 10) bounds how many workspaces the pool is split across. The count runs under a per-account advisory lock so two concurrent creates cannot both win, and soft-deleted orgs do not count — which is why restoring one re-checks the cap. Invited memberships (role `member`) are unlimited and carry no capacity: a member brings access to the org they joined, never their own account's quota.

An unexpired `plan_overrides` row (keyed by account) can raise `max_orgs` and the other caps for one customer; the writer and the usage view read the same folded number.

## Soft delete and the 30-day purge

Deletion is two-phase to give operators a recovery window and to keep ClickHouse rows out of forever-orphan state.

1. **Soft delete.** `DELETE /api/v1/orgs/{id}` flips `organizations.deleted_at = now()`. The org disappears from the user's switcher and every URL referencing it returns 404 — `is_active_member` short-circuits on `deleted_at IS NULL`.
2. **Restore window.** The original deleter can call `POST /api/v1/orgs/{id}/restore` within `deletion_grace_period_days` (default 30). The slug stays held so nobody can squat it and leave the org unrestorable: `idx_organizations_active` is partial and does not see tombstones, so create and rename both test the slug against *every* row rather than relying on the index alone. The hold therefore ends at purge, not at the window — a slug can stay taken for up to a day after restore starts refusing, and longer if the purge batch is backed up.
3. **Purge.** A daily job (`src/jobs/retention.rs`) runs at 03:00 UTC. It first runs the soft-delete purge (`src/jobs/purge_deleted.rs::purge_tick`):
   - Selects up to 10 orgs whose `deleted_at` is past the grace window.
   - **Per org, in one PG transaction:** insert into `clickhouse_purge_queue` (idempotent via `ON CONFLICT (org_id) DO NOTHING`), then `DELETE FROM organizations` — `ON DELETE CASCADE` empties every tenant table.
   - Drains pending queue rows by issuing `ALTER TABLE check_results DELETE WHERE org_id = ?` against ClickHouse for each. The mutation is idempotent; a process restart between halves replays cleanly.
   - Then hard-deletes up to 10 soft-deleted users past the grace window that hold no live (unexpired, unused) recovery token. The `users` `ON DELETE CASCADE` erases memberships, oauth_identities, webauthn_credentials, api_tokens, invitations, sessions and recovery tokens; rows referencing the user as an actor (`login_attempts`, `org_audit_log`, `quota_events`, `plan_overrides`) are kept with the actor nulled.

The same daily job then enforces long-horizon data retention from the `[retention]` config: it deletes `login_attempts`, `quota_events` and `org_audit_log` rows past their windows and reaps sessions that are absolute-expired **or** idle past `auth.session.idle_timeout_days`. ClickHouse `check_results` retention is the table's own `TTL` (background merge), but the window is a per-row `ttl_days` stamped from the writing org's plan (`raw_days`, 30 by default) at insert time, not a single table-wide constant. Short-cadence security sweeps (OAuth-state, magic-link) keep their own faster loops — their frequency is the property.

The outbox table is the load-bearing piece. A naive "DELETE in PG, then DELETE in CH" sequence leaves CH rows orphaned if the worker dies between calls — invisible to queries but on disk forever, breaking the "data fully erased within 30 days" privacy claim.

## Per-org caches

`AppState` keeps tenant-derived caches keyed by `OrgId` so one tenant's data cannot leak into another's response:

| Cache | Type | TTL |
|---|---|---|
| `dashboard_cache` | `moka::sync::Cache<OrgId, Arc<DashboardSummary>>` | 5 s |
| `public_status::cache::PageCache` | `moka::future::Cache<StatusPageId, Arc<PageData>>` | 10 s |
| `PageCache::last_good` | `moka::sync::Cache<StatusPageId, Arc<PageData>>` | retained across `inner`'s TTL eviction for stale-fallback |

The public-page caches are keyed by `StatusPageId`, not `OrgId`: an org can run several pages, each rendering a different subset of monitors, so the cache unit is the page. The underlying aggregator query still binds the org id, so a page only ever sees its own org's data. `PageCache::get_or_compute` does per-page single-flight via `moka`'s `try_get_with`, so a thundering herd against one page doesn't fan out into N expensive aggregator builds.

## Public status routes gating

Public-status routing has two shapes, gated by `tenancy.path_based_public_routes` and `tenancy.subdomain_public_routes`. Path-based routing (`/status`, `/api/public/v1/*` on the operator host, scoped to the single live org) is the default and is correct only for a single-tenant deploy. Multi-tenant deployments **must** flip to subdomain routing (`{slug}.{base_domain}`) — otherwise every visitor sees the lone org's data regardless of which slug they expected. The binary panics at boot on the dangerous combinations (subdomain routes with an empty `base_domain`, or a `cookie_domain` that overlaps the status wildcard); see [Public status routing](configuration.md#public-status-routing) for the full flag matrix.

## Tenant-isolation invariants

These are checked in CI:

- Every runtime SQL statement against a tenant table must include `org_id` in its `WHERE` clause. Enforced by `scripts/check_tenant_isolation.sh` via an `ast-grep` rule. The only allow-listed call sites are `src/storage/admin.rs` (`AdminRepo`, cross-tenant by design) and `src/storage/orgs.rs` (operates on the `organizations` table itself), plus `src/jobs/purge_deleted.rs` (drains soft-deleted orgs and users across tenants).
- Every ClickHouse `SELECT … WHERE target_id = …` must have a sibling `org_id = ?` term. Enforced by `scripts/check_clickhouse_org_scope.sh`.
- A Postgres trigger on every child table (`incident_updates`, `maintenance_window_components`) raises on `org_id` mismatch between child and parent rows.
- An integration test (`tests/tenant_isolation_test.rs`) provisions two orgs and asserts every per-org store backed by Postgres or ClickHouse only sees its own org's rows.

If you add a new tenant-scoped table or a new repository, make sure both ast-grep rules cover it before merge.

## Org-management API

See [REST API](api.md) for full schemas. The catalogue:

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/api/v1/orgs` | Create org (slug, name) — caller becomes owner |
| `GET` | `/api/v1/orgs` | List orgs the caller is a member of |
| `GET` | `/api/v1/orgs/{id}` | Get one org (member-only) |
| `PATCH` | `/api/v1/orgs/{id}` | Edit org (owner-only) |
| `DELETE` | `/api/v1/orgs/{id}` | Soft-delete (owner-only; 422 on the caller's last org) |
| `POST` | `/api/v1/orgs/{id}/restore` | Restore within the grace window (only by the deleter) |
| `GET` | `/api/v1/orgs/check-slug?slug=…` | Slug availability for signup forms |
| `GET` | `/api/v1/orgs/{id}/members` | List members (owner-only) |
| `DELETE` | `/api/v1/orgs/{id}/members/{user_id}` | Remove a member (owner-only; refuses the last owner and the owner of the account the org bills to) |
| `PATCH` | `/api/v1/orgs/{id}/members/{user_id}` | Change a member's role (owner-only; refuses to demote the last owner and the owner of the account the org bills to) |
| `POST` | `/api/v1/me/active-org` | Switch the session's active org |
| `GET` | `/api/v1/me/orgs` | Active (non-deleted) orgs |
| `GET` | `/api/v1/me/deleted-orgs` | Soft-deleted orgs you deleted (restore UI) |
