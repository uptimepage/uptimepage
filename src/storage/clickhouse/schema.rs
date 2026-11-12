use anyhow::Context;
use clickhouse::{Client, Row};
use serde::Deserialize;

use crate::error::Result;

use super::{CountRow, fetch_single};

/// Ordered list of migrations. Each entry is `(filename, sql)`. Filename is
/// recorded in `schema_migrations` after apply so we never re-run a migration
/// that has already executed on this database.
///
/// Crash-atomicity discipline: every migration MUST be idempotent on re-run
/// (i.e. only `CREATE … IF NOT EXISTS` / `ALTER … IF EXISTS`). ClickHouse has
/// no transactions across DDL statements and `schema_migrations` is TinyLog
/// (no atomic CAS), so the apply-then-record sequence is not crash-atomic:
/// an OOM-kill between the last statement and the recording INSERT leaves
/// the migration officially un-applied and the next boot re-runs it. A
/// destructive statement (DROP/TRUNCATE) under those conditions wipes live
/// data, which is why migrations contain none.
///
/// The splitter is a real tokenizer ([`split_statements`]) — it tracks
/// single/double-quote string literals, backtick identifiers, line
/// comments (`--`) and block comments (`/* … */`), so `;` inside any of
/// those does **not** become a chunk boundary. CREATE FUNCTION bodies,
/// regex defaults containing `';'`, doubled-quote escapes (`'it''s'`) and
/// backslash escapes (`'a\\'b'`) all round-trip.
///
/// The runner is not concurrent-safe: two processes racing through their
/// first boot could both observe an empty applied set and both run the
/// migration. With the IF NOT EXISTS discipline above this is harmless
/// (second CREATE is a no-op); for multi-replica, take a pg_advisory_lock
/// around the call.
const MIGRATIONS: &[(&str, &str)] = &[
    (
        "001_initial.sql",
        include_str!("../../../migrations/clickhouse/001_initial.sql"),
    ),
    // 002 `check_results_1h`: the hour-rollup for the long history tail. Same
    // AggregateFunction columns as `check_results_1m`, so [`super::rollup_source`]
    // merges either with one finaliser set; it routes ranges past the 1m
    // rollup's 30-day TTL here. A 2nd matview on raw `check_results` (accrues
    // forward, no backfill). The 13-month TTL exceeds the raw/1m TTL, so the
    // Privacy Policy and the `retention_test` guard disclose it; org erasure
    // must clear it too (see [`CH_TENANT_TABLES`]). Migration SQL is frozen —
    // keep this rationale here.
    (
        "002_check_results_1h.sql",
        include_str!("../../../migrations/clickhouse/002_check_results_1h.sql"),
    ),
    // 003 `flow_runs`: one row per browser-flow run, carrying the step trace and
    // — on a failure — the page snapshot. Two retention windows in one table via
    // a per-column TTL driven by `evidence_days`, so page content is dropped
    // ahead of the trace beside it with no second table and no mutation job.
    // Org erasure must clear it too (see [`CH_TENANT_TABLES`]).
    (
        "003_flow_runs.sql",
        include_str!("../../../migrations/clickhouse/003_flow_runs.sql"),
    ),
    // 004 `heartbeat_pings`: the job's own account of its runs, which
    // `check_results` cannot hold. Job output takes the shorter `evidence_days`
    // window, same split as `flow_runs`. Org erasure must clear it too.
    (
        "004_heartbeat_pings.sql",
        include_str!("../../../migrations/clickhouse/004_heartbeat_pings.sql"),
    ),
    // Raw rows only: a diagnosis explains one response, so it is deliberately
    // absent from the rollups.
    (
        "005_check_diagnostics.sql",
        include_str!("../../../migrations/clickhouse/005_check_diagnostics.sql"),
    ),
    // Raw rows only, like the diagnosis: the path explains one response.
    (
        "006_check_network_path.sql",
        include_str!("../../../migrations/clickhouse/006_check_network_path.sql"),
    ),
    // Raw rows only: the facts a check read describe one run, and the detail
    // page reads the newest row per region straight off the raw table.
    (
        "007_check_details.sql",
        include_str!("../../../migrations/clickhouse/007_check_details.sql"),
    ),
    // 008 `alert_events`: each alert source's delivery log, one row per event.
    // The body sample takes the shorter `evidence_days` window, as in
    // `heartbeat_pings`. Org erasure must clear it too.
    (
        "008_alert_events.sql",
        include_str!("../../../migrations/clickhouse/008_alert_events.sql"),
    ),
    (
        "009_alert_event_id.sql",
        include_str!("../../../migrations/clickhouse/009_alert_event_id.sql"),
    ),
];

