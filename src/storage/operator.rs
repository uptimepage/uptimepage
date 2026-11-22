//! Operator (instance-admin) data access: regions and agents. Cross-tenant by
//! nature and reached only through the static-secret `/operator` surface, so
//! none of these methods take an `OrgId` — they are deliberately global.

use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::agent_wire::HeldConfig;
use crate::error::Result;
use crate::security::{agent_token, token_hash};

#[derive(Debug, sqlx::FromRow)]
pub struct RegionRow {
    pub id: String,
    pub name: String,
    pub city: Option<String>,
    pub country_code: Option<String>,
    pub continent: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub enabled: bool,
    pub default_selected: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct AgentRow {
    pub id: Uuid,
    pub region: String,
    pub name: String,
    pub enabled: bool,
    pub token_prefix: String,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// When the agent was first seen without a config. `None` once a pull
    /// showed one and the agent has been in touch since.
    pub config_missing_since: Option<DateTime<Utc>>,
    /// Monitors the agent last said it holds.
    pub scheduled_monitors: Option<i32>,
    /// As the last config pull reported it; `None` when that pull sent none
    /// or a value outside the wire rule.
    pub build: Option<String>,
}

impl AgentRow {
    /// Checked in within `window`. One never seen ages from its creation, so a
    /// new agent does not read as down before it first boots.
    pub fn is_fresh(&self, now: DateTime<Utc>, window: Duration) -> bool {
        within(now, self.last_seen_at.unwrap_or(self.created_at), window)
    }

    /// Enabled, in touch within `window`, and has shown a config since.
    pub fn holds_config(&self, now: DateTime<Utc>, window: Duration) -> bool {
        self.connected(now, window) && self.config_missing_since.is_none()
    }

    /// Holds a config, or has been without one for at most `window`, so a
    /// restart does not read as an outage. One never seen has been without one
    /// since its creation, so a new agent gets the same grace.
    pub fn is_probing(&self, now: DateTime<Utc>, window: Duration) -> bool {
        self.holds_config(now, window) || self.in_grace(now, window)
    }

    fn in_grace(&self, now: DateTime<Utc>, window: Duration) -> bool {
        let missing_since = match self.last_seen_at {
            None => Some(self.created_at),
            Some(_) => self.config_missing_since,
        };
        self.enabled
            && self.is_fresh(now, window)
            && missing_since.is_some_and(|since| within(now, since, window))
    }

    fn connected(&self, now: DateTime<Utc>, window: Duration) -> bool {
        self.enabled
            && self
                .last_seen_at
                .is_some_and(|seen| within(now, seen, window))
    }
}

fn age_secs(now: DateTime<Utc>, then: DateTime<Utc>) -> u64 {
    (now - then).num_seconds().max(0) as u64
}

fn within(now: DateTime<Utc>, then: DateTime<Utc>, window: Duration) -> bool {
    age_secs(now, then) <= window.as_secs()
}

/// Outcome of deleting a region: a region still referenced by an agent or a
/// target assignment must not vanish out from under them.
pub enum DeleteRegion {
    Deleted,
    NotFound,
    InUse,
}

/// Outcome of resolving an agent bearer token.
pub enum AgentAuth {
    Active { id: Uuid, region: String },
    Disabled,
    Invalid,
}

pub struct OperatorRepo {
    pool: PgPool,
}

impl OperatorRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn list_regions(&self) -> Result<Vec<RegionRow>> {
        let rows = sqlx::query_as::<_, RegionRow>(
            "SELECT id, name, city, country_code, continent, latitude, longitude, \
             enabled, default_selected, created_at FROM regions ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .context("operator: list regions")?;
        Ok(rows)
    }

    /// Create a region. `false` if the id already exists.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_region(
        &self,
        id: &str,
        name: &str,
        city: Option<&str>,
        country_code: Option<&str>,
        continent: Option<&str>,
        latitude: Option<f64>,
        longitude: Option<f64>,
    ) -> Result<bool> {
        let res = sqlx::query(
            "INSERT INTO regions (id, name, city, country_code, continent, latitude, longitude) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (id) DO NOTHING",
        )
        .bind(id)
        .bind(name)
        .bind(city)
        .bind(country_code)
        .bind(continent)
        .bind(latitude)
        .bind(longitude)
        .execute(&self.pool)
        .await
        .context("operator: create region")?;
        Ok(res.rows_affected() > 0)
    }

