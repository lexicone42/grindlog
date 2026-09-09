//! Offline review of collected data: PBs, today's stats, recent runs.
//! `--json` emits the document the records site is built from
//! (scripts/build-site.sh): every run and split, per-day stats, sessions
//! with capture health, finishes, golds, and the reference times. Values
//! read off the layout win over configured ones: its WR and lifetime PB
//! replace the references of the same label, and its season best replaces
//! `baseline_best_ms`. `--api-dir` writes the per-day feed instead (api.rs):
//! one file per broadcast day behind a manifest, for readers that want to
//! fetch only what changed.

use std::path::Path;

use anyhow::Result;

use crate::config::Config;
use crate::timeparse::format_ms;
use crate::{api, app, db, roster, stats, util};

/// Group the runs of every other game into the broadcasts they belong to,
/// and name each broadcast from the games it holds.
///
/// A marathon is ten games in one session, so the session IS the event.
/// Which event it was comes from the ten names rather than from the tag:
/// the tag is what the board's title row happened to read, and the board
/// of the first event titles itself "Arcathlon" with no number, exactly
/// like a randomized draw does. The roster tells them apart for free —
/// the ten games of a numbered event all fit one roster, and a randomized
/// draw crosses all nine and fits none.
///
/// A session holding one game is not an event and is emitted as itself,
/// which is what a day of practising a single game looks like.
///
/// One correction to "the session IS the event": a broadcast can be cut in
/// two. Twitch split 2026-07-23 across two VODs over a 39-second drop, so
/// its Arcathlon #4 arrived as a nine-game session and a one-game session,
/// and the page showed a spurious one-game "Astyanax" event beside a #4
/// that looked incomplete. Neither was true and no data was missing.
///
/// So adjacent sessions of the same day carrying the same tag are joined,
/// but only when no game appears in both. That last condition is what
/// makes it safe: an event is ten DISTINCT games, so two genuine events
/// can never merge without colliding, and the audit leans on the same
/// invariant. Over the whole corpus this fires exactly once.
fn other_events(runs: &[db::OtherRun], rosters: &roster::Rosters) -> Vec<serde_json::Value> {
    // Session boundaries first, then the joins.
    let mut groups: Vec<Vec<&db::OtherRun>> = Vec::new();
    let mut i = 0;
    while i < runs.len() {
        let session = runs[i].session;
        let j = runs[i..].partition_point(|r| r.session == session) + i;
        groups.push(runs[i..j].iter().collect());
        i = j;
    }
    let joinable = |a: &Vec<&db::OtherRun>, b: &Vec<&db::OtherRun>| {
        a[0].day == b[0].day
            && a[0].tag.is_some()
            && a[0].tag == b[0].tag
            && !b
                .iter()
                .any(|r| a.iter().any(|x| x.game.eq_ignore_ascii_case(&r.game)))
    };
    let mut joined: Vec<Vec<&db::OtherRun>> = Vec::new();
    for g in groups {
        match joined.last() {
            Some(prev) if joinable(prev, &g) => joined.last_mut().unwrap().extend(g),
            _ => joined.push(g),
        }
    }

    let mut out: Vec<serde_json::Value> = Vec::new();
    for group in &joined {
        let names: Vec<&str> = group.iter().map(|r| r.game.as_str()).collect();
        // One game is a practice day, not an event to identify.
        let event = (group.len() > 1)
            .then(|| rosters.identify(&names))
            .flatten();
        let label = match (event, group.len()) {
            (Some(e), _) => format!("Arcathlon {}", rosters.event_name(e)),
            (None, n) if n > 1 => "Randomized Arcathlon".to_string(),
            _ => group[0].game.clone(),
        };
        out.push(serde_json::json!({
            "day": group[0].day,
            "started_at_ms": group[0].started_at_ms,
            "label": label,
            // Whether the ten games fit one roster. A reader sorting for
            // the randomized draws wants this, not the label's spelling.
            "randomized": event.is_none() && group.len() > 1,
            "tag": group[0].tag,
            "total_ms": group.iter().filter_map(|r| r.final_time_ms).sum::<i64>(),
            "games": group.iter().map(|r| serde_json::json!({
                "game": r.game,
                "category": r.category,
                "started_at_ms": r.started_at_ms,
                "final_time_ms": r.final_time_ms,
                "outcome": r.outcome,
            })).collect::<Vec<_>>(),
        }));
    }
    out.reverse(); // newest first, which is how the page reads them
    out
}

