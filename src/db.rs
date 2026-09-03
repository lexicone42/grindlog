//! SQLite persistence via sqlx. All times are i64 milliseconds; wall-clock
//! timestamps are unix epoch milliseconds (UTC).

use anyhow::Result;
use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

pub const OUTCOME_FINISHED: &str = "finished";
pub const OUTCOME_RESET: &str = "reset";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS runs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  game TEXT NOT NULL,
  category TEXT NOT NULL,
  attempt_number INTEGER NOT NULL,
  started_at_ms INTEGER NOT NULL,
  ended_at_ms INTEGER NOT NULL,
  outcome TEXT NOT NULL,          -- 'finished' | 'reset'
  reset_reason TEXT,              -- 'zeroed' | 'disappeared' | 'desync'
  final_time_ms INTEGER,          -- set when finished (or corrected)
  last_timer_ms INTEGER,          -- last timer value seen (context for resets)
  session_id INTEGER,             -- broadcast this run belongs to
  ls_attempt INTEGER              -- LiveSplit's lifetime attempt counter
);
CREATE INDEX IF NOT EXISTS idx_runs_game ON runs (game, category);
CREATE INDEX IF NOT EXISTS idx_runs_started ON runs (started_at_ms);

CREATE TABLE IF NOT EXISTS transitions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  at_ms INTEGER NOT NULL,
  from_phase TEXT NOT NULL,
  to_phase TEXT NOT NULL,
  game TEXT NOT NULL,
  category TEXT NOT NULL,
  detail TEXT
);

CREATE TABLE IF NOT EXISTS settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS splits (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id INTEGER NOT NULL,
  act_index INTEGER NOT NULL,
  act_name TEXT NOT NULL,
  cumulative_ms INTEGER NOT NULL,
  segment_ms INTEGER              -- cumulative minus previous act (when known)
);
CREATE INDEX IF NOT EXISTS idx_splits_run ON splits (run_id);

CREATE TABLE IF NOT EXISTS sessions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  started_at_ms INTEGER NOT NULL,
  ended_at_ms INTEGER,            -- NULL while the broadcast is ongoing
  source TEXT NOT NULL,           -- 'hls' | 'streamlink' | 'vod' | 'file'
  label TEXT NOT NULL,            -- channel name, vod id, or file path
  tag TEXT,                       -- e.g. 'arcathlon' for marathon broadcasts
  -- capture health, updated while the session runs
  frames INTEGER,                 -- frames analyzed
  parsed INTEGER,                 -- frames whose timer OCR parsed
  probing INTEGER,                -- frames spent without a locked layout
  relocks INTEGER,                -- layout locks/switches/re-anchors
  counter_reads INTEGER,          -- attempt-counter reads accepted
  events TEXT                     -- JSON [{t, k, d}] of layout events
);
"#;

pub async fn open(path: &str) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        // WAL lets `report` read while the live bot is writing.
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await?;
    sqlx::raw_sql(SCHEMA).execute(&pool).await?;
    // Migrations for databases created before newer columns existed.
    for (table, col, ty) in [
        ("runs", "session_id", "INTEGER"),
        ("runs", "ls_attempt", "INTEGER"),
        ("sessions", "tag", "TEXT"),
        ("sessions", "frames", "INTEGER"),
        ("sessions", "parsed", "INTEGER"),
        ("sessions", "probing", "INTEGER"),
        ("sessions", "relocks", "INTEGER"),
        ("sessions", "counter_reads", "INTEGER"),
        ("sessions", "events", "TEXT"),
    ] {
        let has: Option<i64> = sqlx::query_scalar(&format!(
            "SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?"
        ))
        .bind(col)
        .fetch_optional(&pool)
        .await?;
        if has.is_none() {
            sqlx::query(&format!("ALTER TABLE {table} ADD COLUMN {col} {ty}"))
                .execute(&pool)
                .await?;
        }
    }
    Ok(pool)
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct RunRow {
    pub id: i64,
    pub game: String,
    pub category: String,
    pub attempt_number: i64,
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
    pub outcome: String,
    pub reset_reason: Option<String>,
    pub final_time_ms: Option<i64>,
    pub last_timer_ms: Option<i64>,
    pub session_id: Option<i64>,
    pub ls_attempt: Option<i64>,
}