pub async fn migrate(client: &Client) -> Result<()> {
    tracing::info!("running clickhouse migrations");

    client
        .query(
            "CREATE TABLE IF NOT EXISTS schema_migrations (\
                 filename String, \
                 applied_at DateTime64(3, 'UTC') DEFAULT now64(3) \
             ) ENGINE = TinyLog",
        )
        .execute()
        .await
        .context("create schema_migrations")?;

    #[derive(Row, Deserialize)]
    struct AppliedRow {
        filename: String,
    }
    let applied: Vec<AppliedRow> = client
        .query("SELECT filename FROM schema_migrations")
        .fetch_all::<AppliedRow>()
        .await
        .context("read schema_migrations")?;

    for (name, sql) in MIGRATIONS {
        if applied.iter().any(|f| f.filename == *name) {
            tracing::debug!(migration = name, "clickhouse migration already applied");
            continue;
        }
        tracing::info!(migration = name, "applying clickhouse migration");
        for stmt in split_statements(sql)
            .with_context(|| format!("clickhouse migration {name}: tokenize source"))?
        {
            client
                .query(&stmt)
                .execute()
                .await
                .with_context(|| format!("clickhouse migration {name}"))?;
        }
        client
            .query("INSERT INTO schema_migrations (filename) VALUES (?)")
            .bind(*name)
            .execute()
            .await
            .with_context(|| format!("record clickhouse migration {name}"))?;

        // Fence: re-read schema_migrations and confirm the row landed. Without
        // this an INSERT that the server accepted but failed to persist (TinyLog
        // gives no fsync guarantee, a crash mid-flush can lose the row) would
        // let the next boot re-run the migration. With the IF-NOT-EXISTS
        // discipline above that is harmless today, but the fence costs one
        // count query per migration and removes the silent footgun outright.
        let CountRow { n } = fetch_single(
            client
                .query("SELECT count() AS n FROM schema_migrations WHERE filename = ?")
                .bind(*name),
        )
        .await
        .with_context(|| format!("verify clickhouse migration recorded {name}"))?;
        if n == 0 {
            return Err(anyhow::anyhow!(
                "clickhouse migration {name} applied but not recorded in schema_migrations",
            )
            .into());
        }
    }

    verify_rollup_schema(client).await?;
    dedup_retries_in_rollups(client).await?;

    tracing::info!("clickhouse ready");
    Ok(())
}

/// Without their own window, a retry `check_results` drops still counts twice
/// in the rollups. Not a migration file: ClickHouse refuses this setting
/// through a materialized view, and the inner table's name differs per install.
async fn dedup_retries_in_rollups(client: &Client) -> Result<()> {
    let without_window = client
        .query(
            "SELECT name FROM system.tables \
             WHERE database = currentDatabase() \
             AND engine_full NOT LIKE '%non_replicated_deduplication_window%' \
             AND name IN ( \
                 SELECT concat('.inner_id.', toString(uuid)) FROM system.tables \
                 WHERE database = currentDatabase() \
                 AND name IN ('check_results_1m', 'check_results_1h'))",
        )
        .fetch_all::<String>()
        .await
        .context("find rollup inner tables")?;
    for table in without_window {
        client
            .query(&format!(
                "ALTER TABLE `{table}` MODIFY SETTING non_replicated_deduplication_window = 1000"
            ))
            .execute()
            .await
            .with_context(|| format!("set dedup window on {table}"))?;
    }
    Ok(())
}

