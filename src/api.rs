//! The per-day feed: `report --api-dir <dir>` writes `days/<YYYY-MM-DD>.json`
//! (every run of a broadcast day with its splits, the day's sessions with
//! their capture health), `history.json` (per-day stats and every finish,
//! what the charts need without the runs), `schema.json` (generated from the
//! structs below, so it cannot drift from them) and, last, `manifest.json`,
//! which lists the day files with their byte size and sha256. A reader
//! fetches the manifest, follows `days[]`, and re-fetches only what changed.
//!
//! These structs are the feed's contract (site/static/api/v1/README.md):
//! within a version fields are only added. Day files carry no timestamp and
//! their rows are sorted, so a closed day's bytes do not change between
//! builds unless its database rows did — that is what lets the deploy cache
//! them as immutable and a reader trust the manifest's sha256. Nothing here
//! names the streamer: sessions lose their `label` (a channel name or a file
//! path) and keep their VOD id only where the config publishes VOD links.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::SecondsFormat;
use schemars::JsonSchema;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::config::Config;
use crate::{db, util};

pub const SCHEMA_VERSION: u32 = 1;
/// How long a reader may trust a manifest: the live cron rebuilds every ten
/// minutes, so anything older than this was not rebuilt by a live build.
pub const STALE_AFTER_S: u32 = 900;

/// What the deployment publishes and what it calls things; from the config.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Publish sessions' Twitch VOD ids (a VOD id names the channel).
    pub public_vod_links: bool,
    /// What the tracked best is called ("season best").
    pub record_label: String,
    /// The configured time to beat, standing in until the layout's own
    /// season-best row has been read.
    pub baseline_best_ms: Option<i64>,
    /// Configured reference times (label, ms), in config order.
    pub references: Vec<(String, i64)>,
}

impl Options {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            public_vod_links: cfg.game.public_vod_links,
            record_label: cfg.game.record_label.clone(),
            baseline_best_ms: cfg.game.baseline_best_ms(),
            references: cfg
                .game
                .references
                .iter()
                .filter_map(|r| r.ms().map(|ms| (r.label.clone(), ms)))
                .collect(),
        }
    }
}

// ---- the documents