pub struct NewRun<'a> {
    pub game: &'a str,
    pub category: &'a str,
    pub attempt_number: i64,
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
    pub outcome: &'a str,
    pub reset_reason: Option<&'a str>,
    pub final_time_ms: Option<i64>,
    pub last_timer_ms: Option<i64>,
    pub session_id: Option<i64>,
    pub ls_attempt: Option<i64>,
}

pub async fn insert_run(pool: &SqlitePool, r: NewRun<'_>) -> Result<i64> {
    let res = sqlx::query(
        "INSERT INTO runs (game, category, attempt_number, started_at_ms, ended_at_ms, \
         outcome, reset_reason, final_time_ms, last_timer_ms, session_id, ls_attempt) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(r.game)
    .bind(r.category)
    .bind(r.attempt_number)
    .bind(r.started_at_ms)
    .bind(r.ended_at_ms)
    .bind(r.outcome)
    .bind(r.reset_reason)
    .bind(r.final_time_ms)
    .bind(r.last_timer_ms)
    .bind(r.session_id)
    .bind(r.ls_attempt)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn open_session(
    pool: &SqlitePool,
    started_at_ms: i64,
    source: &str,
    label: &str,
    tag: Option<&str>,
) -> Result<i64> {
    let res =
        sqlx::query("INSERT INTO sessions (started_at_ms, source, label, tag) VALUES (?, ?, ?, ?)")
            .bind(started_at_ms)
            .bind(source)
            .bind(label)
            .bind(tag)
            .execute(pool)
            .await?;
    Ok(res.last_insert_rowid())
}

/// Close sessions left open by a process that died: ended at their last
/// run's end, or at their start when they recorded nothing. Returns how many.
pub async fn close_stale_sessions(pool: &SqlitePool) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE sessions SET ended_at_ms = COALESCE( \
           (SELECT MAX(r.ended_at_ms) FROM runs r WHERE r.session_id = sessions.id), started_at_ms) \
         WHERE ended_at_ms IS NULL",
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

pub async fn close_session(pool: &SqlitePool, id: i64, ended_at_ms: i64) -> Result<()> {
    sqlx::query("UPDATE sessions SET ended_at_ms = ? WHERE id = ?")
        .bind(ended_at_ms)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Capture health of one session: how much of the feed was read and what
/// the layout machinery did. Persisted on the session row so a bad day shows
/// up on the site instead of only in a log file.
#[derive(Debug, Clone, Default)]
pub struct SessionHealth {
    pub frames: i64,
    pub parsed: i64,
    pub probing: i64,
    pub relocks: i64,
    pub counter_reads: i64,
    pub events: Vec<serde_json::Value>,
}

impl SessionHealth {
    /// Record a layout event (lock, switch, drift, geometry); capped so a
    /// pathological day can't grow the row without bound.
    pub fn event(&mut self, at_ms: i64, kind: &str, detail: impl Into<String>) {
        if self.events.len() < 400 {
            self.events
                .push(serde_json::json!({"t": at_ms, "k": kind, "d": detail.into()}));
        }
    }
}

pub async fn update_session_health(pool: &SqlitePool, id: i64, h: &SessionHealth) -> Result<()> {
    sqlx::query(
        "UPDATE sessions SET frames = ?, parsed = ?, probing = ?, relocks = ?, counter_reads = ?, events = ? \
         WHERE id = ?",
    )
    .bind(h.frames)
    .bind(h.parsed)
    .bind(h.probing)
    .bind(h.relocks)
    .bind(h.counter_reads)
    .bind(serde_json::Value::Array(h.events.clone()).to_string())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub id: i64,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub source: String,
    pub label: String,
    pub tag: Option<String>,
    pub attempts: i64,
    pub finished: i64,
    pub best_ms: Option<i64>,
    pub frames: Option<i64>,
    pub parsed: Option<i64>,
    pub probing: Option<i64>,
    pub relocks: Option<i64>,
    pub counter_reads: Option<i64>,
    pub events: Option<serde_json::Value>,
}

pub async fn recent_sessions(pool: &SqlitePool, limit: i64) -> Result<Vec<SessionSummary>> {
    let rows = sqlx::query(
        "SELECT s.id, s.started_at_ms, s.ended_at_ms, s.source, s.label, s.tag, \
         s.frames, s.parsed, s.probing, s.relocks, s.counter_reads, s.events, \
         COUNT(r.id) AS attempts, \
         COALESCE(SUM(CASE WHEN r.outcome = 'finished' THEN 1 ELSE 0 END), 0) AS finished, \
         MIN(CASE WHEN r.outcome = 'finished' THEN r.final_time_ms END) AS best_ms \
         FROM sessions s LEFT JOIN runs r ON r.session_id = s.id \
         GROUP BY s.id ORDER BY s.id DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SessionSummary {
            id: r.get("id"),
            started_at_ms: r.get("started_at_ms"),
            ended_at_ms: r.get("ended_at_ms"),
            source: r.get("source"),
            label: r.get("label"),
            tag: r.get("tag"),
            attempts: r.get("attempts"),
            finished: r.get("finished"),
            best_ms: r.get("best_ms"),
            frames: r.get("frames"),
            parsed: r.get("parsed"),
            probing: r.get("probing"),
            relocks: r.get("relocks"),
            counter_reads: r.get("counter_reads"),
            events: r
                .get::<Option<String>, _>("events")
                .and_then(|s| serde_json::from_str(&s).ok()),
        })
        .collect())
}

pub async fn next_attempt_number(pool: &SqlitePool, game: &str, category: &str) -> Result<i64> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE game = ? AND category = ?")
        .bind(game)
        .bind(category)
        .fetch_one(pool)
        .await?;
    Ok(n + 1)
}