/// Exact `(column, type)` shape of `check_results_1m`, in definition order.
/// Editing the matview in `001_initial.sql` is a no-op on an existing DB
/// (recorded migration + `IF NOT EXISTS`), so [`verify_rollup_schema`] checks
/// the live view against this at boot and fails loud on drift. A matview change
/// = recreate migration + update 001 + update this list. Type strings are
/// CH-version-formatted; a server upgrade that reformats them fails boot here.
const EXPECTED_ROLLUP_SCHEMA: &[(&str, &str)] = &[
    ("org_id", "UUID"),
    ("target_id", "UUID"),
    ("region", "LowCardinality(String)"),
    ("minute", "DateTime('UTC')"),
    ("total_checks", "AggregateFunction(count)"),
    ("up_checks", "AggregateFunction(countIf, UInt8)"),
    ("down_checks", "AggregateFunction(countIf, UInt8)"),
    ("degraded_checks", "AggregateFunction(countIf, UInt8)"),
    ("error_checks", "AggregateFunction(countIf, UInt8)"),
    ("avg_duration_ms", "AggregateFunction(avg, UInt32)"),
    (
        "duration_quantiles",
        "AggregateFunction(quantiles(0.5, 0.95, 0.99), UInt32)",
    ),
    ("avg_dns_ms", "AggregateFunction(avg, Nullable(UInt16))"),
    ("avg_connect_ms", "AggregateFunction(avg, Nullable(UInt16))"),
    ("avg_tls_ms", "AggregateFunction(avg, Nullable(UInt16))"),
    ("avg_ttfb_ms", "AggregateFunction(avg, Nullable(UInt16))"),
    (
        "last_status_state",
        "AggregateFunction(argMax, Enum8('up' = 1, 'down' = 2, 'degraded' = 3, 'error' = 4), DateTime('UTC'))",
    ),
];

/// Boot check: both rollups must equal [`EXPECTED_ROLLUP_SCHEMA`]. The 1h view
/// mirrors the 1m column set with `hour` in place of `minute`.
async fn verify_rollup_schema(client: &Client) -> Result<()> {
    verify_view_schema(client, "check_results_1m", EXPECTED_ROLLUP_SCHEMA).await?;
    let expected_1h: Vec<(&str, &str)> = EXPECTED_ROLLUP_SCHEMA
        .iter()
        .map(|(n, t)| {
            if *n == "minute" {
                ("hour", *t)
            } else {
                (*n, *t)
            }
        })
        .collect();
    verify_view_schema(client, "check_results_1h", &expected_1h).await?;
    Ok(())
}

async fn verify_view_schema(client: &Client, view: &str, expected: &[(&str, &str)]) -> Result<()> {
    #[derive(Row, Deserialize)]
    struct Col {
        name: String,
        #[serde(rename = "type")]
        ty: String,
    }
    let live: Vec<(String, String)> = client
        .query(
            "SELECT name, type FROM system.columns \
             WHERE database = currentDatabase() AND table = ? \
             ORDER BY position",
        )
        .bind(view)
        .fetch_all::<Col>()
        .await
        .context("clickhouse verify_view_schema: read system.columns")?
        .into_iter()
        .map(|c| (c.name, c.ty))
        .collect();
    let expected: Vec<(String, String)> = expected
        .iter()
        .map(|(n, t)| ((*n).to_string(), (*t).to_string()))
        .collect();
    if live != expected {
        return Err(anyhow::anyhow!(
            "{view} schema drifted from the readers' contract — a matview edit is a \
             no-op on an existing DB; ship a recreate migration and update \
             EXPECTED_ROLLUP_SCHEMA.\n  expected: {expected:?}\n  live:     {live:?}"
        )
        .into());
    }
    Ok(())
}

