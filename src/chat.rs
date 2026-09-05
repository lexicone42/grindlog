//! Twitch chat integration: announces finished runs and answers commands.
//!
//! Viewer commands (each on its own `chat.command_cooldown_secs` cooldown,
//! 10s by default, so anyone can poke the bot during live testing without
//! mod rights):
//!   !pb  !lastrun (!last)  !today  !attempts  !deaths (!resets)  !pace
//!   !golds (!gold)  !splits  !status (!timer, !ngtimer)
//! Mod commands (broadcaster, badge mods, or logins in chat.mods):
//!   !setgame <game...> <category>   !correct <time>   !void
//! With `chat.command_prefix` set, every command is namespaced under it (see
//! `parse_prefixed`) and the bare form is left to other bots.
//!
//! Replies name a run the way the finish announcement and the site do
//! (`db::run_no`): by the runner's own LiveSplit counter ("run 96677"), or by
//! our tracked ordinal marked as ours ("tracked #2056") when it was not read.
//! "The last run" (`!lastrun`, `!correct`, `!void`) is the run of the tracked
//! game/category that started last, not the last row written: imports land
//! older days after newer ones.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{info, warn};
use twitch_irc::login::StaticLoginCredentials;
use twitch_irc::message::{PrivmsgMessage, ServerMessage};
use twitch_irc::{ClientConfig, SecureTCPTransport, TwitchIRCClient};

use crate::app::Shared;
use crate::config::ChatCfg;
use crate::timeparse::{format_ms, parse_time};
use crate::{db, stats, util};

type IrcClient = TwitchIRCClient<SecureTCPTransport, StaticLoginCredentials>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Pb,
    LastRun,
    Today,
    Attempts,
    Status,
    Deaths,
    Pace,
    Golds,
    Splits,
    SetGame { game: String, category: String },
    Correct { ms: i64 },
    Void,
    Malformed(&'static str),
}

impl Cmd {
    fn key(&self) -> &'static str {
        match self {
            Cmd::Pb => "pb",
            Cmd::LastRun => "lastrun",
            Cmd::Today => "today",
            Cmd::Attempts => "attempts",
            Cmd::Status => "status",
            Cmd::Deaths => "deaths",
            Cmd::Pace => "pace",
            Cmd::Golds => "golds",
            Cmd::Splits => "splits",
            Cmd::SetGame { .. } => "setgame",
            Cmd::Correct { .. } => "correct",
            Cmd::Void => "void",
            Cmd::Malformed(_) => "malformed",
        }
    }

    fn mod_only(&self) -> bool {
        matches!(self, Cmd::SetGame { .. } | Cmd::Correct { .. } | Cmd::Void)
    }
}

/// Parse a command under a namespace prefix: with prefix "ng", `!ngpb` is
/// `!pb` and the bare `!pb` is ignored (another bot's business).
pub fn parse_prefixed(text: &str, prefix: &str) -> Option<Cmd> {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        return parse_command(text);
    }
    let mut it = text.split_whitespace();
    let head = it.next()?;
    let body = head.strip_prefix('!')?;
    if body.len() <= prefix.len() || !body[..prefix.len()].eq_ignore_ascii_case(prefix) {
        return None;
    }
    let rest: String = std::iter::once(format!("!{}", &body[prefix.len()..]))
        .chain(it.map(str::to_string))
        .collect::<Vec<_>>()
        .join(" ");
    parse_command(&rest)
}

pub fn parse_command(text: &str) -> Option<Cmd> {
    let mut it = text.split_whitespace();
    let head = it.next()?;
    if !head.starts_with('!') {
        return None;
    }
    match head.to_ascii_lowercase().as_str() {
        "!pb" => Some(Cmd::Pb),
        "!lastrun" | "!last" => Some(Cmd::LastRun),
        "!today" => Some(Cmd::Today),
        "!attempts" => Some(Cmd::Attempts),
        "!status" | "!timer" | "!ngtimer" => Some(Cmd::Status),
        "!deaths" | "!resets" => Some(Cmd::Deaths),
        "!pace" => Some(Cmd::Pace),
        "!golds" | "!gold" => Some(Cmd::Golds),
        "!splits" => Some(Cmd::Splits),
        "!setgame" => {
            let rest: Vec<&str> = it.collect();
            if rest.len() < 2 {
                return Some(Cmd::Malformed("usage: !setgame <game...> <category>"));
            }
            // Last word is the category, everything before it the game name.
            let category = rest[rest.len() - 1].to_string();
            let game = rest[..rest.len() - 1].join(" ");
            Some(Cmd::SetGame { game, category })
        }
        "!correct" => match it.next().and_then(parse_time) {
            Some(ms) => Some(Cmd::Correct { ms }),
            None => Some(Cmd::Malformed(
                "usage: !correct <time>, e.g. !correct 12:34.5",
            )),
        },
        "!void" => Some(Cmd::Void),
        _ => None,
    }
}

