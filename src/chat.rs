//! Twitch chat integration: announces finished runs and answers commands.
//!
//! Viewer commands (10s shared cooldown each, so anyone can poke the bot
//! during live testing without mod rights):
//!   !pb !lastrun !today !attempts !status (aliases !timer, !ngtimer)
//! Mod commands (broadcaster, badge mods, or logins in chat.mods):
//!   !setgame <game...> <category>   !correct <time>   !void

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
            None => Some(Cmd::Malformed("usage: !correct <time>, e.g. !correct 12:34.5")),
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

pub async fn run_chat(ctx: ChatCtx, mut announce_rx: mpsc::UnboundedReceiver<String>) -> Result<()> {
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
            let baseline = ctx.shared.baseline_best_ms;
            match (tracked, baseline) {
                (Some(pb), Some(base))
                    if base < pb.final_time_ms.unwrap_or(i64::MAX) =>
                {
                    Some(format!(
                        "{label} for {game} [{category}]: {} (pre-tracking) — best tracked run: {} (attempt #{})",
                        format_ms(base),
                        format_ms(pb.final_time_ms.unwrap_or(0)),
                        pb.attempt_number,
                    ))
                }
                (Some(pb), _) => Some(format!(
                    "{label} for {game} [{category}]: {} (attempt #{}, {})",
                    format_ms(pb.final_time_ms.unwrap_or(0)),
                    pb.attempt_number,
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
        Cmd::LastRun => match db::last_run(&ctx.pool).await? {
            Some(r) if r.outcome == db::OUTCOME_FINISHED => Some(format!(
                "Last run: {} ({} [{}], attempt #{})",
                format_ms(r.final_time_ms.unwrap_or(0)),
                r.game,
                r.category,
                r.attempt_number,
            )),
            Some(r) => Some(format!(
                "Last run: reset at {} ({} [{}], attempt #{})",
                r.last_timer_ms.map(format_ms).unwrap_or_else(|| "?".into()),
                r.game,
                r.category,
                r.attempt_number,
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
            Some(format!(
                "Tracker: {} | {timer} | {ocr} | updated {age_s}s ago",
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
                        let vs_pb = match db::personal_best(&ctx.pool, &game, &category).await? {
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
                            "{} done at {}{vs_pb} (timer ~{timer})",
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
        Cmd::SetGame { game: g, category: c } => {
            db::set_setting(&ctx.pool, "game", g).await?;
            db::set_setting(&ctx.pool, "category", c).await?;
            *ctx.shared.game.write().await = (g.clone(), c.clone());
            Some(format!("Now tracking {g} [{c}]."))
        }
        Cmd::Correct { ms } => match db::correct_last_run(&ctx.pool, *ms).await? {
            Some(r) => Some(format!(
                "Corrected run #{} to {}.",
                r.attempt_number,
                format_ms(*ms)
            )),
            None => Some("No run to correct.".to_string()),
        },
        Cmd::Void => match db::void_last_run(&ctx.pool).await? {
            Some(r) => Some(format!(
                "Voided run #{} ({}).",
                r.attempt_number,
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
            Some(Cmd::Correct { ms: (12 * 60 + 34) * 1000 + 500 })
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
            Some(Cmd::SetGame { game: "Ninja Gaiden".into(), category: "Any%".into() })
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
        assert!(Cmd::SetGame { game: "x".into(), category: "y".into() }.mod_only());
        assert!(!Cmd::Pb.mod_only());
        assert!(!Cmd::Status.mod_only());
    }
}