/// Split a migration source into executable statements with full
/// awareness of string literals and comments.
///
/// Quoted regions (`'…'`, `"…"`, `` `…` ``) and comments (`--` to
/// newline, `/* … */` block) suppress `;` recognition. Doubled quotes
/// (`''`, `""`) and backslash escapes (`\'`, `\\`) inside a string keep
/// the parser inside that string. Comment bodies are dropped from the
/// emitted statement; quoted bodies are kept verbatim.
///
/// Returns an error on an unterminated string literal or block comment
/// at EOF — these are migration bugs that must boot-fail loudly with a
/// pointer to the source rather than be papered over with a half-parsed
/// statement that ClickHouse then rejects far from the cause.
fn split_statements(sql: &str) -> Result<Vec<String>> {
    enum State {
        Normal,
        Quoted(char),
        LineComment,
        BlockComment,
    }

    let mut state = State::Normal;
    let mut current = String::new();
    let mut out = Vec::new();
    let mut chars = sql.chars().peekable();

    while let Some(c) = chars.next() {
        match state {
            State::Normal => {
                if c == ';' {
                    push_statement(&mut current, &mut out);
                } else if c == '-' && chars.peek() == Some(&'-') {
                    chars.next();
                    state = State::LineComment;
                } else if c == '/' && chars.peek() == Some(&'*') {
                    chars.next();
                    state = State::BlockComment;
                } else if c == '\'' || c == '"' || c == '`' {
                    current.push(c);
                    state = State::Quoted(c);
                } else {
                    current.push(c);
                }
            }
            State::Quoted(q) => {
                current.push(c);
                if c == '\\' {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                } else if c == q {
                    if chars.peek() == Some(&q) {
                        if let Some(next) = chars.next() {
                            current.push(next);
                        }
                    } else {
                        state = State::Normal;
                    }
                }
            }
            State::LineComment => {
                if c == '\n' {
                    current.push('\n');
                    state = State::Normal;
                }
            }
            State::BlockComment => {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    state = State::Normal;
                }
            }
        }
    }
    match state {
        State::Normal | State::LineComment => {
            push_statement(&mut current, &mut out);
            Ok(out)
        }
        State::Quoted(q) => {
            Err(anyhow::anyhow!("unterminated {q} string literal in migration source").into())
        }
        State::BlockComment => {
            Err(anyhow::anyhow!("unterminated /* … */ block comment in migration source").into())
        }
    }
}

fn push_statement(buf: &mut String, out: &mut Vec<String>) {
    let trimmed = buf.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }
    buf.clear();
}

#[cfg(test)]
mod tests {
    use crate::domain::CheckStatus;

    use super::{MIGRATIONS, split_statements};

    /// Parse the single `Enum8('name' = N, ...)` definition out of the embedded
    /// migration into (name, value) pairs.
    fn check_results_enum8() -> Vec<(String, i8)> {
        let sql = MIGRATIONS[0].1;
        let open = sql
            .find("Enum8(")
            .expect("check_results has an Enum8 column")
            + "Enum8(".len();
        let close = open + sql[open..].find(')').expect("Enum8 close paren");
        sql[open..close]
            .split(',')
            .map(|kv| {
                let (name, val) = kv.split_once('=').expect("Enum8 entry is `name = value`");
                (
                    name.trim().trim_matches('\'').to_string(),
                    val.trim().parse::<i8>().expect("Enum8 value is an int"),
                )
            })
            .collect()
    }

    /// Cross-store contract: every `CheckStatus` must exist in the
    /// `check_results` Enum8 with a matching name+value. ClickHouse `Enum8` is a
    /// closed domain — inserting an undefined key rejects the whole block, so a
    /// new variant without a migration silently dark-holes all ingest.
    #[test]
    fn check_status_matches_clickhouse_enum8() {
        // Exhaustive on purpose: a new `CheckStatus` variant fails to compile
        // here, forcing this list AND the migration Enum8 to be updated together.
        const ALL: &[CheckStatus] = &[
            CheckStatus::Up,
            CheckStatus::Down,
            CheckStatus::Degraded,
            CheckStatus::Error,
        ];
        // Uncalled, but its body is still exhaustiveness-checked at compile
        // time — a new variant turns this into a hard E0004, the forcing signal.
        #[allow(dead_code)]
        fn exhaustiveness_guard(s: CheckStatus) {
            match s {
                CheckStatus::Up
                | CheckStatus::Down
                | CheckStatus::Degraded
                | CheckStatus::Error => {}
            }
        }

        let pairs = check_results_enum8();
        assert_eq!(
            pairs.len(),
            ALL.len(),
            "check_results Enum8 has {} keys but CheckStatus has {} variants: {pairs:?}",
            pairs.len(),
            ALL.len()
        );
        for &s in ALL {
            assert!(
                pairs
                    .iter()
                    .any(|(name, val)| name == s.as_str() && *val == s.as_enum8()),
                "CheckStatus::{s:?} ({}={}) is not in the check_results Enum8 {pairs:?} — \
                 adding a variant requires a ClickHouse migration",
                s.as_str(),
                s.as_enum8()
            );
        }
    }