/// One broadcast day: `days/<day>.json`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DayFile {
    /// The streamer's local day, YYYY-MM-DD.
    pub day: String,
    /// The day is behind today and every session in it has ended. A closed
    /// file's bytes only change when the database rows behind it are edited
    /// (a VOD re-import, a corrected time); compare `sha256` in the manifest.
    pub closed: bool,
    pub stats: DayStats,
    /// Sessions that started on this day or recorded a run on it, oldest
    /// first, so every `runs[].session_id` resolves within the file. Their
    /// counts span the whole session, not only this day.
    pub sessions: Vec<Session>,
    /// Every attempt that started on this day, oldest first.
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, JsonSchema)]
pub struct DayStats {
    /// Attempts the bot captured.
    pub attempts: i64,
    pub finished: i64,
    pub resets: i64,
    /// The fastest finish, ms; null without one.
    pub best_ms: Option<i64>,
    /// The runner's own LiveSplit attempt counter at the first and last
    /// captured run with a number: `last_no - first_no + 1` is how many
    /// attempts he really made, the honest denominator for `attempts`.
    pub first_no: Option<i64>,
    pub last_no: Option<i64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Session {
    /// Database id; `runs[].session_id` refers to it. Not stable across
    /// re-imports, unlike a run's `id`.
    pub id: i64,
    pub started_at_ms: i64,
    /// Null while the broadcast is ongoing.
    pub ended_at_ms: Option<i64>,
    /// "hls" for live capture, "vod" for a VOD re-analysis.
    pub source: String,
    /// A marathon or event tag, when the operator set one.
    pub tag: Option<String>,
    /// Runs recorded over the whole session.
    pub attempts: i64,
    pub finished: i64,
    pub best_ms: Option<i64>,
    /// Capture health: frames analysed.
    pub frames: Option<i64>,
    /// Frames whose timer was read.
    pub parsed: Option<i64>,
    /// Frames spent without a locked layout.
    pub probing: Option<i64>,
    /// Layout locks, switches and re-anchors.
    pub relocks: Option<i64>,
    /// Attempt-counter reads accepted.
    pub counter_reads: Option<i64>,
    /// Diagnostic layout events, `[{t, k, d}]`: time (ms), kind, detail.
    pub events: Option<serde_json::Value>,
    /// The broadcast's Twitch VOD, present only where the deployment
    /// publishes VOD links and the VOD is known; a run sits at
    /// `started_at_ms - vod_created_at_ms` into it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vod_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vod_created_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Run {
    /// The run's key: its `started_at_ms`. Unique, and the one field a VOD
    /// re-import preserves; database ids are not published.
    pub id: i64,
    /// The bot's own count of the runs it saw (the tracked ordinal),
    /// renumbered chronologically by imports. Only a fallback for naming a
    /// run when `ls_attempt` is null.
    pub attempt_number: i64,
    /// The runner's own LiveSplit attempt counter, read off the layout while
    /// the run was in progress: the number he and the site use. Null when it
    /// was not read.
    pub ls_attempt: Option<i64>,
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
    /// "finished" or "reset".
    pub outcome: String,
    /// How a reset run ended: "zeroed", "tooshort", "desync" or
    /// "disappeared"; null for finishes.
    pub reset_reason: Option<String>,
    /// The finish time, ms; null for resets.
    pub final_time_ms: Option<i64>,
    /// The last timer value seen: where a reset died.
    pub last_timer_ms: Option<i64>,
    /// The `sessions[].id` the run was recorded in.
    pub session_id: Option<i64>,
    /// Per-act splits by cumulative time, in act order, where they were
    /// read; the final act's split is the finish time.
    pub splits: Vec<Split>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Split {
    pub act_index: i64,
    pub act_name: String,
    pub cumulative_ms: i64,
}

/// `history.json`: per-day stats and every finish — what the charts need
/// without the runs.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct History {
    /// One row per broadcast day with captured runs, oldest first.
    pub days: Vec<HistoryDay>,
    /// Every finished run, oldest first.
    pub finishes: Vec<Finish>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HistoryDay {
    /// The streamer's local day, YYYY-MM-DD; `days/<day>.json` has the runs.
    pub day: String,
    #[serde(flatten)]
    pub stats: DayStats,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Finish {
    pub attempt_number: i64,
    pub ls_attempt: Option<i64>,
    pub started_at_ms: i64,
    pub final_time_ms: i64,
}

/// `manifest.json`, written last: where a reader starts.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Manifest {
    pub schema_version: u32,
    /// When this build ran, unix ms; `generated_at` is the same instant in
    /// ISO 8601 (UTC).
    pub generated_at_ms: i64,
    pub generated_at: String,
    /// Treat the manifest as stale when it is older than this many seconds.
    pub stale_after_s: u32,
    /// The streamer's local day at build time; that day's file is the live
    /// one and is never `closed`.
    pub today: String,
    /// The local UTC offset in force at build time, minutes. Days are the
    /// streamer's, not the reader's.
    pub day_offset_minutes: i64,
    pub game: String,
    pub category: String,
    /// What the tracked best is called ("season best").
    pub record_label: String,
    pub attempt_numbering: AttemptNumbering,
    /// The times to beat, each with its scope and where it was read.
    pub records: Vec<Record>,
    /// The bot's most recent state-machine transition; null before the first.
    pub last_transition: Option<Transition>,
    /// One entry per day file, sorted by day.
    pub days: Vec<DayEntry>,
    pub files: Files,
}

/// The two attempt numbers, explained.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AttemptNumbering {
    pub livesplit_attempt: String,
    pub tracked_ordinal: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Record {
    pub label: String,
    pub ms: i64,
    pub scope: Scope,
    pub source: Source,
}

/// "season": the runner's comparison, which resets with each season of his
/// splits file; "lifetime": all time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Season,
    Lifetime,
}

/// "layout": read off the LiveSplit layout's own reference rows, which the
/// runner keeps current; "config": the deployment's configured value,
/// standing in until the layout has been read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Layout,
    Config,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Transition {
    pub at_ms: i64,
    #[serde(rename = "from")]
    pub from_phase: String,
    #[serde(rename = "to")]
    pub to_phase: String,
    pub game: String,
    pub category: String,
    /// Free-form detail, e.g. "final_ms=696810".
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct DayEntry {
    pub day: String,
    /// Relative to the manifest: "days/<day>.json".
    pub path: String,
    /// See `DayFile.closed`: a closed file is served as immutable.
    pub closed: bool,
    pub bytes: u64,
    /// SHA-256 of the file's bytes, lowercase hex.
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Files {
    pub history: FileEntry,
    pub schema: FileEntry,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FileEntry {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

/// Not a file: the shapes of the feed's documents, one per property, for
/// `schema.json`.
#[derive(JsonSchema)]
#[allow(dead_code)]
pub struct Documents {
    pub manifest: Manifest,
    pub day: DayFile,
    pub history: History,
}

// ---- building

/// One build, serialized: the bytes are hashed for the manifest before
/// anything touches disk, so the manifest describes exactly what is written.
pub struct Feed {
    /// Sorted by day.
    pub days: Vec<BuiltDay>,
    pub history: Vec<u8>,
    pub schema: Vec<u8>,
    pub manifest: Manifest,
}

pub struct BuiltDay {
    pub day: String,
    pub closed: bool,
    pub bytes: Vec<u8>,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// A day is closed when it is behind `today` and no session in it is still
/// open. `today` is the streamer's local day, as SQLite names it.
pub fn is_closed(day: &str, today: &str, sessions: &[Session]) -> bool {
    day < today && sessions.iter().all(|s| s.ended_at_ms.is_some())
}

fn day_stats(runs: &[Run]) -> DayStats {
    DayStats {
        attempts: runs.len() as i64,
        finished: runs
            .iter()
            .filter(|r| r.outcome == db::OUTCOME_FINISHED)
            .count() as i64,
        resets: runs
            .iter()
            .filter(|r| r.outcome == db::OUTCOME_RESET)
            .count() as i64,
        best_ms: runs
            .iter()
            .filter(|r| r.outcome == db::OUTCOME_FINISHED)
            .filter_map(|r| r.final_time_ms)
            .min(),
        first_no: runs.iter().filter_map(|r| r.ls_attempt).min(),
        last_no: runs.iter().filter_map(|r| r.ls_attempt).max(),
    }
}

fn public_session(s: &db::SessionSummary, public_vod_links: bool) -> Session {
    Session {
        id: s.id,
        started_at_ms: s.started_at_ms,
        ended_at_ms: s.ended_at_ms,
        source: s.source.clone(),
        tag: s.tag.clone(),
        attempts: s.attempts,
        finished: s.finished,
        best_ms: s.best_ms,
        frames: s.frames,
        parsed: s.parsed,
        probing: s.probing,
        relocks: s.relocks,
        counter_reads: s.counter_reads,
        events: s.events.clone(),
        vod_id: s.vod_id.clone().filter(|_| public_vod_links),
        vod_created_at_ms: s.vod_created_at_ms.filter(|_| public_vod_links),
    }
}

async fn setting_ms(pool: &SqlitePool, key: &str) -> Result<Option<i64>> {
    Ok(db::get_setting(pool, key)
        .await?
        .and_then(|s| s.parse::<i64>().ok()))
}

/// The times to beat: the season best (the layout's own row when read, else
/// the configured baseline), then the configured references with the
/// layout's WR and lifetime PB replacing those of the same label — the same
/// precedence `report --json` and the site apply.
async fn records(pool: &SqlitePool, opts: &Options) -> Result<Vec<Record>> {
    let mut out = Vec::new();
    match setting_ms(pool, "ls_season_best_ms").await? {
        Some(ms) => out.push(Record {
            label: opts.record_label.clone(),
            ms,
            scope: Scope::Season,
            source: Source::Layout,
        }),
        None => {
            if let Some(ms) = opts.baseline_best_ms {
                out.push(Record {
                    label: opts.record_label.clone(),
                    ms,
                    scope: Scope::Season,
                    source: Source::Config,
                });
            }
        }
    }
    let mut refs: Vec<Record> = opts
        .references
        .iter()
        .map(|(label, ms)| Record {
            label: label.clone(),
            ms: *ms,
            scope: Scope::Lifetime,
            source: Source::Config,
        })
        .collect();
    for (key, label) in [("ls_wr_ms", "WR"), ("ls_pb_ms", "Lifetime PB")] {
        if let Some(ms) = setting_ms(pool, key).await? {
            refs.retain(|r| r.label != label);
            refs.push(Record {
                label: label.to_string(),
                ms,
                scope: Scope::Lifetime,
                source: Source::Layout,
            });
        }
    }
    out.extend(refs);
    Ok(out)
}

/// Build the whole feed for one game/category. `today` is the streamer's
/// local day (`db::local_today`) and `now_ms` the build time; both are
/// parameters so a test can pin them.
pub async fn build(
    pool: &SqlitePool,
    game: &str,
    category: &str,
    opts: &Options,
    today: &str,
    now_ms: i64,
) -> Result<Feed> {
    // The run's published id is its start time; a collision would publish
    // two runs under one key, so refuse to build rather than pick one.
    let dups = db::duplicate_start_times(pool, game, category).await?;
    if !dups.is_empty() {
        let list: Vec<String> = dups
            .iter()
            .take(5)
            .map(|(ms, n)| format!("{ms} x{n}"))
            .collect();
        bail!(
            "{} started_at_ms value(s) shared by more than one run ({}); the feed keys runs by \
             started_at_ms and cannot be built",
            dups.len(),
            list.join(", ")
        );
    }

    let mut splits_by_run: HashMap<i64, Vec<Split>> = HashMap::new();
    for (run_id, s) in db::splits_since(pool, game, category, 0).await? {
        splits_by_run.entry(run_id).or_default().push(Split {
            act_index: s.act_index,
            act_name: s.act_name,
            cumulative_ms: s.cumulative_ms,
        });
    }
    let sessions: HashMap<i64, db::SessionSummary> = db::recent_sessions(pool, 100_000)
        .await?
        .into_iter()
        .map(|s| (s.id, s))
        .collect();

    // Group by the local day SQLite names, exactly as daily_stats does.
    #[derive(Default)]
    struct Group {
        runs: Vec<Run>,
        sessions: BTreeSet<i64>,
    }
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for (day, r) in db::runs_with_day(pool, game, category).await? {
        let g = groups.entry(day).or_default();
        if let Some(sid) = r.session_id {
            if sessions.contains_key(&sid) {
                g.sessions.insert(sid);
            }
        }
        let mut splits = splits_by_run.remove(&r.id).unwrap_or_default();
        splits.sort_by_key(|s| s.act_index);
        g.runs.push(Run {
            id: r.started_at_ms,
            attempt_number: r.attempt_number,
            ls_attempt: r.ls_attempt,
            started_at_ms: r.started_at_ms,
            ended_at_ms: r.ended_at_ms,
            outcome: r.outcome,
            reset_reason: r.reset_reason,
            final_time_ms: r.final_time_ms,
            last_timer_ms: r.last_timer_ms,
            session_id: r.session_id,
            splits,
        });
    }
    for (sid, day) in db::session_days(pool).await? {
        if sessions.contains_key(&sid) {
            groups.entry(day).or_default().sessions.insert(sid);
        }
    }

    let mut days = Vec::with_capacity(groups.len());
    let mut history_days = Vec::new();
    let mut finishes = Vec::new();
    for (day, mut g) in groups {
        g.runs.sort_by_key(|r| r.started_at_ms);
        let mut day_sessions: Vec<Session> = g
            .sessions
            .iter()
            .map(|sid| public_session(&sessions[sid], opts.public_vod_links))
            .collect();
        day_sessions.sort_by_key(|s| (s.started_at_ms, s.id));
        let stats = day_stats(&g.runs);
        if stats.attempts > 0 {
            history_days.push(HistoryDay {
                day: day.clone(),
                stats: stats.clone(),
            });
        }
        finishes.extend(
            g.runs
                .iter()
                .filter(|r| r.outcome == db::OUTCOME_FINISHED)
                .filter_map(|r| {
                    r.final_time_ms.map(|ms| Finish {
                        attempt_number: r.attempt_number,
                        ls_attempt: r.ls_attempt,
                        started_at_ms: r.started_at_ms,
                        final_time_ms: ms,
                    })
                }),
        );
        let closed = is_closed(&day, today, &day_sessions);
        let doc = DayFile {
            day: day.clone(),
            closed,
            stats,
            sessions: day_sessions,
            runs: g.runs,
        };
        days.push(BuiltDay {
            day,
            closed,
            bytes: serde_json::to_vec(&doc)?,
        });
    }
    finishes.sort_by_key(|f| f.started_at_ms);

    let history = serde_json::to_vec(&History {
        days: history_days,
        finishes,
    })?;
    let schema = serde_json::to_vec_pretty(&schemars::schema_for!(Documents))?;

    let entry = |path: &str, bytes: &[u8]| FileEntry {
        path: path.to_string(),
        bytes: bytes.len() as u64,
        sha256: sha256_hex(bytes),
    };
    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        generated_at_ms: now_ms,
        generated_at: chrono::DateTime::from_timestamp_millis(now_ms)
            .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true))
            .unwrap_or_default(),
        stale_after_s: STALE_AFTER_S,
        today: today.to_string(),
        day_offset_minutes: util::local_utc_offset_minutes(),
        game: game.to_string(),
        category: category.to_string(),
        record_label: opts.record_label.clone(),
        attempt_numbering: AttemptNumbering {
            livesplit_attempt: "the runner's own LiveSplit attempt counter, read off the layout; \
                                the number the runner and the site use"
                .into(),
            tracked_ordinal: "the bot's own count of runs it saw; only a fallback when the \
                              counter was not read"
                .into(),
        },
        records: records(pool, opts).await?,
        last_transition: db::last_transition(pool).await?.map(|t| Transition {
            at_ms: t.at_ms,
            from_phase: t.from_phase,
            to_phase: t.to_phase,
            game: t.game,
            category: t.category,
            detail: t.detail,
        }),
        days: days
            .iter()
            .map(|d| DayEntry {
                day: d.day.clone(),
                path: format!("days/{}.json", d.day),
                closed: d.closed,
                bytes: d.bytes.len() as u64,
                sha256: sha256_hex(&d.bytes),
            })
            .collect(),
        files: Files {
            history: entry("history.json", &history),
            schema: entry("schema.json", &schema),
        },
    };
    Ok(Feed {
        days,
        history,
        schema,
        manifest,
    })
}

// ---- writing

pub struct Written {
    pub files: usize,
    pub bytes: u64,
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("renaming {} into place", tmp.display()))?;
    Ok(())
}

/// Write the feed under `dir`: the day files and `history.json` and
/// `schema.json` first, `manifest.json` last, each replaced atomically, so
/// a reader (or an interrupted build) never sees a manifest that points at
/// bytes not yet on disk. Files of days no longer in the feed are left where
/// they are; the manifest is the index and the deploy follows it.
pub fn write(feed: &Feed, dir: &Path) -> Result<Written> {
    let days_dir = dir.join("days");
    fs::create_dir_all(&days_dir).with_context(|| format!("creating {}", days_dir.display()))?;
    let mut written = Written { files: 0, bytes: 0 };
    let mut put = |path: &Path, bytes: &[u8]| -> Result<()> {
        write_atomic(path, bytes)?;
        written.files += 1;
        written.bytes += bytes.len() as u64;
        Ok(())
    };
    for d in &feed.days {
        put(&days_dir.join(format!("{}.json", d.day)), &d.bytes)?;
    }
    put(&dir.join("history.json"), &feed.history)?;
    put(&dir.join("schema.json"), &feed.schema)?;
    put(
        &dir.join("manifest.json"),
        &serde_json::to_vec(&feed.manifest)?,
    )?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{NewRun, OUTCOME_FINISHED, OUTCOME_RESET};
    use chrono::{Local, TimeZone};

    const GAME: &str = "smb";
    const CAT: &str = "Any%";
    const FAR_FUTURE: &str = "2999-01-01";

    async fn test_pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        (dir, pool)
    }

    /// Local noon of a date: safely inside one local day whatever the zone.
    fn noon(y: i32, m: u32, d: u32) -> i64 {
        Local
            .with_ymd_and_hms(y, m, d, 12, 0, 0)
            .unwrap()
            .timestamp_millis()
    }

    async fn add_run(
        pool: &SqlitePool,
        session: Option<i64>,
        n: i64,
        start: i64,
        ls: Option<i64>,
        fin: Option<i64>,
    ) -> i64 {
        db::insert_run(
            pool,
            NewRun {
                game: GAME,
                category: CAT,
                attempt_number: n,
                started_at_ms: start,
                ended_at_ms: start + fin.unwrap_or(50_000),
                outcome: if fin.is_some() {
                    OUTCOME_FINISHED
                } else {
                    OUTCOME_RESET
                },
                reset_reason: if fin.is_some() { None } else { Some("zeroed") },
                final_time_ms: fin,
                last_timer_ms: fin.or(Some(50_000)),
                session_id: session,
                ls_attempt: ls,
            },
        )
        .await
        .unwrap()
    }

    fn opts() -> Options {
        Options {
            public_vod_links: false,
            record_label: "season best".into(),
            baseline_best_ms: Some(800_000),
            references: vec![("WR".into(), 600_000), ("Lifetime PB".into(), 650_000)],
        }
    }

    /// Two broadcast days with a session each, one closed and one still
    /// open, and a session on a third day that recorded nothing.
    async fn fixture(pool: &SqlitePool) -> (i64, i64) {
        let d1 = noon(2026, 6, 15);
        let s1 = db::open_session(pool, d1 - 3_600_000, "hls", "somechannel", None, None)
            .await
            .unwrap();
        let r1 = add_run(pool, Some(s1), 1, d1, Some(100), Some(700_000)).await;
        db::insert_splits(
            pool,
            r1,
            &[
                crate::splits::RecordedSplit {
                    act_index: 0,
                    act_name: "Act 1".into(),
                    cumulative_ms: 55_000,
                },
                crate::splits::RecordedSplit {
                    act_index: 1,
                    act_name: "Act 2".into(),
                    cumulative_ms: 700_000,
                },
            ],
        )
        .await
        .unwrap();
        add_run(pool, Some(s1), 2, d1 + 900_000, Some(101), None).await;
        db::close_session(pool, s1, d1 + 3_600_000).await.unwrap();
        // A session that started the next day and captured no run.
        let idle = db::open_session(pool, noon(2026, 6, 16), "hls", "somechannel", None, None)
            .await
            .unwrap();
        db::close_session(pool, idle, noon(2026, 6, 16) + 60_000)
            .await
            .unwrap();
        // Still broadcasting two days later.
        let d3 = noon(2026, 6, 17);
        let s3 = db::open_session(pool, d3, "hls", "somechannel", None, None)
            .await
            .unwrap();
        add_run(pool, Some(s3), 3, d3 + 60_000, None, None).await;
        (s1, s3)
    }

    fn parse(bytes: &[u8]) -> serde_json::Value {
        serde_json::from_slice(bytes).unwrap()
    }

    #[tokio::test]
    async fn days_follow_the_local_day_and_close_only_behind_today() {
        let (_dir, pool) = test_pool().await;
        let (s1, s3) = fixture(&pool).await;
        let feed = build(&pool, GAME, CAT, &opts(), FAR_FUTURE, 1_000)
            .await
            .unwrap();
        let days: Vec<&str> = feed.days.iter().map(|d| d.day.as_str()).collect();
        assert_eq!(days, ["2026-06-15", "2026-06-16", "2026-06-17"]);
        assert_eq!(
            feed.days.iter().map(|d| d.closed).collect::<Vec<_>>(),
            [true, true, false],
            "the open session keeps its day open however far behind today it is"
        );

        let d15 = parse(&feed.days[0].bytes);
        assert_eq!(d15["day"], "2026-06-15");
        assert_eq!(d15["closed"], true);
        assert_eq!(
            d15["stats"],
            serde_json::json!({"attempts": 2, "finished": 1, "resets": 1,
                               "best_ms": 700_000, "first_no": 100, "last_no": 101})
        );
        assert_eq!(d15["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(d15["sessions"][0]["id"], s1);
        assert!(d15["sessions"][0].get("label").is_none());
        let runs = d15["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0]["id"], runs[0]["started_at_ms"]);
        assert_eq!(runs[0]["splits"][1]["cumulative_ms"], 700_000);
        assert_eq!(runs[1]["splits"].as_array().unwrap().len(), 0);
        assert_eq!(runs[1]["session_id"], s1);

        let d16 = parse(&feed.days[1].bytes);
        assert_eq!(d16["stats"]["attempts"], 0);
        assert_eq!(d16["stats"]["best_ms"], serde_json::Value::Null);
        assert_eq!(d16["runs"].as_array().unwrap().len(), 0);
        assert_eq!(d16["sessions"].as_array().unwrap().len(), 1);

        let d17 = parse(&feed.days[2].bytes);
        assert_eq!(d17["sessions"][0]["id"], s3);
        assert_eq!(d17["sessions"][0]["ended_at_ms"], serde_json::Value::Null);

        // The manifest agrees with the files, and today is never closed.
        let m = &feed.manifest;
        assert_eq!(m.days.len(), 3);
        assert_eq!(m.days[0].path, "days/2026-06-15.json");
        assert_eq!(m.today, FAR_FUTURE);
        let same_day = build(&pool, GAME, CAT, &opts(), "2026-06-15", 1_000)
            .await
            .unwrap();
        assert!(!same_day.days[0].closed, "today is never closed");
        assert!(!same_day.days[1].closed, "nor is a day after today");

        // history.json: the days with runs, with the numbers daily_stats gives.
        let h = parse(&feed.history);
        let daily = db::daily_stats(&pool, GAME, CAT).await.unwrap();
        let hd = h["days"].as_array().unwrap();
        assert_eq!(hd.len(), daily.len());
        for (row, d) in hd.iter().zip(&daily) {
            assert_eq!(row["day"], d.day);
            assert_eq!(row["attempts"], d.attempts);
            assert_eq!(row["finished"], d.finished);
            assert_eq!(row["resets"], d.resets);
            assert_eq!(row["best_ms"], serde_json::json!(d.best_ms));
            assert_eq!(row["first_no"], serde_json::json!(d.first_no));
            assert_eq!(row["last_no"], serde_json::json!(d.last_no));
        }
        assert_eq!(h["finishes"].as_array().unwrap().len(), 1);
        assert_eq!(h["finishes"][0]["ls_attempt"], 100);
        assert_eq!(h["finishes"][0]["final_time_ms"], 700_000);
    }

    #[tokio::test]
    async fn run_ids_are_start_times_and_must_be_unique() {
        let (_dir, pool) = test_pool().await;
        fixture(&pool).await;
        let feed = build(&pool, GAME, CAT, &opts(), FAR_FUTURE, 0)
            .await
            .unwrap();
        let mut ids = Vec::new();
        for d in &feed.days {
            for r in parse(&d.bytes)["runs"].as_array().unwrap() {
                assert_eq!(r["id"], r["started_at_ms"]);
                ids.push(r["id"].as_i64().unwrap());
            }
        }
        let distinct: BTreeSet<i64> = ids.iter().copied().collect();
        assert_eq!(distinct.len(), ids.len());

        // A second run at an existing start time makes the key ambiguous;
        // the build refuses rather than publish two runs under one id.
        add_run(&pool, None, 9, ids[0], None, None).await;
        let err = build(&pool, GAME, CAT, &opts(), FAR_FUTURE, 0)
            .await
            .err()
            .expect("duplicate started_at_ms must fail the build");
        assert!(err.to_string().contains("started_at_ms"), "{err}");
    }

    #[tokio::test]
    async fn closed_day_bytes_are_stable_across_builds() {
        let (_dir, pool) = test_pool().await;
        fixture(&pool).await;
        let a = build(&pool, GAME, CAT, &opts(), FAR_FUTURE, 1_000)
            .await
            .unwrap();
        let b = build(&pool, GAME, CAT, &opts(), FAR_FUTURE, 2_000_000)
            .await
            .unwrap();
        assert_eq!(a.days.len(), b.days.len());
        for (x, y) in a.days.iter().zip(&b.days) {
            assert_eq!(x.day, y.day);
            assert_eq!(x.bytes, y.bytes, "{} changed between builds", x.day);
        }
        assert_eq!(a.history, b.history);
        assert_eq!(a.schema, b.schema);
        assert_eq!(a.manifest.days, b.manifest.days);
        assert_eq!(
            a.manifest.files.history.sha256,
            b.manifest.files.history.sha256
        );
        assert_ne!(a.manifest.generated_at_ms, b.manifest.generated_at_ms);

        // Written twice into different directories: identical closed files.
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        write(&a, d1.path()).unwrap();
        write(&b, d2.path()).unwrap();
        let f1 = fs::read(d1.path().join("days/2026-06-15.json")).unwrap();
        let f2 = fs::read(d2.path().join("days/2026-06-15.json")).unwrap();
        assert_eq!(f1, f2);
        assert!(parse(&f1)["closed"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn manifest_hashes_match_the_files_written() {
        let (_dir, pool) = test_pool().await;
        fixture(&pool).await;
        db::log_transition(&pool, 5_000, "RUNNING", "FINISHED", GAME, CAT, "final_ms=1")
            .await
            .unwrap();
        let feed = build(&pool, GAME, CAT, &opts(), FAR_FUTURE, 1_700_000_000_000)
            .await
            .unwrap();
        let out = tempfile::tempdir().unwrap();
        let w = write(&feed, out.path()).unwrap();
        assert_eq!(w.files, feed.days.len() + 3);

        let m = parse(&fs::read(out.path().join("manifest.json")).unwrap());
        assert_eq!(m["schema_version"], 1);
        assert_eq!(m["stale_after_s"], 900);
        assert_eq!(m["generated_at"], "2023-11-14T22:13:20Z");
        assert_eq!(m["last_transition"]["from"], "RUNNING");
        assert_eq!(m["last_transition"]["to"], "FINISHED");
        assert_eq!(m["last_transition"]["at_ms"], 5_000);
        let mut checked = 0;
        let mut total = 0;
        for e in m["days"].as_array().unwrap() {
            let bytes = fs::read(out.path().join(e["path"].as_str().unwrap())).unwrap();
            assert_eq!(bytes.len() as u64, e["bytes"].as_u64().unwrap());
            assert_eq!(sha256_hex(&bytes), e["sha256"].as_str().unwrap());
            assert_eq!(parse(&bytes)["day"], e["day"]);
            assert!(!String::from_utf8_lossy(&bytes).contains("\"label\""));
            checked += 1;
            total += bytes.len();
        }
        assert_eq!(checked, 3);
        for f in ["history", "schema"] {
            let e = &m["files"][f];
            let bytes = fs::read(out.path().join(e["path"].as_str().unwrap())).unwrap();
            assert_eq!(bytes.len() as u64, e["bytes"].as_u64().unwrap());
            assert_eq!(sha256_hex(&bytes), e["sha256"].as_str().unwrap());
            total += bytes.len();
        }
        assert_eq!(
            w.bytes as usize,
            total + fs::read(out.path().join("manifest.json")).unwrap().len()
        );

        // The schema describes the three documents and is a real schema.
        let s = parse(&fs::read(out.path().join("schema.json")).unwrap());
        assert!(s["$schema"].as_str().unwrap().contains("json-schema.org"));
        for doc in ["manifest", "day", "history"] {
            assert!(s["properties"][doc].is_object(), "schema lacks {doc}");
        }
        assert!(s["$defs"]["Run"]["properties"]["ls_attempt"].is_object());
        assert_eq!(
            s["$defs"]["Scope"]["enum"],
            serde_json::json!(["season", "lifetime"])
        );
    }

    #[tokio::test]
    async fn vod_ids_only_where_the_config_publishes_them() {
        let (_dir, pool) = test_pool().await;
        let (s1, _) = fixture(&pool).await;
        db::set_session_vod(&pool, s1, "v123456", noon(2026, 6, 15) - 3_500_000)
            .await
            .unwrap();
        let private = build(&pool, GAME, CAT, &opts(), FAR_FUTURE, 0)
            .await
            .unwrap();
        let text = String::from_utf8(private.days[0].bytes.clone()).unwrap();
        assert!(!text.contains("vod_id"), "{text}");
        assert!(!text.contains("v123456"));
        assert!(!text.contains("somechannel"));

        let public = build(
            &pool,
            GAME,
            CAT,
            &Options {
                public_vod_links: true,
                ..opts()
            },
            FAR_FUTURE,
            0,
        )
        .await
        .unwrap();
        let d = parse(&public.days[0].bytes);
        assert_eq!(d["sessions"][0]["vod_id"], "v123456");
        assert!(d["sessions"][0]["vod_created_at_ms"].is_i64());
        // A session without a known VOD has no key at all, even when public.
        let d17 = parse(&public.days[2].bytes);
        assert!(d17["sessions"][0].get("vod_id").is_none());
    }

    #[tokio::test]
    async fn records_prefer_the_layout_over_the_config() {
        let (_dir, pool) = test_pool().await;
        let from_config = build(&pool, GAME, CAT, &opts(), FAR_FUTURE, 0)
            .await
            .unwrap();
        let r = &from_config.manifest.records;
        assert_eq!(r.len(), 3);
        assert_eq!(
            r[0],
            Record {
                label: "season best".into(),
                ms: 800_000,
                scope: Scope::Season,
                source: Source::Config
            }
        );
        assert_eq!((r[1].label.as_str(), r[1].source), ("WR", Source::Config));
        assert_eq!(r[2].scope, Scope::Lifetime);
        assert!(from_config.manifest.last_transition.is_none());
        assert!(from_config.days.is_empty());

        db::set_setting(&pool, "ls_season_best_ms", "700000")
            .await
            .unwrap();
        db::set_setting(&pool, "ls_pb_ms", "640000").await.unwrap();
        let from_layout = build(&pool, GAME, CAT, &opts(), FAR_FUTURE, 0)
            .await
            .unwrap();
        let r = &from_layout.manifest.records;
        assert_eq!(
            r[0],
            Record {
                label: "season best".into(),
                ms: 700_000,
                scope: Scope::Season,
                source: Source::Layout
            }
        );
        assert_eq!((r[1].label.as_str(), r[1].source), ("WR", Source::Config));
        assert_eq!(
            r[2],
            Record {
                label: "Lifetime PB".into(),
                ms: 640_000,
                scope: Scope::Lifetime,
                source: Source::Layout
            }
        );
        let m = parse(&serde_json::to_vec(&from_layout.manifest).unwrap());
        assert_eq!(m["records"][0]["scope"], "season");
        assert_eq!(m["records"][2]["source"], "layout");
    }

    #[test]
    fn closed_needs_a_past_day_and_no_open_session() {
        let open = Session {
            id: 1,
            started_at_ms: 0,
            ended_at_ms: None,
            source: "hls".into(),
            tag: None,
            attempts: 0,
            finished: 0,
            best_ms: None,
            frames: None,
            parsed: None,
            probing: None,
            relocks: None,
            counter_reads: None,
            events: None,
            vod_id: None,
            vod_created_at_ms: None,
        };
        let ended = Session {
            ended_at_ms: Some(1),
            ..open.clone()
        };
        assert!(is_closed("2026-06-15", "2026-06-16", &[]));
        assert!(is_closed(
            "2026-06-15",
            "2026-06-16",
            std::slice::from_ref(&ended)
        ));
        assert!(!is_closed(
            "2026-06-15",
            "2026-06-16",
            &[ended, open.clone()]
        ));
        assert!(!is_closed("2026-06-16", "2026-06-16", &[]));
        assert!(!is_closed("2026-06-17", "2026-06-16", &[]));
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