pub struct ChatCtx {
    pub cfg: ChatCfg,
    pub channel: String,
    pub pool: sqlx::SqlitePool,
    pub shared: Arc<Shared>,
}

pub async fn run_chat(
    ctx: ChatCtx,
    mut announce_rx: mpsc::UnboundedReceiver<String>,
) -> Result<()> {
    let token = ctx
        .cfg
        .oauth_token
        .trim()
        .trim_start_matches("oauth:")
        .to_string();
    let creds = StaticLoginCredentials::new(ctx.cfg.username.clone(), Some(token));
    let (mut incoming, client) = IrcClient::new(ClientConfig::new_simple(creds));
    client
        .join(ctx.channel.clone())
        .context("joining channel")?;
    info!("chat: joined #{} as {}", ctx.channel, ctx.cfg.username);

    let mut cooldowns: HashMap<&'static str, Instant> = HashMap::new();
    let cooldown = Duration::from_secs(ctx.cfg.command_cooldown_secs);
    let mut announce_open = true;
    loop {
        tokio::select! {
            maybe = incoming.recv() => {
                let Some(msg) = maybe else { break };
                if let ServerMessage::Privmsg(m) = msg {
                    if let Err(e) = handle_privmsg(&ctx, &client, &m, &mut cooldowns, cooldown).await {
                        warn!("chat command failed: {e:#}");
                    }
                }
            }
            maybe = announce_rx.recv(), if announce_open => {
                match maybe {
                    Some(text) => {
                        if let Err(e) = client.say(ctx.channel.clone(), text).await {
                            warn!("chat announce failed: {e}");
                        }
                    }
                    None => announce_open = false,
                }
            }
        }
    }
    Ok(())
}

async fn handle_privmsg(
    ctx: &ChatCtx,
    client: &IrcClient,
    m: &PrivmsgMessage,
    cooldowns: &mut HashMap<&'static str, Instant>,
    cooldown: Duration,
) -> Result<()> {
    let Some(cmd) = parse_prefixed(&m.message_text, &ctx.cfg.command_prefix) else {
        return Ok(());
    };
    let is_mod = m
        .badges
        .iter()
        .any(|b| b.name == "moderator" || b.name == "broadcaster")
        || ctx.cfg.mods.contains(&m.sender.login.to_ascii_lowercase());

    if cmd.mod_only() && !is_mod {
        return Ok(()); // silently ignore, keeps chat clean
    }
    if !cmd.mod_only() {
        let now = Instant::now();
        if let Some(&last) = cooldowns.get(cmd.key()) {
            if now.duration_since(last) < cooldown {
                return Ok(());
            }
        }
        cooldowns.insert(cmd.key(), now);
    }

    if let Some(reply) = build_reply(ctx, &cmd).await? {
        client.say(ctx.channel.clone(), reply).await?;
    }
    Ok(())
}

