//! Offline review of collected data: PBs, today's stats, recent runs.
//! `--json` emits a machine-readable document (handy for publishing the
//! records to a website later).

use anyhow::Result;

use crate::config::Config;
use crate::timeparse::format_ms;
use crate::{app, db, stats, util};

pub async fn run(cfg: Config, json: bool) -> Result<()> {
    let pool = db::open(&cfg.database.path).await?;
    let (game, category) = app::load_game(&pool, &cfg).await?;
    let summaries = db::summaries(&pool).await?;
    let today = db::today_stats(&pool, &game, &category, util::local_day_start_ms()).await?;
    // Every run and every split, for the site's per-day log.
    let all_runs = db::runs_since(&pool, &game, &category, 0).await?;
    let mut splits_by_run = serde_json::Map::new();
    for (run_id, split) in db::splits_since(&pool, &game, &category, 0).await? {
        splits_by_run
            .entry(run_id.to_string())
            .or_insert_with(|| serde_json::Value::Array(Vec::new()))
            .as_array_mut()
            .unwrap()
            .push(serde_json::to_value(&split)?);
    }
    let daily = db::daily_stats(&pool, &game, &category).await?;
    // Every session: the site needs tags and capture health per day.
    let sessions = db::recent_sessions(&pool, 100_000).await?;
    let recent = db::recent_runs(&pool, 15).await?;
    let brief = db::runs_brief(&pool, &game, &category).await?;
    let acts = cfg.game.act_list();
    let deaths = stats::death_chart(&brief, &acts);
    let survival = stats::survival(&brief, &acts);
    let pbs = stats::pb_history(&brief);
    let streaks = stats::streaks(&brief);
    let golds = db::golds(&pool, &game, &category).await?;

    if json {
        // Every finished run, oldest first — the site's finish-times chart.
        let finishes: Vec<serde_json::Value> = brief
            .iter()
            .filter(|r| r.finished)
            .map(|r| {
                serde_json::json!({
                    "attempt_number": r.attempt_number,
                    "ls_attempt": r.ls_attempt,
                    "started_at_ms": r.started_at_ms,
                    "final_time_ms": r.final_time_ms,
                })
            })
            .collect();
        let references: Vec<serde_json::Value> = cfg
            .game
            .references
            .iter()
            .filter_map(|r| {
                r.ms()
                    .map(|ms| serde_json::json!({"label": r.label, "ms": ms}))
            })
            .collect();
        let doc = serde_json::json!({
            "generated_at_ms": util::unix_ms(),
            "current_game": game,
            "current_category": category,
            "record_label": cfg.game.record_label,
            "references": references,
            "baseline_best_ms": cfg.game.baseline_best_ms(),
            "ls_sob_ms": db::get_setting(&pool, "ls_sob_ms")
                .await?
                .and_then(|s| s.parse::<i64>().ok()),
            "summaries": summaries,
            "today": today,
            "runs": all_runs,
            "splits_by_run": splits_by_run,
            "daily": daily,
            "sessions": sessions,
            "death_chart": deaths,
            "survival": survival,
            "acts": cfg.game.acts,
            "golds": golds,
            "pb_history": pbs,
            "streaks": streaks,
            "finishes": finishes,
            "recent_runs": recent,
        });
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(());
    }

    println!("Currently tracking: {game} [{category}]\n");

    if summaries.is_empty() {
        println!("No runs recorded yet.");
        return Ok(());
    }

    println!("Personal bests:");
    for s in &summaries {
        println!(
            "  {} [{}] — best {}, {}/{} finished",
            s.game,
            s.category,
            s.best_ms.map(format_ms).unwrap_or_else(|| "—".into()),
            s.finished,
            s.attempts,
        );
    }

    println!("\nBy day ({game} [{category}]):");
    for d in &daily {
        println!(
            "  {}  {:>3} attempts, {:>2} finished, {:>3} resets{}",
            d.day,
            d.attempts,
            d.finished,
            d.resets,
            match d.best_ms {
                Some(b) => format!(", best {}", format_ms(b)),
                None => String::new(),
            }
        );
    }

    if !deaths.is_empty() {
        println!(
            "\nWhere runs die ({} resets):",
            streaks.attempts - streaks.finished
        );
        let max = deaths.iter().map(|d| d.deaths).max().unwrap_or(1).max(1);
        for d in &deaths {
            let bar = "#".repeat(((d.deaths * 30) / max) as usize);
            println!("  {:<8} {:>4}  {:>5.1}%  {bar}", d.label, d.deaths, d.pct);
        }
    }
    if !survival.is_empty() {
        let parts: Vec<String> = survival
            .iter()
            .map(|s| format!("{} {:.0}%", s.label, s.pct))
            .collect();
        println!("\nSurvival past each act: {}", parts.join(" | "));
    }
    if !golds.is_empty() {
        println!("\nGold segments:");
        for g in &golds {
            println!(
                "  {:<8} {}  ({} samples)",
                g.act_name,
                format_ms(g.gold_ms),
                g.samples
            );
        }
        if golds.len() == cfg.game.acts.len() && !cfg.game.acts.is_empty() {
            let sum: i64 = golds.iter().map(|g| g.gold_ms).sum();
            println!("  Sum of best: {}", format_ms(sum));
        }
    }
    if !pbs.is_empty() {
        println!("\nPB progression:");
        for p in &pbs {
            println!(
                "  {}  {}  (attempt #{})",
                util::date_of_ms(p.at_ms),
                format_ms(p.time_ms),
                p.attempt_number
            );
        }
    }
    println!(
        "\nGrind: {} attempts for {} finishes{}; longest reset streak {}, current {}",
        streaks.attempts,
        streaks.finished,
        match streaks.attempts_per_finish {
            Some(a) => format!(" ({a:.1} attempts/finish)"),
            None => String::new(),
        },
        streaks.longest_reset_streak,
        streaks.current_reset_streak,
    );

    if !sessions.is_empty() {
        println!("\nSessions:");
        for s in &sessions {
            let dur = match s.ended_at_ms {
                Some(end) => {
                    let mins = (end - s.started_at_ms).max(0) / 60_000;
                    format!("{}h{:02}m", mins / 60, mins % 60)
                }
                None => "live".to_string(),
            };
            println!(
                "  #{:<3} {}  {:>6}  {:>3} attempts, {:>2} finished{}  ({})",
                s.id,
                util::datetime_of_ms(s.started_at_ms),
                dur,
                s.attempts,
                s.finished,
                match s.best_ms {
                    Some(b) => format!(", best {}", format_ms(b)),
                    None => String::new(),
                },
                s.source,
            );
        }
    }

    println!(
        "\nToday ({game} [{category}]): {} attempts, {} finished, {} resets{}",
        today.attempts,
        today.finished,
        today.resets,
        match today.best_ms {
            Some(b) => format!(", best {}", format_ms(b)),
            None => String::new(),
        }
    );

    println!("\nRecent runs:");
    for r in &recent {
        let outcome = match r.final_time_ms {
            Some(ms) => format_ms(ms),
            None => format!(
                "reset @ {} ({})",
                r.last_timer_ms.map(format_ms).unwrap_or_else(|| "?".into()),
                r.reset_reason.as_deref().unwrap_or("?")
            ),
        };
        println!(
            "  #{:<4} {}  {} [{}]  {}",
            r.attempt_number,
            util::datetime_of_ms(r.started_at_ms),
            r.game,
            r.category,
            outcome,
        );
    }
    Ok(())
}
