//! A monitor's notification channels live in `target_channels` and a group's in
//! `group_channels`, whose foreign keys drop a binding in the transaction that
//! deletes its owner or its channel. A write that binds channels locks them
//! here first, before any group, monitor or binding row. A statement that
//! removes the bindings of several owners locks them first in key order, so
//! two such removals sharing rows cannot each hold one the other waits on.

use std::collections::HashMap;

use anyhow::anyhow;
use uuid::Uuid;

use crate::domain::OrgId;
use crate::error::codes;
use crate::error::{AppError, Result};

pub const ALERT_CHANNEL_FIELD: &str = "alerts.channel_id";
pub const BULK_CHANNELS_FIELD: &str = "action.channel_ids";

/// The `alerts` column a target row decodes, built from its bindings in saved
/// order, for the targets row the surrounding query names `$t`. Read in the
/// same statement as the row, so both come from one snapshot.
macro_rules! target_alerts_column {
    ($t:literal) => {
        concat!(
            "COALESCE((SELECT jsonb_agg(jsonb_build_object('channel_id', tc.channel_id) \
                                        ORDER BY tc.position, tc.channel_id) \
                         FROM target_channels tc WHERE tc.target_id = ",
            $t,
            ".id), '[]'::jsonb) AS alerts"
        )
    };
}
pub(crate) use target_alerts_column;

/// [`target_alerts_column`] for the monitor_groups row the query names `$g`.
macro_rules! group_alerts_column {
    ($g:literal) => {
        concat!(
            "COALESCE((SELECT jsonb_agg(jsonb_build_object('channel_id', gc.channel_id) \
                                        ORDER BY gc.position, gc.channel_id) \
                         FROM group_channels gc WHERE gc.group_id = ",
            $g,
            ".id), '[]'::jsonb) AS alerts"
        )
    };
}
pub(crate) use group_alerts_column;

/// Whose bindings a write changes.
#[derive(Debug, Clone, Copy)]
pub(crate) enum BindingOwner {
    Target,
    Group,
}

impl BindingOwner {
    const fn table(self) -> &'static str {
        match self {
            Self::Target => "target_channels",
            Self::Group => "group_channels",
        }
    }

    const fn column(self) -> &'static str {
        match self {
            Self::Target => "target_id",
            Self::Group => "group_id",
        }
    }
}

/// Locks, in the shared order, every binding of the monitors a delete is about
/// to cascade away.
pub(crate) async fn lock_bindings_of(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    targets: &[Uuid],
) -> Result<()> {
    sqlx::query(
        "SELECT 1 FROM target_channels WHERE target_id = ANY($1) AND org_id = $2 \
         ORDER BY target_id, channel_id FOR UPDATE",
    )
    .bind(targets)
    .bind(org.0)
    .execute(&mut **tx)
    .await
    .map_err(|e| AppError::Other(anyhow!("lock bindings: {e}")))?;
    Ok(())
}

pub fn unknown_channel(id: Uuid, field: &str) -> AppError {
    AppError::bad_request_field(
        codes::INVALID_ALERT_CONFIG,
        format!("notification channel {id} does not exist"),
        field,
    )
}

/// Holds the channels a write names until `tx` ends, and refuses any the org
/// does not have, before anything is written. `FOR KEY SHARE` blocks a channel
/// delete (and a rename, the name being unique) but not a config or delivery
/// update: the delete either waits for this write and then drops the committed
/// binding, or wins and the write is refused here. Taken before any monitor or
/// binding row, so a writer never holds a binding while waiting on a channel.
pub(crate) async fn lock_bound_channels(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    ids: &[Uuid],
    field: &str,
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let found: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM notification_channels \
         WHERE id = ANY($1::uuid[]) AND org_id = $2 FOR KEY SHARE",
    )
    .bind(&ids)
    .bind(org.0)
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| AppError::Other(anyhow!("lock bound channels: {e}")))?;
    match ids.iter().find(|id| !found.contains(id)) {
        Some(missing) => Err(unknown_channel(*missing, field)),
        None => Ok(()),
    }
}

/// Binds `owners[i]` to `channels[i]` at `positions[i]`. The caller holds the
/// channels.
pub(crate) async fn insert_bindings(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    owner: BindingOwner,
    owners: &[Uuid],
    channels: &[Uuid],
    positions: &[i32],
) -> Result<()> {
    if owners.is_empty() {
        return Ok(());
    }
    let sql = format!(
        "INSERT INTO {} ({}, channel_id, org_id, position) \
         SELECT t, c, $4, p FROM unnest($1::uuid[], $2::uuid[], $3::int4[]) AS u(t, c, p)",
        owner.table(),
        owner.column(),
    );
    sqlx::query(&sql)
        .bind(owners)
        .bind(channels)
        .bind(positions)
        .bind(org.0)
        .execute(&mut **tx)
        .await
        .map_err(|e| AppError::Other(anyhow!("insert bindings: {e}")))?;
    Ok(())
}

/// Makes `id`'s bindings exactly `channels`, in that order, writing only the
/// rows that change: a kept binding is left alone, so a channel delete running
/// at the same time removes it without waiting on this write. The caller holds
/// the channels and the owner's row.
pub(crate) async fn set_bindings(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    owner: BindingOwner,
    id: Uuid,
    channels: &[Uuid],
) -> Result<()> {
    let (table, column) = (owner.table(), owner.column());
    let current: HashMap<Uuid, i32> = sqlx::query_as(&format!(
        "SELECT channel_id, position FROM {table} WHERE {column} = $1 AND org_id = $2"
    ))
    .bind(id)
    .bind(org.0)
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| AppError::Other(anyhow!("read bindings: {e}")))?
    .into_iter()
    .collect();

    let removed: Vec<Uuid> = current
        .keys()
        .filter(|c| !channels.contains(c))
        .copied()
        .collect();
    let (mut moved, mut moved_to) = (Vec::new(), Vec::new());
    let (mut added, mut added_at) = (Vec::new(), Vec::new());
    for (position, channel) in (0_i32..).zip(channels) {
        match current.get(channel) {
            Some(&at) if at == position => {}
            Some(_) => {
                moved.push(*channel);
                moved_to.push(position);
            }
            None => {
                added.push(*channel);
                added_at.push(position);
            }
        }
    }

    if !removed.is_empty() {
        sqlx::query(&format!(
            "DELETE FROM {table} \
             WHERE {column} = $1 AND org_id = $2 AND channel_id = ANY($3::uuid[])"
        ))
        .bind(id)
        .bind(org.0)
        .bind(&removed)
        .execute(&mut **tx)
        .await
        .map_err(|e| AppError::Other(anyhow!("remove bindings: {e}")))?;
    }
    if !moved.is_empty() {
        sqlx::query(&format!(
            "UPDATE {table} b SET position = u.p \
             FROM unnest($3::uuid[], $4::int4[]) AS u(c, p) \
             WHERE b.{column} = $1 AND b.org_id = $2 AND b.channel_id = u.c"
        ))
        .bind(id)
        .bind(org.0)
        .bind(&moved)
        .bind(&moved_to)
        .execute(&mut **tx)
        .await
        .map_err(|e| AppError::Other(anyhow!("reorder bindings: {e}")))?;
    }
    let owners = vec![id; added.len()];
    insert_bindings(tx, org, owner, &owners, &added, &added_at).await
}