async fn build_reply(ctx: &ChatCtx, cmd: &Cmd) -> Result<Option<String>> {
    let (game, category) = ctx.shared.game.read().await.clone();
    let reply = match cmd {
        Cmd::Malformed(usage) => Some((*usage).to_string()),
        Cmd::Pb => {
            let tracked = db::personal_best(&ctx.pool, &game, &category).await?;
            let label = &ctx.shared.record_label;
            let baseline = *ctx.shared.baseline_best_ms.read().await;
            match (tracked, baseline) {
                (Some(pb), Some(base))
                    if base < pb.final_time_ms.unwrap_or(i64::MAX) =>
                {
                    Some(format!(
                        "{label} for {game} [{category}]: {} (pre-tracking) — best tracked run: {} ({})",
                        format_ms(base),
                        format_ms(pb.final_time_ms.unwrap_or(0)),
                        pb.run_no(),
                    ))
                }
                (Some(pb), _) => Some(format!(
                    "{label} for {game} [{category}]: {} ({}, {})",
                    format_ms(pb.final_time_ms.unwrap_or(0)),
                    pb.run_no(),
                    util::date_of_ms(pb.ended_at_ms),
                )),
                (None, Some(base)) => Some(format!(
                    "{label} for {game} [{category}]: {} (pre-tracking); no tracked finishes yet.",
                    format_ms(base),
                )),
                (None, None) => {
                    Some(format!("No finished runs recorded yet for {game} [{category}]."))
                }
            }
        }
        Cmd::LastRun => match db::last_run(&ctx.pool, &game, &category).await? {
            Some(r) if r.outcome == db::OUTCOME_FINISHED => Some(format!(
                "Last run: {} ({} [{}], {})",
                format_ms(r.final_time_ms.unwrap_or(0)),
                r.game,
                r.category,
                r.run_no(),
            )),
            Some(r) => Some(format!(
                "Last run: reset at {} ({} [{}], {})",
                r.last_timer_ms.map(format_ms).unwrap_or_else(|| "?".into()),
                r.game,
                r.category,
                r.run_no(),
            )),
            None => Some("No runs recorded yet.".to_string()),
        },
        Cmd::Today => {
            let stats =
                db::today_stats(&ctx.pool, &game, &category, util::local_day_start_ms()).await?;
            Some(format!(
                "Today for {game} [{category}]: {} attempts, {} finished, {} resets{}",
                stats.attempts,
                stats.finished,
                stats.resets,
                match stats.best_ms {
                    Some(b) => format!(", best {}", format_ms(b)),
                    None => String::new(),
                },
            ))
        }
        Cmd::Attempts => {
            let n = db::total_attempts(&ctx.pool, &game, &category).await?;
            Some(format!("{game} [{category}]: {n} attempts logged."))
        }
        Cmd::Status => {
            let st = ctx.shared.status.read().await.clone();
            let age_s = (util::unix_ms() - st.updated_unix_ms).max(0) / 1000;
            let timer = match st.smoothed_ms {
                Some(ms) => match st.read_age_ms {
                    // A projection more than ~10s past the last clean read is
                    // a guess (death screen, menu, ads) — say so.
                    Some(age) if age > 10_000 => format!(
                        "timer ~{} (projected — last clean read {}s ago)",
                        format_ms(ms),
                        age / 1000
                    ),
                    _ => format!("timer ~{}", format_ms(ms)),
                },
                None => "no timer".to_string(),
            };
            let ocr = match &st.last_ocr {
                Some(s) => format!("last OCR {s:?}"),
                None => "OCR illegible".to_string(),
            };
            let health = match st.parse_pct {
                Some(p) => format!(" | reading {p:.0}% of frames, layout {}", st.layout),
                None => String::new(),
            };
            Some(format!(
                "Tracker: {} | {timer} | {ocr}{health} | updated {age_s}s ago",
                st.phase,
            ))
        }
        Cmd::Deaths => {
            let brief = db::runs_brief(&ctx.pool, &game, &category).await?;
            let chart = stats::death_chart(&brief, &ctx.shared.acts);
            if chart.is_empty() {
                Some("No resets recorded yet.".to_string())
            } else {
                let s = stats::streaks(&brief);
                let parts: Vec<String> = chart
                    .iter()
                    .filter(|b| b.deaths > 0)
                    .map(|b| format!("{} {} ({:.0}%)", b.label, b.deaths, b.pct))
                    .collect();
                Some(format!(
                    "Deaths by act: {} — {} attempts, {} finished",
                    parts.join(", "),
                    s.attempts,
                    s.finished,
                ))
            }
        }
        Cmd::Pace => {
            let cur = ctx.shared.current_splits.read().await.clone();
            let status = ctx.shared.status.read().await.clone();
            if status.phase != "RUNNING" {
                Some("No run in progress.".to_string())
            } else {
                let timer = match (status.smoothed_ms, status.read_age_ms) {
                    (Some(ms), Some(age)) if age > 10_000 => {
                        format!("{} (projected)", format_ms(ms))
                    }
                    (Some(ms), _) => format_ms(ms),
                    _ => "?".into(),
                };
                match cur.last() {
                    None => Some(format!("Timer ~{timer} — no splits yet this run.")),
                    Some(last) => {
                        // Against the best tracked run, named by what the
                        // config calls the record ("PB", "season best").
                        let vs_record = match db::personal_best(&ctx.pool, &game, &category).await?
                        {
                            Some(pb) => db::run_splits(&ctx.pool, pb.id)
                                .await?
                                .iter()
                                .find(|s| s.act_index == last.act_index as i64)
                                .map(|s| {
                                    let d = last.cumulative_ms - s.cumulative_ms;
                                    let label = &ctx.shared.record_label;
                                    if d <= 0 {
                                        format!(" — {} ahead of {label} pace", format_ms(-d))
                                    } else {
                                        format!(" — {} behind {label} pace", format_ms(d))
                                    }
                                })
                                .unwrap_or_default(),
                            None => String::new(),
                        };
                        Some(format!(
                            "{} done at {}{vs_record} (timer ~{timer})",
                            last.act_name,
                            format_ms(last.cumulative_ms),
                        ))
                    }
                }
            }
        }
        Cmd::Golds => {
            let golds = db::golds(&ctx.pool, &game, &category).await?;
            if golds.is_empty() {
                Some("No split data recorded yet.".to_string())
            } else {
                let parts: Vec<String> = golds
                    .iter()
                    .map(|g| format!("{} {}", g.act_name, format_ms(g.gold_ms)))
                    .collect();
                let sob = if golds.len() == ctx.shared.acts.len() {
                    let sum: i64 = golds.iter().map(|g| g.gold_ms).sum();
                    format!(" — sum of best {}", format_ms(sum))
                } else {
                    String::new()
                };
                Some(format!("Golds: {}{sob}", parts.join(", ")))
            }
        }
        Cmd::Splits => {
            let cur = ctx.shared.current_splits.read().await.clone();
            if cur.is_empty() {
                Some("No splits yet this run.".to_string())
            } else {
                let parts: Vec<String> = cur
                    .iter()
                    .map(|s| format!("{} {}", s.act_name, format_ms(s.cumulative_ms)))
                    .collect();
                Some(format!("This run: {}", parts.join(", ")))
            }
        }
        Cmd::SetGame {
            game: g,
            category: c,
        } => {
            db::set_setting(&ctx.pool, "game", g).await?;
            db::set_setting(&ctx.pool, "category", c).await?;
            *ctx.shared.game.write().await = (g.clone(), c.clone());
            Some(format!("Now tracking {g} [{c}]."))
        }
        Cmd::Correct { ms } => {
            match db::correct_last_run(&ctx.pool, &game, &category, *ms).await? {
                Some(r) => Some(format!("Corrected {} to {}.", r.run_no(), format_ms(*ms))),
                None => Some("No run to correct.".to_string()),
            }
        }
        Cmd::Void => match db::void_last_run(&ctx.pool, &game, &category).await? {
            Some(r) => Some(format!(
                "Voided {} ({}).",
                r.run_no(),
                match r.final_time_ms {
                    Some(ms) => format_ms(ms),
                    None => r.outcome.clone(),
                }
            )),
            None => Some("No run to void.".to_string()),
        },
    };
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_viewer_commands() {
        assert_eq!(parse_command("!pb"), Some(Cmd::Pb));
        assert_eq!(parse_command("!PB"), Some(Cmd::Pb));
        assert_eq!(parse_command("!lastrun"), Some(Cmd::LastRun));
        assert_eq!(parse_command("!last"), Some(Cmd::LastRun));
        assert_eq!(parse_command("!today"), Some(Cmd::Today));
        assert_eq!(parse_command("!attempts"), Some(Cmd::Attempts));
        assert_eq!(parse_command("!status"), Some(Cmd::Status));
        assert_eq!(parse_command("!timer"), Some(Cmd::Status));
        assert_eq!(parse_command("!ngtimer"), Some(Cmd::Status));
        assert_eq!(parse_command("!deaths"), Some(Cmd::Deaths));
        assert_eq!(parse_command("!resets"), Some(Cmd::Deaths));
        assert!(!Cmd::Deaths.mod_only());
        assert_eq!(parse_command("!pace"), Some(Cmd::Pace));
        assert_eq!(parse_command("!golds"), Some(Cmd::Golds));
        assert_eq!(parse_command("!splits"), Some(Cmd::Splits));
        assert!(!Cmd::Pace.mod_only() && !Cmd::Golds.mod_only() && !Cmd::Splits.mod_only());
    }

    #[test]
    fn parses_setgame_with_multiword_name() {
        assert_eq!(
            parse_command("!setgame Super Mario Bros. Any%"),
            Some(Cmd::SetGame {
                game: "Super Mario Bros.".into(),
                category: "Any%".into()
            })
        );
        assert!(matches!(
            parse_command("!setgame OnlyOneArg"),
            Some(Cmd::Malformed(_))
        ));
    }

    #[test]
    fn parses_correct_times() {
        assert_eq!(
            parse_command("!correct 12:34.5"),
            Some(Cmd::Correct {
                ms: (12 * 60 + 34) * 1000 + 500
            })
        );
        assert!(matches!(
            parse_command("!correct nonsense"),
            Some(Cmd::Malformed(_))
        ));
        assert!(matches!(parse_command("!correct"), Some(Cmd::Malformed(_))));
    }

    #[test]
    fn ignores_non_commands() {
        assert_eq!(parse_command("hello there"), None);
        assert_eq!(parse_command("!unknowncmd"), None);
        assert_eq!(parse_command(""), None);
    }

    #[test]
    fn prefix_namespaces_commands() {
        assert_eq!(parse_prefixed("!ngpb", "ng"), Some(Cmd::Pb));
        assert_eq!(parse_prefixed("!NgDeaths", "ng"), Some(Cmd::Deaths));
        assert_eq!(
            parse_prefixed("!ngsetgame Ninja Gaiden Any%", "ng"),
            Some(Cmd::SetGame {
                game: "Ninja Gaiden".into(),
                category: "Any%".into()
            })
        );
        // bare commands belong to other bots when a prefix is set
        assert_eq!(parse_prefixed("!pb", "ng"), None);
        assert_eq!(parse_prefixed("!ng", "ng"), None);
        // longer decorative prefixes work too
        assert_eq!(parse_prefixed("!ngrust-pb", "ngrust-"), Some(Cmd::Pb));
        // empty prefix = classic behavior
        assert_eq!(parse_prefixed("!pb", ""), Some(Cmd::Pb));
    }

    #[test]
    fn mod_gating() {
        assert!(Cmd::Void.mod_only());
        assert!(Cmd::Correct { ms: 0 }.mod_only());
        assert!(Cmd::SetGame {
            game: "x".into(),
            category: "y".into()
        }
        .mod_only());
        assert!(!Cmd::Pb.mod_only());
        assert!(!Cmd::Status.mod_only());
    }

    // ---- replies against a real (temporary) database ----------------------

    use crate::app::Status;
    use crate::splits::RecordedSplit;
    use tokio::sync::RwLock;

    const GAME: &str = "Ninja Gaiden";
    const CAT: &str = "Any%";

    /// A chat context over an empty temp database, tracking GAME [CAT] and
    /// calling the record `record_label`, as live.toml does ("season best").
    async fn test_ctx(record_label: &str) -> (tempfile::TempDir, ChatCtx) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chat.db");
        let pool = db::open(path.to_str().unwrap()).await.unwrap();
        let shared = Arc::new(Shared {
            game: RwLock::new((GAME.into(), CAT.into())),
            status: RwLock::new(Status {
                phase: "IDLE".into(),
                ..Default::default()
            }),
            acts: vec![("Act 1".into(), Some(120_000)), ("Act 2".into(), None)],
            current_splits: RwLock::new(Vec::new()),
            record_label: record_label.into(),
            baseline_best_ms: RwLock::new(None),
        });
        let cfg = ChatCfg {
            enabled: true,
            channel: "testchannel".into(),
            username: "bot".into(),
            oauth_token: String::new(),
            announce: false,
            command_cooldown_secs: 10,
            mods: Vec::new(),
            command_prefix: String::new(),
        };
        let ctx = ChatCtx {
            cfg,
            channel: "testchannel".into(),
            pool,
            shared,
        };
        (dir, ctx)
    }

    /// Insert one run of GAME [CAT]: finished at `final_ms`, or a reset
    /// that died at 0:42.0 when `None`.
    async fn add_run(
        ctx: &ChatCtx,
        attempt: i64,
        ls_attempt: Option<i64>,
        started_at_ms: i64,
        final_ms: Option<i64>,
    ) -> i64 {
        db::insert_run(
            &ctx.pool,
            db::NewRun {
                game: GAME,
                category: CAT,
                attempt_number: attempt,
                started_at_ms,
                ended_at_ms: started_at_ms + 800_000,
                outcome: if final_ms.is_some() {
                    db::OUTCOME_FINISHED
                } else {
                    db::OUTCOME_RESET
                },
                reset_reason: if final_ms.is_some() {
                    None
                } else {
                    Some("zeroed")
                },
                final_time_ms: final_ms,
                last_timer_ms: final_ms.or(Some(42_000)),
                session_id: None,
                ls_attempt,
            },
        )
        .await
        .unwrap()
    }

    async fn reply(ctx: &ChatCtx, cmd: Cmd) -> String {
        build_reply(ctx, &cmd).await.unwrap().expect("a reply")
    }

    #[tokio::test]
    async fn lastrun_names_the_run_by_the_runners_counter() {
        let (_dir, ctx) = test_ctx("season best").await;
        assert_eq!(reply(&ctx, Cmd::LastRun).await, "No runs recorded yet.");

        // His LiveSplit counter was read: that is the run's name.
        add_run(&ctx, 2056, Some(96_677), 1_000_000, Some(754_500)).await;
        assert_eq!(
            reply(&ctx, Cmd::LastRun).await,
            "Last run: 12:34.5 (Ninja Gaiden [Any%], run 96677)"
        );

        // Not read: our ordinal, marked as ours, never "attempt #".
        add_run(&ctx, 2057, None, 2_000_000, None).await;
        assert_eq!(
            reply(&ctx, Cmd::LastRun).await,
            "Last run: reset at 0:42.0 (Ninja Gaiden [Any%], tracked #2057)"
        );
    }

    #[tokio::test]
    async fn lastrun_is_the_latest_started_not_the_last_inserted() {
        let (_dir, ctx) = test_ctx("PB").await;
        // The live bot logs attempt 2057, then an import lands a run from
        // days ago with a higher row id, and another game has a newer row.
        add_run(&ctx, 2057, Some(96_678), 5_000_000, Some(700_000)).await;
        add_run(&ctx, 1500, Some(95_000), 1_000_000, None).await;
        db::insert_run(
            &ctx.pool,
            db::NewRun {
                game: "Other Game",
                category: CAT,
                attempt_number: 1,
                started_at_ms: 9_000_000,
                ended_at_ms: 9_100_000,
                outcome: db::OUTCOME_RESET,
                reset_reason: Some("zeroed"),
                final_time_ms: None,
                last_timer_ms: Some(1000),
                session_id: None,
                ls_attempt: Some(1),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            reply(&ctx, Cmd::LastRun).await,
            "Last run: 11:40.0 (Ninja Gaiden [Any%], run 96678)"
        );
    }

    #[tokio::test]
    async fn pb_reply_names_the_record_run() {
        let (_dir, ctx) = test_ctx("season best").await;
        assert_eq!(
            reply(&ctx, Cmd::Pb).await,
            "No finished runs recorded yet for Ninja Gaiden [Any%]."
        );
        add_run(&ctx, 2000, Some(96_000), 1_000_000, Some(695_100)).await;
        add_run(&ctx, 2001, Some(96_001), 2_000_000, Some(720_000)).await;
        let r = reply(&ctx, Cmd::Pb).await;
        assert!(
            r.starts_with("season best for Ninja Gaiden [Any%]: 11:35.1 (run 96000, "),
            "{r}"
        );
        assert!(!r.contains("attempt #"), "{r}");

        // A faster pre-tracking baseline is the record; the tracked best is
        // still named by his number.
        *ctx.shared.baseline_best_ms.write().await = Some(690_000);
        assert_eq!(
            reply(&ctx, Cmd::Pb).await,
            "season best for Ninja Gaiden [Any%]: 11:30.0 (pre-tracking) — best tracked run: 11:35.1 (run 96000)"
        );

        // Without his number the fallback is marked as ours.
        db::void_last_run(&ctx.pool, GAME, CAT).await.unwrap();
        db::void_last_run(&ctx.pool, GAME, CAT).await.unwrap();
        add_run(&ctx, 2002, None, 3_000_000, Some(695_100)).await;
        assert_eq!(
            reply(&ctx, Cmd::Pb).await,
            "season best for Ninja Gaiden [Any%]: 11:30.0 (pre-tracking) — best tracked run: 11:35.1 (tracked #2002)"
        );
    }

    #[tokio::test]
    async fn correct_and_void_name_the_run_and_take_the_latest_started() {
        let (_dir, ctx) = test_ctx("PB").await;
        assert_eq!(
            reply(&ctx, Cmd::Correct { ms: 1 }).await,
            "No run to correct."
        );
        assert_eq!(reply(&ctx, Cmd::Void).await, "No run to void.");

        // The run just watched (reset), then an imported older run with a
        // higher id: !correct and !void must pick the one just watched.
        add_run(&ctx, 2056, Some(96_677), 5_000_000, None).await;
        let imported = add_run(&ctx, 1500, Some(95_000), 1_000_000, None).await;
        assert_eq!(
            reply(&ctx, Cmd::Correct { ms: 720_000 }).await,
            "Corrected run 96677 to 12:00.0."
        );
        let fixed = db::last_run(&ctx.pool, GAME, CAT).await.unwrap().unwrap();
        assert_eq!(fixed.ls_attempt, Some(96_677));
        assert_eq!(fixed.outcome, db::OUTCOME_FINISHED);
        assert_eq!(
            reply(&ctx, Cmd::LastRun).await,
            "Last run: 12:00.0 (Ninja Gaiden [Any%], run 96677)"
        );
        assert_eq!(reply(&ctx, Cmd::Void).await, "Voided run 96677 (12:00.0).");
        let left = db::last_run(&ctx.pool, GAME, CAT).await.unwrap().unwrap();
        assert_eq!(left.id, imported);

        // A run whose number was not read voids under the tracked ordinal
        // with its outcome, as before.
        add_run(&ctx, 2057, None, 6_000_000, None).await;
        assert_eq!(
            reply(&ctx, Cmd::Void).await,
            "Voided tracked #2057 (reset)."
        );
    }

    #[tokio::test]
    async fn pace_is_labelled_with_the_record_label() {
        let (_dir, ctx) = test_ctx("season best").await;
        assert_eq!(reply(&ctx, Cmd::Pace).await, "No run in progress.");

        // The best tracked run reached Act 1 at 2:00.0.
        let pb_id = add_run(&ctx, 2000, Some(96_000), 1_000_000, Some(695_100)).await;
        db::insert_splits(
            &ctx.pool,
            pb_id,
            &[RecordedSplit {
                act_index: 0,
                act_name: "Act 1".into(),
                cumulative_ms: 120_000,
            }],
        )
        .await
        .unwrap();
        // The run in progress did it in 1:58.0.
        *ctx.shared.status.write().await = Status {
            phase: "RUNNING".into(),
            smoothed_ms: Some(130_000),
            read_age_ms: Some(500),
            updated_unix_ms: util::unix_ms(),
            ..Default::default()
        };
        ctx.shared.current_splits.write().await.push(RecordedSplit {
            act_index: 0,
            act_name: "Act 1".into(),
            cumulative_ms: 118_000,
        });
        assert_eq!(
            reply(&ctx, Cmd::Pace).await,
            "Act 1 done at 1:58.0 — 0:02.0 ahead of season best pace (timer ~2:10.0)"
        );
    }
}