pub async fn run(cfg: Config, json: bool, api_dir: Option<&Path>) -> Result<()> {
    let pool = db::open(&cfg.database.path).await?;
    let (game, category) = app::load_game(&pool, &cfg).await?;
    if let Some(dir) = api_dir {
        let opts = api::Options::from_config(&cfg);
        let today = db::local_today(&pool).await?;
        let feed = api::build(&pool, &game, &category, &opts, &today, util::unix_ms()).await?;
        let w = api::write(&feed, dir)?;
        println!(
            "wrote {} files ({} bytes) to {}: {} days, {} closed, today {}",
            w.files,
            w.bytes,
            dir.display(),
            feed.days.len(),
            feed.days.iter().filter(|d| d.closed).count(),
            today,
        );
        return Ok(());
    }
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
    // Every session: the site needs tags and capture health per day. Its
    // attempts, finishes and best are this game's, so a broadcast that also
    // recorded other games (a marathon day) still reads as what it did for
    // the game this page is about.
    let sessions = db::recent_sessions(&pool, &game, &category, 100_000).await?;
    // The whole JSON is embedded in a public page: a session's label is a
    // channel name for live capture but a local file path for source =
    // "file", which has no business being published.
    let public_sessions: Vec<serde_json::Value> = sessions
        .iter()
        .map(|s| {
            let mut v = serde_json::to_value(s).unwrap_or_default();
            if let Some(o) = v.as_object_mut() {
                o.remove("label");
                // A VOD id names the channel as surely as the label does;
                // the page and the feed name the game only, until the owner
                // says otherwise.
                if !cfg.game.public_vod_links {
                    o.remove("vod_id");
                    o.remove("vod_created_at_ms");
                }
            }
            v
        })
        .collect();
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
        let mut references: Vec<serde_json::Value> = cfg
            .game
            .references
            .iter()
            .filter_map(|r| {
                r.ms()
                    .map(|ms| serde_json::json!({"label": r.label, "ms": ms}))
            })
            .collect();
        // Reference times read off the layout itself replace the configured
        // ones of the same name — the streamer keeps them current, we don't.
        for (key, label) in [("ls_wr_ms", "WR"), ("ls_pb_ms", "Lifetime PB")] {
            if let Some(ms) = db::get_setting(&pool, key)
                .await?
                .and_then(|s| s.parse::<i64>().ok())
            {
                references.retain(|r| r["label"] != label);
                references.push(serde_json::json!({"label": label, "ms": ms}));
            }
        }
        let doc = serde_json::json!({
            "generated_at_ms": util::unix_ms(),
            "current_game": game,
            "current_category": category,
            "record_label": cfg.game.record_label,
            "references": references,
            // The layout's own season best outranks the configured baseline:
            // it is what his comparison column actually shows.
            "baseline_best_ms": db::get_setting(&pool, "ls_season_best_ms")
                .await?
                .and_then(|s| s.parse::<i64>().ok())
                .or(cfg.game.baseline_best_ms()),
            "ls_pb_ms": db::get_setting(&pool, "ls_pb_ms")
                .await?
                .and_then(|s| s.parse::<i64>().ok()),
            "ls_sob_ms": db::get_setting(&pool, "ls_sob_ms")
                .await?
                .and_then(|s| s.parse::<i64>().ok()),
            "summaries": summaries,
            // Every other game, grouped back into the broadcasts it was
            // played in. The page pivots this both ways: by event, and by
            // game across events.
            "other_events": other_events(
                &db::other_runs(&pool, &game, &category).await?,
                &roster::Rosters::bundled().unwrap_or_default(),
            ),
            "today": today,
            "runs": all_runs,
            "splits_by_run": splits_by_run,
            "daily": daily,
            "sessions": public_sessions,
            // Days are the streamer's, not the reader's: the browser must not
            // re-derive them in its own timezone or a reader abroad sees a
            // broadcast split across two chips that the rest of the page
            // counts as one.
            "day_offset_minutes": util::local_utc_offset_minutes(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn run(session: i64, game: &str, ms: i64) -> db::OtherRun {
        db::OtherRun {
            game: game.into(),
            category: "Arcathlon".into(),
            started_at_ms: 1_000 + session * 100 + ms / 1000,
            final_time_ms: Some(ms),
            outcome: "finished".into(),
            session,
            tag: Some("Arcathlon".into()),
            day: "2026-08-28".into(),
        }
    }

    /// The board of the FIRST event titles itself "Arcathlon" with no
    /// number, exactly as a randomized draw does, so the session tag
    /// cannot tell them apart and the games have to. Ten games that all
    /// belong to one roster are that event; ten drawn from across the
    /// pool are a random draw.
    #[test]
    fn an_event_is_named_by_its_games_not_by_its_tag() {
        let r = roster::Rosters::bundled().expect("the bundled rosters parse");

        // Event #1 as his board lists it, tagged only "Arcathlon".
        let one: Vec<db::OtherRun> = [
            "Batman",
            "Castlevania",
            "Ninja Gaiden",
            "Ninja Gaiden II",
            "Ninja Gaiden III",
            "Super Mario Bros",
            "Super Mario Bros 2",
            "Super Mario Bros 3",
            "Zelda",
            "Zelda II",
        ]
        .iter()
        .enumerate()
        .map(|(i, g)| run(1, g, 600_000 + i as i64 * 1000))
        .collect();

        // A draw crossing several events, tagged the same way.
        let rando: Vec<db::OtherRun> = [
            "Astyanax",
            "King Kong 2",
            "Super Mario Bros 3",
            "Batman: ROTJ",
            "Kabuki Quantum Fighter",
            "Hebereke",
            "Batman",
            "Super Mario Bros 2",
            "Metal Storm",
            "Chip N Dale",
        ]
        .iter()
        .enumerate()
        .map(|(i, g)| run(2, g, 700_000 + i as i64 * 1000))
        .collect();

        let mut runs = one;
        runs.extend(rando);
        let out = other_events(&runs, &r);
        assert_eq!(out.len(), 2, "one event per session");

        // Newest first: session 2 leads.
        assert_eq!(out[0]["label"], "Randomized Arcathlon");
        assert_eq!(out[0]["randomized"], true);
        assert_eq!(out[1]["label"], "Arcathlon #1");
        assert_eq!(out[1]["randomized"], false);
        assert_eq!(out[1]["games"].as_array().unwrap().len(), 10);
        // The total is the sum of what the board timed, not a wall clock.
        assert_eq!(
            out[1]["total_ms"].as_i64().unwrap(),
            (0..10).map(|i| 600_000 + i * 1000).sum::<i64>()
        );
    }

    /// Twitch cut 2026-07-23 in two over a 39-second drop, so one Arcathlon
    /// #4 arrived as a nine-game session and a one-game session and the page
    /// showed a spurious "Astyanax" event beside an incomplete #4. Adjacent
    /// same-day, same-tag sessions rejoin.
    #[test]
    fn a_broadcast_split_across_two_vods_is_one_event() {
        let r = roster::Rosters::bundled().unwrap();
        let nine = [
            "Castlevania II",
            "Cowboy Kid",
            "Goonies II",
            "Hebereke",
            "Little Samson",
            "Panic Restaurant",
            "Shadowgate",
            "Solstice",
            "TMNT III",
        ];
        let mut runs: Vec<db::OtherRun> = nine
            .iter()
            .enumerate()
            .map(|(i, g)| run(1, g, 600_000 + i as i64 * 1000))
            .collect();
        runs.push(run(2, "Astyanax", 1_399_000)); // the tail VOD

        let out = other_events(&runs, &r);
        assert_eq!(out.len(), 1, "one broadcast, not two");
        assert_eq!(out[0]["label"], "Arcathlon #4");
        assert_eq!(out[0]["games"].as_array().unwrap().len(), 10);
        assert_eq!(out[0]["randomized"], false);
    }

    /// The join must never fuse two genuine events. An Arcathlon is ten
    /// DISTINCT games, so a repeated name is proof the two halves are not
    /// one broadcast — which is the whole safety of the rule.
    #[test]
    fn sessions_sharing_a_game_are_never_joined() {
        let r = roster::Rosters::bundled().unwrap();
        // Two same-day, same-tag sessions that both hold Batman.
        let runs = vec![
            run(1, "Batman", 700_000),
            run(1, "Castlevania", 800_000),
            run(2, "Batman", 710_000),
            run(2, "Zelda", 900_000),
        ];
        let out = other_events(&runs, &r);
        assert_eq!(out.len(), 2, "a shared game keeps them apart");
    }

    /// A tagless session joins nothing: without a tag there is no evidence
    /// the two halves belong together, and two practice sessions on one day
    /// are not a marathon.
    #[test]
    fn untagged_sessions_are_never_joined() {
        let r = roster::Rosters::bundled().unwrap();
        let mut a = run(1, "Solstice", 600_000);
        let mut b = run(2, "Hebereke", 700_000);
        a.tag = None;
        b.tag = None;
        let out = other_events(&[a, b], &r);
        assert_eq!(out.len(), 2);
    }

    /// A day spent on one game is not a marathon and must not be named
    /// after an event it has nothing to do with.
    #[test]
    fn a_single_game_session_is_itself() {
        let r = roster::Rosters::bundled().unwrap();
        let out = other_events(&[run(9, "Die Hard (NES)", 142_000)], &r);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["label"], "Die Hard (NES)");
        assert_eq!(out[0]["randomized"], false);
    }

    /// Without rosters nothing can be identified, and every marathon has
    /// to read as a draw rather than as a wrong event.
    #[test]
    fn no_rosters_means_no_event_is_claimed() {
        let none = roster::Rosters::default();
        let runs: Vec<db::OtherRun> = ["Batman", "Castlevania", "Zelda"]
            .iter()
            .map(|g| run(1, g, 600_000))
            .collect();
        let out = other_events(&runs, &none);
        assert_eq!(out[0]["label"], "Randomized Arcathlon");
        assert_eq!(out[0]["randomized"], true);
    }
}