pub async fn personal_best(
    pool: &SqlitePool,
    game: &str,
    category: &str,
) -> Result<Option<RunRow>> {
    let row = sqlx::query_as::<_, RunRow>(
        "SELECT * FROM runs WHERE game = ? AND category = ? AND outcome = 'finished' \
         AND final_time_ms IS NOT NULL ORDER BY final_time_ms ASC, id ASC LIMIT 1",
    )
    .bind(game)
    .bind(category)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn last_run(pool: &SqlitePool) -> Result<Option<RunRow>> {
    let row = sqlx::query_as::<_, RunRow>("SELECT * FROM runs ORDER BY id DESC LIMIT 1")
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TodayStats {
    pub attempts: i64,
    pub finished: i64,
    pub resets: i64,
    pub best_ms: Option<i64>,
}

pub async fn today_stats(
    pool: &SqlitePool,
    game: &str,
    category: &str,
    day_start_ms: i64,
) -> Result<TodayStats> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS attempts, \
         COALESCE(SUM(CASE WHEN outcome = 'finished' THEN 1 ELSE 0 END), 0) AS finished, \
         COALESCE(SUM(CASE WHEN outcome = 'reset' THEN 1 ELSE 0 END), 0) AS resets, \
         MIN(CASE WHEN outcome = 'finished' THEN final_time_ms END) AS best_ms \
         FROM runs WHERE game = ? AND category = ? AND started_at_ms >= ?",
    )
    .bind(game)
    .bind(category)
    .bind(day_start_ms)
    .fetch_one(pool)
    .await?;
    Ok(TodayStats {
        attempts: row.get("attempts"),
        finished: row.get("finished"),
        resets: row.get("resets"),
        best_ms: row.get("best_ms"),
    })
}