    #[test]
    fn split_strips_line_comments_before_splitting() {
        // A stray `;` inside a `--` comment used to produce an empty-query
        // chunk that ClickHouse rejected with SYNTAX_ERROR code 62. Regression
        // guard: the splitter must drop the comment text first.
        let sql = "-- prelude with a semi; in it\n\
                   CREATE TABLE foo (x UInt8) ENGINE = TinyLog;\n\
                   -- another; with a semi\n\
                   DROP TABLE foo;";
        let stmts = split_statements(sql).expect("test input is well-formed");
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("CREATE TABLE foo"));
        assert!(stmts[1].contains("DROP TABLE foo"));
    }

    #[test]
    fn split_discards_trailing_blank_chunk() {
        let stmts = split_statements("SELECT 1;\n").expect("test input is well-formed");
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "SELECT 1");
    }

    #[test]
    fn split_preserves_inline_comment_after_statement() {
        let stmts = split_statements("SELECT 1; -- trailing\nSELECT 2;")
            .expect("test input is well-formed");
        assert_eq!(stmts, vec!["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn split_keeps_semicolon_inside_single_quoted_string() {
        // A `;` inside a quoted literal would become a chunk boundary
        // under a naive split, leaving two syntax-error halves. The
        // tokenizer must keep this as one statement.
        let sql = "INSERT INTO t VALUES ('a; b', 'c;');";
        let stmts = split_statements(sql).expect("test input is well-formed");
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "INSERT INTO t VALUES ('a; b', 'c;')");
    }

    #[test]
    fn split_handles_doubled_quote_escape() {
        // SQL-standard `''` inside a string represents a single quote and
        // must not close the literal; an unintended close would expose a
        // following `;` to the splitter.
        let sql = "SELECT 'it''s; not over'; SELECT 2;";
        let stmts = split_statements(sql).expect("test input is well-formed");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT 'it''s; not over'");
        assert_eq!(stmts[1], "SELECT 2");
    }

    #[test]
    fn split_handles_backslash_escape_inside_string() {
        // ClickHouse accepts `\'` as an escaped quote. The escape must
        // keep the parser inside the string so the trailing `;` is data.
        let sql = "SELECT 'a\\'b;c'; SELECT 2;";
        let stmts = split_statements(sql).expect("test input is well-formed");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT 'a\\'b;c'");
        assert_eq!(stmts[1], "SELECT 2");
    }

    #[test]
    fn split_keeps_semicolon_inside_double_quoted_identifier() {
        let sql = "SELECT \"a;b\" FROM t; SELECT 2;";
        let stmts = split_statements(sql).expect("test input is well-formed");
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("\"a;b\""));
    }

    #[test]
    fn split_keeps_semicolon_inside_backtick_identifier() {
        let sql = "SELECT `a;b` FROM t; SELECT 2;";
        let stmts = split_statements(sql).expect("test input is well-formed");
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("`a;b`"));
    }

    #[test]
    fn split_strips_block_comment_with_semicolon_inside() {
        let sql = "SELECT 1 /* foo; bar */; SELECT 2;";
        let stmts = split_statements(sql).expect("test input is well-formed");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[1], "SELECT 2");
    }

    #[test]
    fn split_comment_only_input_produces_nothing() {
        let stmts = split_statements("-- just a comment\n/* and a block */")
            .expect("test input is well-formed");
        assert!(stmts.is_empty());
    }

    #[test]
    fn split_handles_trailing_statement_without_semicolon() {
        let stmts = split_statements("SELECT 1; SELECT 2").expect("test input is well-formed");
        assert_eq!(stmts, vec!["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn split_errors_on_unterminated_string_literal() {
        let err = split_statements("INSERT INTO t VALUES ('oops")
            .expect_err("must error on unterminated string");
        let msg = err.to_string();
        assert!(
            msg.contains("unterminated") && msg.contains("string"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn split_errors_on_unterminated_block_comment() {
        let err = split_statements("SELECT 1 /* never closes")
            .expect_err("must error on unterminated block comment");
        let msg = err.to_string();
        assert!(
            msg.contains("unterminated") && msg.contains("block comment"),
            "unexpected error: {msg}"
        );
    }
}
