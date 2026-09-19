//! Operator (instance-admin) data access: regions and agents. Cross-tenant by
//! nature and reached only through the static-secret `/operator` surface, so
//! none of these methods take an `OrgId` — they are deliberately global.

use anyhow::Context;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

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
            "SELECT id, region, name, enabled, token_prefix, last_seen_at, created_at \
             FROM agents ORDER BY region, name",
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
             RETURNING id, region, name, enabled, token_prefix, last_seen_at, created_at",
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
    /// authenticated pull/push (debounced upstream by the extractor).
    pub async fn touch_agent_seen(&self, id: Uuid) -> Result<()> {
        sqlx::query("UPDATE agents SET last_seen_at = now() WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .context("operator: touch agent last_seen")?;
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
}