pub async fn total_attempts(pool: &SqlitePool, game: &str, category: &str) -> Result<i64> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE game = ? AND category = ?")
        .bind(game)
        .bind(category)
        .fetch_one(pool)
        .await?;
    Ok(n)
}

/// Overwrite the final time of the most recent run (mod `!correct`); also
/// flips its outcome to finished, since a corrected time implies completion.
pub async fn correct_last_run(pool: &SqlitePool, final_ms: i64) -> Result<Option<RunRow>> {
    let res = sqlx::query(
        "UPDATE runs SET final_time_ms = ?, outcome = 'finished', reset_reason = NULL \
         WHERE id = (SELECT MAX(id) FROM runs)",
    )
    .bind(final_ms)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Ok(None);
    }
    last_run(pool).await
}

/// Delete the most recent run (mod `!void`). Returns the deleted row.
pub async fn void_last_run(pool: &SqlitePool) -> Result<Option<RunRow>> {
    let Some(row) = last_run(pool).await? else {
        return Ok(None);
    };
    sqlx::query("DELETE FROM runs WHERE id = ?")
        .bind(row.id)
        .execute(pool)
        .await?;
    Ok(Some(row))
}

pub async fn log_transition(
    pool: &SqlitePool,
    at_ms: i64,
    from_phase: &str,
    to_phase: &str,
    game: &str,
    category: &str,
    detail: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO transitions (at_ms, from_phase, to_phase, game, category, detail) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(at_ms)
    .bind(from_phase)
    .bind(to_phase)
    .bind(game)
    .bind(category)
    .bind(detail)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_setting(pool: &SqlitePool, key: &str) -> Result<Option<String>> {
    let v: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(v)
}

pub async fn set_setting(pool: &SqlitePool, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES (?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// All runs for a game/category started at or after `since_ms`, in order.
pub async fn runs_since(
    pool: &SqlitePool,
    game: &str,
    category: &str,
    since_ms: i64,
) -> Result<Vec<RunRow>> {
    let rows = sqlx::query_as::<_, RunRow>(
        "SELECT * FROM runs WHERE game = ? AND category = ? AND started_at_ms >= ? ORDER BY id",
    )
    .bind(game)
    .bind(category)
    .bind(since_ms)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn recent_runs(pool: &SqlitePool, limit: i64) -> Result<Vec<RunRow>> {
    let rows = sqlx::query_as::<_, RunRow>("SELECT * FROM runs ORDER BY id DESC LIMIT ?")
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

#[derive(Debug, Clone, Serialize)]
pub struct DayStats {
    pub day: String,
    pub attempts: i64,
    pub finished: i64,
    pub resets: i64,
    pub best_ms: Option<i64>,
}

/// Per-local-day breakdown for one game/category, oldest first.
pub async fn daily_stats(pool: &SqlitePool, game: &str, category: &str) -> Result<Vec<DayStats>> {
    let rows = sqlx::query(
        "SELECT date(started_at_ms / 1000, 'unixepoch', 'localtime') AS day, \
         COUNT(*) AS attempts, \
         COALESCE(SUM(CASE WHEN outcome = 'finished' THEN 1 ELSE 0 END), 0) AS finished, \
         COALESCE(SUM(CASE WHEN outcome = 'reset' THEN 1 ELSE 0 END), 0) AS resets, \
         MIN(CASE WHEN outcome = 'finished' THEN final_time_ms END) AS best_ms \
         FROM runs WHERE game = ? AND category = ? GROUP BY day ORDER BY day",
    )
    .bind(game)
    .bind(category)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| DayStats {
            day: r.get("day"),
            attempts: r.get("attempts"),
            finished: r.get("finished"),
            resets: r.get("resets"),
            best_ms: r.get("best_ms"),
        })
        .collect())
}

/// Insert the recorded splits of a completed run. Segment times are derived
/// from consecutive cumulative values (only when the previous act is known).
pub async fn insert_splits(
    pool: &SqlitePool,
    run_id: i64,
    splits: &[crate::splits::RecordedSplit],
) -> Result<()> {
    for (n, s) in splits.iter().enumerate() {
        let segment_ms = if s.act_index == 0 {
            Some(s.cumulative_ms)
        } else {
            // previous recorded split must be exactly the preceding act
            splits[..n]
                .iter()
                .find(|p| p.act_index + 1 == s.act_index)
                .map(|p| s.cumulative_ms - p.cumulative_ms)
        };
        sqlx::query(
            "INSERT INTO splits (run_id, act_index, act_name, cumulative_ms, segment_ms) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(run_id)
        .bind(s.act_index as i64)
        .bind(&s.act_name)
        .bind(s.cumulative_ms)
        .bind(segment_ms)
        .execute(pool)
        .await?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct SplitRow {
    pub act_index: i64,
    pub act_name: String,
    pub cumulative_ms: i64,
    pub segment_ms: Option<i64>,
}

/// Splits (with their run ids) for all runs of a game/category started at or
/// after `since_ms` — feeds the site's per-run detail view.
pub async fn splits_since(
    pool: &SqlitePool,
    game: &str,
    category: &str,
    since_ms: i64,
) -> Result<Vec<(i64, SplitRow)>> {
    let rows = sqlx::query(
        "SELECT s.run_id, s.act_index, s.act_name, s.cumulative_ms, s.segment_ms \
         FROM splits s JOIN runs r ON r.id = s.run_id \
         WHERE r.game = ? AND r.category = ? AND r.started_at_ms >= ? \
         ORDER BY s.run_id, s.act_index",
    )
    .bind(game)
    .bind(category)
    .bind(since_ms)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.get("run_id"),
                SplitRow {
                    act_index: r.get("act_index"),
                    act_name: r.get("act_name"),
                    cumulative_ms: r.get("cumulative_ms"),
                    segment_ms: r.get("segment_ms"),
                },
            )
        })
        .collect())
}

pub async fn run_splits(pool: &SqlitePool, run_id: i64) -> Result<Vec<SplitRow>> {
    let rows = sqlx::query(
        "SELECT act_index, act_name, cumulative_ms, segment_ms FROM splits \
         WHERE run_id = ? ORDER BY act_index",
    )
    .bind(run_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SplitRow {
            act_index: r.get("act_index"),
            act_name: r.get("act_name"),
            cumulative_ms: r.get("cumulative_ms"),
            segment_ms: r.get("segment_ms"),
        })
        .collect())
}

#[derive(Debug, Clone, Serialize)]
pub struct Gold {
    pub act_index: i64,
    pub act_name: String,
    pub gold_ms: i64,
    pub samples: i64,
    /// When the gold was set (start of the run that holds it).
    pub set_at_ms: i64,
}

/// Best (gold) segment per act across all recorded runs of a game/category,
/// with the date each gold was set.
pub async fn golds(pool: &SqlitePool, game: &str, category: &str) -> Result<Vec<Gold>> {
    let rows = sqlx::query(
        "SELECT act_index, act_name, segment_ms AS gold_ms, started_at_ms AS set_at_ms, cnt AS samples \
         FROM ( \
           SELECT s.act_index, s.act_name, s.segment_ms, r.started_at_ms, \
                  ROW_NUMBER() OVER (PARTITION BY s.act_index \
                                     ORDER BY s.segment_ms ASC, r.started_at_ms ASC) AS rn, \
                  COUNT(*) OVER (PARTITION BY s.act_index) AS cnt \
           FROM splits s \
           JOIN runs r ON r.id = s.run_id \
           /* per-act means in one pass; a correlated subquery here rescanned \
              every split for every candidate row */ \
           JOIN (SELECT s3.act_index AS act_index, AVG(s3.segment_ms) AS mean \
                 FROM splits s3 JOIN runs r3 ON r3.id = s3.run_id \
                 WHERE r3.game = ? AND r3.category = ? AND s3.segment_ms IS NOT NULL \
                 GROUP BY s3.act_index) avg ON avg.act_index = s.act_index \
           WHERE r.game = ? AND r.category = ? AND s.segment_ms IS NOT NULL \
             /* a segment under 60% of the act's average is a misread column, \
                not a gold (nobody runs an act 40% faster than their norm) */ \
             AND s.segment_ms >= 0.6 * avg.mean \
         ) WHERE rn = 1 ORDER BY act_index",
    )
    .bind(game)
    .bind(category)
    .bind(game)
    .bind(category)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Gold {
            act_index: r.get("act_index"),
            act_name: r.get("act_name"),
            gold_ms: r.get("gold_ms"),
            samples: r.get("samples"),
            set_at_ms: r.get("set_at_ms"),
        })
        .collect())
}