    /// Patch a region's display fields. Each `None` leaves that field as-is, so
    /// a name-only update never wipes the city. `false` if no such region.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_region(
        &self,
        id: &str,
        name: Option<&str>,
        city: Option<&str>,
        country_code: Option<&str>,
        continent: Option<&str>,
        latitude: Option<f64>,
        longitude: Option<f64>,
    ) -> Result<bool> {
        let res = sqlx::query(
            "UPDATE regions SET name = COALESCE($2, name), \
             city = COALESCE($3, city), \
             country_code = COALESCE($4, country_code), \
             continent = COALESCE($5, continent), \
             latitude = COALESCE($6, latitude), \
             longitude = COALESCE($7, longitude) WHERE id = $1",
        )
        .bind(id)
        .bind(name)
        .bind(city)
        .bind(country_code)
        .bind(continent)
        .bind(latitude)
        .bind(longitude)
        .execute(&self.pool)
        .await
        .context("operator: update region")?;
        Ok(res.rows_affected() > 0)
    }

    /// Enable/disable a region (disabled = not scheduled, not served to agents,
    /// history kept). `false` if no such region.
    pub async fn set_region_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        let res = sqlx::query("UPDATE regions SET enabled = $2 WHERE id = $1")
            .bind(id)
            .bind(enabled)
            .execute(&self.pool)
            .await
            .context("operator: set region enabled")?;
        Ok(res.rows_affected() > 0)
    }

    /// Opt a region in or out of the set a new monitor starts assigned to.
    /// Independent of `enabled`: an opted-out region still probes the monitors
    /// already assigned to it, it just stops being a default.
    pub async fn set_region_default_selected(&self, id: &str, on: bool) -> Result<bool> {
        let res = sqlx::query("UPDATE regions SET default_selected = $2 WHERE id = $1")
            .bind(id)
            .bind(on)
            .execute(&self.pool)
            .await
            .context("operator: set region default_selected")?;
        Ok(res.rows_affected() > 0)
    }

    /// Delete a region. Lets the FK from `agents`/`target_regions` enforce
    /// "still in use" rather than a check-then-delete (which would race a
    /// concurrent agent/assignment into a raw 500): a `23503` is reported as
    /// [`DeleteRegion::InUse`].
    pub async fn delete_region(&self, id: &str) -> Result<DeleteRegion> {
        match sqlx::query("DELETE FROM regions WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
        {
            Ok(res) if res.rows_affected() > 0 => Ok(DeleteRegion::Deleted),
            Ok(_) => Ok(DeleteRegion::NotFound),
            Err(e) if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23503") => {
                Ok(DeleteRegion::InUse)
            }
            Err(e) => Err(anyhow::Error::new(e)
                .context("operator: delete region")
                .into()),
        }
    }

    pub async fn list_agents(&self) -> Result<Vec<AgentRow>> {
        let rows = sqlx::query_as::<_, AgentRow>(
            "SELECT id, region, name, enabled, token_prefix, last_seen_at, created_at, \
             config_missing_since, scheduled_monitors, build FROM agents ORDER BY region, name",
        )
        .fetch_all(&self.pool)
        .await
        .context("operator: list agents")?;
        Ok(rows)
    }

    /// Provision an agent and mint its token. Returns the row plus the raw
    /// token (shown exactly once). `None` if the region does not exist.
    pub async fn create_agent(
        &self,
        region: &str,
        name: &str,
    ) -> Result<Option<(AgentRow, String)>> {
        let (region_exists,): (bool,) =
            sqlx::query_as("SELECT EXISTS (SELECT 1 FROM regions WHERE id = $1)")
                .bind(region)
                .fetch_one(&self.pool)
                .await
                .context("operator: create agent region check")?;
        if !region_exists {
            return Ok(None);
        }
        let raw = agent_token::generate_raw();
        let prefix = agent_token::visible_prefix(&raw).to_string();
        let hash = token_hash::hash(&raw)?;
        let row = sqlx::query_as::<_, AgentRow>(
            "INSERT INTO agents (region, name, token_hash, token_prefix) \
             VALUES ($1, $2, $3, $4) \
             RETURNING id, region, name, enabled, token_prefix, last_seen_at, created_at, \
                       config_missing_since, scheduled_monitors, build",
        )
        .bind(region)
        .bind(name)
        .bind(&hash)
        .bind(&prefix)
        .fetch_one(&self.pool)
        .await
        .context("operator: create agent")?;
        Ok(Some((row, raw)))
    }

    pub async fn set_agent_enabled(&self, id: Uuid, enabled: bool) -> Result<bool> {
        let res = sqlx::query("UPDATE agents SET enabled = $2 WHERE id = $1")
            .bind(id)
            .bind(enabled)
            .execute(&self.pool)
            .await
            .context("operator: set agent enabled")?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn rename_agent(&self, id: Uuid, name: &str) -> Result<bool> {
        let res = sqlx::query("UPDATE agents SET name = $2 WHERE id = $1")
            .bind(id)
            .bind(name)
            .execute(&self.pool)
            .await
            .context("operator: rename agent")?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn delete_agent(&self, id: Uuid) -> Result<bool> {
        let res = sqlx::query("DELETE FROM agents WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .context("operator: delete agent")?;
        Ok(res.rows_affected() > 0)
    }

    /// Resolve an agent bearer token: prefix-narrow then argon2-verify, and
    /// reject a disabled agent so the operator's disable toggle actually stops
    /// the box (not only the silence alert).
    pub async fn authenticate_agent(&self, raw: &str) -> Result<AgentAuth> {
        if !raw.starts_with(agent_token::PREFIX) {
            return Ok(AgentAuth::Invalid);
        }
        let prefix = agent_token::visible_prefix(raw);
        let rows: Vec<(Uuid, String, String, bool)> = sqlx::query_as(
            "SELECT id, region, token_hash, enabled FROM agents WHERE token_prefix = $1",
        )
        .bind(prefix)
        .fetch_all(&self.pool)
        .await
        .context("operator: authenticate agent")?;
        for (id, region, hash, enabled) in rows {
            if token_hash::verify(raw, &hash) {
                return Ok(if enabled {
                    AgentAuth::Active { id, region }
                } else {
                    AgentAuth::Disabled
                });
            }
        }
        Ok(AgentAuth::Invalid)
    }

    /// Lazy `last_seen_at` bump; called from the agent-auth path on every
    /// authenticated pull/push (debounced upstream by the extractor). An agent
    /// never seen, or back after more than `stale_after_secs`, has most likely
    /// restarted without its config, so it reads as holding none until a pull
    /// shows one, and the count from before it left goes with it. Same
    /// statement as the bump, so no reader sees it fresh with the state from
    /// before it left.
    pub async fn touch_agent_seen(&self, id: Uuid, stale_after_secs: u64) -> Result<()> {
        sqlx::query(
            "UPDATE agents SET \
                 config_missing_since = CASE \
                     WHEN config_missing_since IS NULL \
                      AND (last_seen_at IS NULL \
                           OR last_seen_at < now() - ($2::bigint * interval '1 second')) \
                     THEN now() ELSE config_missing_since END, \
                 scheduled_monitors = CASE \
                     WHEN last_seen_at IS NULL \
                       OR last_seen_at < now() - ($2::bigint * interval '1 second') \
                     THEN NULL ELSE scheduled_monitors END, \
                 last_seen_at = now() \
             WHERE id = $1",
        )
        .bind(id)
        .bind(stale_after_secs as i64)
        .execute(&self.pool)
        .await
        .context("operator: touch agent last_seen")?;
        Ok(())
    }

    /// Record what a config pull says the agent holds. Conditional, so a pull
    /// that repeats the state of a fresh agent writes nothing. Showing a config
    /// leaves the agent fresh, so the heartbeat of the same or an earlier
    /// request voids nothing. Showing none keeps the first time it was seen.
    pub async fn record_config_evidence(
        &self,
        id: Uuid,
        held: HeldConfig,
        stale_after_secs: u64,
    ) -> Result<()> {
        let query = match held {
            HeldConfig::Loaded(count) => sqlx::query(
                "UPDATE agents SET config_missing_since = NULL, scheduled_monitors = $2, \
                     last_seen_at = now() \
                 WHERE id = $1 \
                   AND (config_missing_since IS NOT NULL \
                        OR scheduled_monitors IS DISTINCT FROM $2 \
                        OR last_seen_at IS NULL \
                        OR last_seen_at < now() - ($3::bigint * interval '1 second'))",
            )
            .bind(id)
            .bind(count)
            .bind(stale_after_secs as i64),
            HeldConfig::Nothing => sqlx::query(
                "UPDATE agents SET config_missing_since = now(), scheduled_monitors = NULL \
                 WHERE id = $1 AND config_missing_since IS NULL",
            )
            .bind(id),
        };
        query
            .execute(&self.pool)
            .await
            .context("operator: record agent config evidence")?;
        Ok(())
    }

    /// Persist an agent's self-reported flow capability. Conditional so a repeat
    /// pull reporting the same value writes nothing.
    pub async fn set_agent_flow_capable(&self, id: Uuid, capable: bool) -> Result<()> {
        sqlx::query(
            "UPDATE agents SET flow_capable = $2 \
             WHERE id = $1 AND flow_capable IS DISTINCT FROM $2",
        )
        .bind(id)
        .bind(capable)
        .execute(&self.pool)
        .await
        .context("operator: set agent flow_capable")?;
        Ok(())
    }

    /// Persist the build a config pull reported, `None` when it sent none, so
    /// an agent moved back to a build that does not report keeps no newer one.
    /// Conditional so a repeat pull reporting the same build writes nothing.
    pub async fn set_agent_build(&self, id: Uuid, build: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE agents SET build = $2 WHERE id = $1 AND build IS DISTINCT FROM $2")
            .bind(id)
            .bind(build)
            .execute(&self.pool)
            .await
            .context("operator: set agent build")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(90);

    fn row(now: DateTime<Utc>, seen_ago: Option<i64>, missing_ago: Option<i64>) -> AgentRow {
        let ago = |secs| now - chrono::Duration::seconds(secs);
        AgentRow {
            id: Uuid::now_v7(),
            region: "eu".into(),
            name: "eu-1".into(),
            enabled: true,
            token_prefix: "tok".into(),
            last_seen_at: seen_ago.map(ago),
            created_at: ago(3600),
            config_missing_since: missing_ago.map(ago),
            scheduled_monitors: None,
            build: None,
        }
    }

    #[test]
    fn an_agent_holds_a_config_while_in_touch_and_showing_one() {
        let now = Utc::now();
        assert!(row(now, Some(90), None).holds_config(now, WINDOW));
        assert!(!row(now, Some(91), None).holds_config(now, WINDOW));
        assert!(!row(now, Some(5), Some(0)).holds_config(now, WINDOW));
        assert!(
            !row(now, None, None).holds_config(now, WINDOW),
            "never seen"
        );
        let mut disabled = row(now, Some(5), None);
        disabled.enabled = false;
        assert!(!disabled.holds_config(now, WINDOW));
    }

    #[test]
    fn an_agent_without_a_config_probes_until_the_window_runs_out() {
        let now = Utc::now();
        assert!(row(now, Some(5), None).is_probing(now, WINDOW));
        assert!(row(now, Some(5), Some(90)).is_probing(now, WINDOW));
        assert!(!row(now, Some(5), Some(91)).is_probing(now, WINDOW));
        assert!(
            !row(now, Some(91), Some(10)).is_probing(now, WINDOW),
            "a stale agent is not probing, in grace or not"
        );
        let mut disabled = row(now, Some(5), Some(10));
        disabled.enabled = false;
        assert!(!disabled.is_probing(now, WINDOW));
    }

    #[test]
    fn a_never_seen_agent_ages_from_its_creation() {
        let now = Utc::now();
        let mut minted = row(now, None, None);
        minted.created_at = now - chrono::Duration::seconds(90);
        assert!(minted.is_fresh(now, WINDOW));
        assert!(minted.is_probing(now, WINDOW), "in its grace");
        assert!(!minted.holds_config(now, WINDOW));
        minted.created_at = now - chrono::Duration::seconds(91);
        assert!(!minted.is_fresh(now, WINDOW));
        assert!(!minted.is_probing(now, WINDOW));
    }
}