/// All runs for a game/category in insertion order, trimmed for stats.
pub async fn runs_brief(
    pool: &SqlitePool,
    game: &str,
    category: &str,
) -> Result<Vec<crate::stats::RunBrief>> {
    let rows = sqlx::query(
        // Chronological, not insertion order: imports and backfills add older
        // days after newer ones, and everything derived from this sequence
        // (PB progression, streaks, survival) is only meaningful in time order.
        "SELECT started_at_ms, attempt_number, ls_attempt, outcome, final_time_ms, last_timer_ms \
         FROM runs WHERE game = ? AND category = ? ORDER BY started_at_ms, id",
    )
    .bind(game)
    .bind(category)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| crate::stats::RunBrief {
            started_at_ms: r.get("started_at_ms"),
            attempt_number: r.get("attempt_number"),
            ls_attempt: r.get("ls_attempt"),
            finished: r.get::<String, _>("outcome") == OUTCOME_FINISHED,
            final_time_ms: r.get("final_time_ms"),
            last_timer_ms: r.get("last_timer_ms"),
        })
        .collect())
}

#[derive(Debug, Clone, Serialize)]
pub struct GameSummary {
    pub game: String,
    pub category: String,
    pub best_ms: Option<i64>,
    pub finished: i64,
    pub attempts: i64,
}

pub async fn summaries(pool: &SqlitePool) -> Result<Vec<GameSummary>> {
    let rows = sqlx::query(
        "SELECT game, category, \
         MIN(CASE WHEN outcome = 'finished' THEN final_time_ms END) AS best_ms, \
         COALESCE(SUM(CASE WHEN outcome = 'finished' THEN 1 ELSE 0 END), 0) AS finished, \
         COUNT(*) AS attempts \
         FROM runs GROUP BY game, category ORDER BY game, category",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| GameSummary {
            game: r.get("game"),
            category: r.get("category"),
            best_ms: r.get("best_ms"),
            finished: r.get("finished"),
            attempts: r.get("attempts"),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let pool = open(path.to_str().unwrap()).await.unwrap();
        (dir, pool)
    }

    fn run(game: &'static str, attempt: i64, start: i64, final_ms: Option<i64>) -> NewRun<'static> {
        NewRun {
            game,
            category: "Any%",
            attempt_number: attempt,
            started_at_ms: start,
            ended_at_ms: start + 100_000,
            outcome: if final_ms.is_some() {
                OUTCOME_FINISHED
            } else {
                OUTCOME_RESET
            },
            reset_reason: if final_ms.is_some() {
                None
            } else {
                Some("zeroed")
            },
            final_time_ms: final_ms,
            last_timer_ms: final_ms.or(Some(42_000)),
            session_id: None,
            ls_attempt: None,
        }
    }

    #[tokio::test]
    async fn pb_and_attempt_numbers() {
        let (_dir, pool) = test_pool().await;
        assert_eq!(next_attempt_number(&pool, "smb", "Any%").await.unwrap(), 1);
        insert_run(&pool, run("smb", 1, 1000, Some(300_000)))
            .await
            .unwrap();
        insert_run(&pool, run("smb", 2, 2000, None)).await.unwrap();
        insert_run(&pool, run("smb", 3, 3000, Some(295_000)))
            .await
            .unwrap();
        assert_eq!(next_attempt_number(&pool, "smb", "Any%").await.unwrap(), 4);
        let pb = personal_best(&pool, "smb", "Any%").await.unwrap().unwrap();
        assert_eq!(pb.final_time_ms, Some(295_000));
        assert_eq!(pb.attempt_number, 3);
        assert!(personal_best(&pool, "other", "Any%")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn today_stats_filters_by_day_start() {
        let (_dir, pool) = test_pool().await;
        insert_run(&pool, run("smb", 1, 500, Some(310_000)))
            .await
            .unwrap(); // "yesterday"
        insert_run(&pool, run("smb", 2, 5000, None)).await.unwrap();
        insert_run(&pool, run("smb", 3, 6000, Some(290_000)))
            .await
            .unwrap();
        let t = today_stats(&pool, "smb", "Any%", 1000).await.unwrap();
        assert_eq!(t.attempts, 2);
        assert_eq!(t.finished, 1);
        assert_eq!(t.resets, 1);
        assert_eq!(t.best_ms, Some(290_000));
        let empty = today_stats(&pool, "smb", "Any%", 99_999).await.unwrap();
        assert_eq!(empty.attempts, 0);
        assert_eq!(empty.best_ms, None);
    }

    #[tokio::test]
    async fn correct_and_void_last_run() {
        let (_dir, pool) = test_pool().await;
        assert!(correct_last_run(&pool, 1).await.unwrap().is_none());
        insert_run(&pool, run("smb", 1, 1000, None)).await.unwrap();
        let fixed = correct_last_run(&pool, 123_400).await.unwrap().unwrap();
        assert_eq!(fixed.final_time_ms, Some(123_400));
        assert_eq!(fixed.outcome, OUTCOME_FINISHED);
        assert_eq!(fixed.reset_reason, None);

        let gone = void_last_run(&pool).await.unwrap().unwrap();
        assert_eq!(gone.id, fixed.id);
        assert!(last_run(&pool).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn session_lifecycle_and_summary() {
        let (_dir, pool) = test_pool().await;
        let sid = open_session(&pool, 1000, "hls", "somechannel", None)
            .await
            .unwrap();
        let mut r = run("smb", 1, 2000, Some(300_000));
        r.session_id = Some(sid);
        insert_run(&pool, r).await.unwrap();
        insert_run(&pool, run("smb", 2, 3000, None)).await.unwrap(); // no session
        close_session(&pool, sid, 10_000).await.unwrap();

        let s = recent_sessions(&pool, 5).await.unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].id, sid);
        assert_eq!(s[0].ended_at_ms, Some(10_000));
        assert_eq!(s[0].attempts, 1);
        assert_eq!(s[0].finished, 1);
        assert_eq!(s[0].best_ms, Some(300_000));
        assert_eq!(s[0].label, "somechannel");
    }

    #[tokio::test]
    async fn settings_roundtrip() {
        let (_dir, pool) = test_pool().await;
        assert_eq!(get_setting(&pool, "game").await.unwrap(), None);
        set_setting(&pool, "game", "SMB1").await.unwrap();
        set_setting(&pool, "game", "SMB3").await.unwrap();
        assert_eq!(
            get_setting(&pool, "game").await.unwrap().as_deref(),
            Some("SMB3")
        );
    }

    #[tokio::test]
    async fn summaries_group_by_game() {
        let (_dir, pool) = test_pool().await;
        insert_run(&pool, run("smb", 1, 1000, Some(300_000)))
            .await
            .unwrap();
        insert_run(&pool, run("smb", 2, 2000, None)).await.unwrap();
        insert_run(&pool, run("zelda", 1, 3000, None))
            .await
            .unwrap();
        let s = summaries(&pool).await.unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].game, "smb");
        assert_eq!(s[0].attempts, 2);
        assert_eq!(s[0].best_ms, Some(300_000));
        assert_eq!(s[1].game, "zelda");
        assert_eq!(s[1].best_ms, None);
    }
}
