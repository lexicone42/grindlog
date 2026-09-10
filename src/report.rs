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
fn other_events(
    runs: &[db::OtherRun],
    rosters: &roster::Rosters,
    practice_cats: &[&str],
) -> Vec<serde_json::Value> {
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
        // A PRACTICE session is not an event and must never be identified as
        // one. A marathon is one run of each of ten games; a practice day is
        // many attempts at one or two, and the two arrive here as the same
        // shape — a session holding several runs of several games. Without
        // this test the "more than one game, fits no roster" branch below
        // takes over and a Big 20 practice day is published as "Randomized
        // Arcathlon", which is not a thing that happened.
        //
        // The category is what separates them, because it is what separates
        // them in the database: `follow_title = "track"` files practice
        // under the category the roster that named the game gives ("Big 20
        // #23"), or `game.other_category` when it gives none, and a
        // marathon import files completions under the board's own name.
        let practice = group.iter().all(|r| {
            practice_cats
                .iter()
                .any(|c| r.category.eq_ignore_ascii_case(c))
        });
        let distinct: Vec<&str> = {
            let mut v: Vec<&str> = names.clone();
            v.sort_unstable();
            v.dedup();
            v
        };
        // One game is a practice day, not an event to identify.
        let event = (!practice && group.len() > 1)
            .then(|| rosters.identify(&names))
            .flatten();
        let label = match (practice, event, distinct.len()) {
            // Named by what he actually practised. Beyond three the names
            // stop being a label and become a list, and the page has the
            // games themselves right underneath.
            (true, _, n) if n > 3 => format!("{n} games"),
            (true, _, _) => distinct.join(", "),
            (_, Some(e), _) => format!("Arcathlon {}", rosters.event_name(e)),
            (_, None, n) if n > 1 => "Randomized Arcathlon".to_string(),
            _ => group[0].game.clone(),
        };
        out.push(serde_json::json!({
            "day": group[0].day,
            "started_at_ms": group[0].started_at_ms,
            "label": label,
            // Whether the ten games fit one roster. A reader sorting for
            // the randomized draws wants this, not the label's spelling.
            // Never a practice session: it is not a draw of any kind.
            "randomized": !practice && event.is_none() && group.len() > 1,
            // And which of the two this is, said plainly rather than left
            // to be inferred from the absence of the other flag.
            "practice": practice,
            "tag": group[0].tag,
            // The event's running total, which a practice day does not have:
            // adding up five Die Hard finishes out of forty attempts gives a
            // number that is the sum of nothing anyone ran.
            "total_ms": (!practice).then(|| group.iter().filter_map(|r| r.final_time_ms).sum::<i64>()),
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

/// Every category that means "his own practice", as opposed to a completion
/// in one of his marathons: the category each shipped event files its games
/// under, plus the deployment's own catch-all.
///
/// Both, not one. `game.other_category` was what `track` wrote until
/// 2026-09-10, and the runs it wrote before the roster took the job over
/// are still in the database; dropping it here would strand them outside
/// every page that exists to show them.
fn practice_categories(cfg: &Config) -> Vec<String> {
    let mut v: Vec<String> = roster::bundled_all()
        .iter()
        .flat_map(|r| r.categories().into_iter().map(str::to_string))
        .collect();
    v.push(cfg.game.other_category.clone());
    v.sort();
    v.dedup();
    v
}

/// The Big 20 race lineup joined against what the database holds, for the
/// page that tracks his preparation for it.
///
/// Two numbers per game, kept apart on purpose:
///
///   **practice** — attempts under his own splits, which is what `track`
///   records and what preparing for the race actually looks like: forty
///   Die Hard attempts and five finishes.
///   **marathon** — his time for that game in one of his own Arcathlons.
///   Six of the twenty are also Arcathlon games, and a single completion
///   from a marathon two months ago is a real data point about the game
///   and is NOT practice for this race. Summing them together would make
///   an untouched game look prepared.
///
/// A game he has never played carries neither, which is the whole point:
/// the page is a list of twenty and the empty rows are the news.
fn big20_prep(summaries: &[db::GameSummary], cfg: &Config) -> serde_json::Value {
    let Some((rosters, event)) = roster::big20() else {
        return serde_json::Value::Null;
    };
    // Practice is exactly what `follow_title = "track"` writes, which is
    // this category and nothing else. Everything else under one of these
    // names is an appearance somewhere else — an Arcathlon completion,
    // almost always.
    //
    // Asked the other way round first (marathon = a category naming a
    // `[[games]]` entry in board mode) it got every game wrong that
    // mattered: `mode = "board"` is not enabled in live.toml, so that list
    // was empty and Jaws' five Arcathlon completions read as five practice
    // attempts with a 100% finish rate. The categories runs are actually
    // filed under are the thing to ask about, not the config that would
    // have produced them.
    //
    // Since 2026-09-10 that is the race's own category ("Big 20 #23", from
    // the roster) rather than the config's catch-all, and the config value
    // is still accepted so the two rows recorded before that change are not
    // stranded outside the page that exists to show them.
    let practice_cats: Vec<&str> = rosters
        .categories()
        .into_iter()
        .chain(std::iter::once(cfg.game.other_category.as_str()))
        .collect();
    let (url, date) = rosters.event_source(event);
    let games: Vec<serde_json::Value> = rosters
        .lineup(event)
        .into_iter()
        .enumerate()
        .map(|(i, (name, goal))| {
            // Fold each summary's game name through the roster before
            // comparing: a run recorded from a board that read "aws" is
            // this race's Jaws, and a page that matched on the raw string
            // would call the game untouched while its runs sat on their
            // own per-game page.
            let mine = |s: &db::GameSummary| {
                rosters
                    .canonical(&s.game)
                    .map(|c| c.eq_ignore_ascii_case(name))
                    .unwrap_or(false)
            };
            let is_practice = |s: &db::GameSummary| {
                practice_cats
                    .iter()
                    .any(|c| s.category.eq_ignore_ascii_case(c))
            };
            let practice: Vec<&db::GameSummary> = summaries
                .iter()
                .filter(|s| mine(s) && is_practice(s))
                .collect();
            let elsewhere: Vec<&db::GameSummary> = summaries
                .iter()
                .filter(|s| mine(s) && !is_practice(s))
                .collect();
            let best = |v: &[&db::GameSummary]| v.iter().filter_map(|s| s.best_ms).min();
            serde_json::json!({
                // Its place in the race, which is the order the page lists
                // them in and the order he will run them on the day.
                "n": i + 1,
                "game": name,
                "goal": goal,
                "attempts": practice.iter().map(|s| s.attempts).sum::<i64>(),
                "finished": practice.iter().map(|s| s.finished).sum::<i64>(),
                "best_ms": best(&practice),
                "first_at_ms": practice.iter().filter_map(|s| s.first_at_ms).min(),
                "last_at_ms": practice.iter().filter_map(|s| s.last_at_ms).max(),
                // His Arcathlon time for it, where there is one.
                "marathon_ms": best(&elsewhere),
                "marathon_at_ms": elsewhere.iter().filter_map(|s| s.last_at_ms).max(),
            })
        })
        .collect();
    serde_json::json!({
        "race": rosters.event_name(event),
        "url": url,
        "date": date,
        "games": games,
    })
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
    let practice = practice_categories(&cfg);
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
    let recent = db::recent_runs(&pool, &game, &category, 15).await?;
    let brief = db::runs_brief(&pool, &game, &category).await?;
    let acts = cfg.game.act_list();
    let deaths = stats::death_chart(&brief, &acts);
    let survival = stats::survival(&brief, &acts);
    let pbs = stats::pb_history(&brief);
    let streaks = stats::streaks(&brief);
    let golds = db::golds(&pool, &game, &category).await?;

    if json {
        // What the pane last read, with the game canonicalised before it is
        // shown to a person. The stored title event is the RAW header —
        // that is where "Kiown in Night Mayor World" comes from, and the
        // twenty-one spellings of Double Dragon II. The tracker already
        // folds those onto canonical names; the page was not getting the
        // benefit and published the damaged reading. Falls back to the raw
        // text when nothing on the shipped rosters fits, which is the
        // honest answer: better a damaged name than a confident wrong one.
        let mut now = db::now_playing(&pool).await?;
        if let Some(raw) = now.game.as_deref() {
            if let Some(c) = roster::canonical_any(raw) {
                now.game = Some(c);
            }
        }
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
            // The twenty games of the Big 20 race, in race order, each with
            // whatever this database holds for it. Present whether or not he
            // has practised any of them: a prep page's job is to show what
            // is left as much as what is done.
            "big20": big20_prep(&summaries, &cfg),
            // What the pane last saw, whatever game it was. The page leads
            // with this rather than with the tracked game, because on a
            // Big 20 day the tracked game is not what is happening.
            "now": now,
            // The channel, so the live panel can link to the stream it is
            // reading.
            //
            // This NAMES THE STREAMER, which the page and the feed had
            // deliberately not done. The owner asked for the link, so the
            // decision is made and this is unconditional rather than a
            // flag — but it is deliberately NOT wired to
            // `public_vod_links`, which stays off. That flag publishes a
            // deep link to the moment of every individual run, which is a
            // much larger disclosure than naming the channel, and the two
            // should not ride together just because both mention Twitch.
            "channel": cfg.stream.channel,
            // Every other game, grouped back into the broadcasts it was
            // played in. The page pivots this both ways: by event, and by
            // game across events.
            "other_events": other_events(
                &db::other_runs(&pool, &game, &category).await?,
                &roster::Rosters::bundled().unwrap_or_default(),
                &practice.iter().map(String::as_str).collect::<Vec<_>>(),
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
        let out = other_events(&runs, &r, &["Other"]);
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

        let out = other_events(&runs, &r, &["Other"]);
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
        let out = other_events(&runs, &r, &["Other"]);
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
        let out = other_events(&[a, b], &r, &["Other"]);
        assert_eq!(out.len(), 2);
    }

    /// A day spent on one game is not a marathon and must not be named
    /// after an event it has nothing to do with.
    #[test]
    fn a_single_game_session_is_itself() {
        let r = roster::Rosters::bundled().unwrap();
        let out = other_events(&[run(9, "Die Hard (NES)", 142_000)], &r, &["Other"]);
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
        let out = other_events(&runs, &none, &["Other"]);
        assert_eq!(out[0]["label"], "Randomized Arcathlon");
        assert_eq!(out[0]["randomized"], true);
    }

    /// A day of Big 20 practice is several games and many attempts in one
    /// session, which is the SAME SHAPE a randomized marathon arrives in.
    /// Taken for an event it is published as "Randomized Arcathlon" — a
    /// marathon that never happened, on the page and in the feed — with a
    /// total that is the sum of whichever attempts he happened to finish.
    /// The category is what tells them apart.
    #[test]
    fn a_practice_day_is_not_a_marathon_however_many_games_it_holds() {
        let r = roster::Rosters::bundled().unwrap();
        let practice = |game: &'static str, ms: Option<i64>| db::OtherRun {
            category: "Other".into(),
            final_time_ms: ms,
            outcome: if ms.is_some() { "finished" } else { "reset" }.into(),
            tag: None,
            ..run(7, game, 0)
        };
        let out = other_events(
            &[
                practice("Die Hard", Some(142_000)),
                practice("Die Hard", None),
                practice("Die Hard", Some(121_000)),
                practice("Kid Klown in Night Mayor World", None),
            ],
            &r,
            &["Other"],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["label"], "Die Hard, Kid Klown in Night Mayor World");
        assert_eq!(out[0]["practice"], true);
        assert_eq!(out[0]["randomized"], false, "not a draw of anything");
        assert!(
            out[0]["total_ms"].is_null(),
            "a practice day has no running total"
        );
        assert_eq!(out[0]["games"].as_array().unwrap().len(), 4);

        // Beyond three games the names stop being a label and become a list.
        let many: Vec<db::OtherRun> = ["Die Hard", "Jaws", "Faria", "Yoshi", "Hydlide"]
            .iter()
            .map(|g| practice(g, None))
            .collect();
        let out = other_events(&many, &r, &["Other"]);
        assert_eq!(out[0]["label"], "5 games");
        assert_eq!(out[0]["practice"], true);

        // And a real marathon is untouched: its runs are not in that
        // category, so it identifies exactly as it did before.
        let arca: Vec<db::OtherRun> = ["Batman", "Castlevania", "Zelda"]
            .iter()
            .map(|g| run(1, g, 600_000))
            .collect();
        let out = other_events(&arca, &r, &["Other"]);
        assert_eq!(out[0]["practice"], false);
        assert!(out[0]["total_ms"].is_number());
    }

    fn summary(
        game: &str,
        category: &str,
        attempts: i64,
        finished: i64,
        best: i64,
    ) -> db::GameSummary {
        db::GameSummary {
            game: game.into(),
            category: category.into(),
            best_ms: (finished > 0).then_some(best),
            finished,
            attempts,
            first_at_ms: Some(1_780_000_000_000),
            last_at_ms: Some(1_787_000_000_000),
        }
    }

    /// Practice for the race and an appearance in a marathon are different
    /// things and are counted apart. This is where the first version was
    /// wrong: it asked the CONFIG which categories were marathons, and
    /// `mode = "board"` is not enabled in the deployment, so the answer was
    /// "none" and Jaws' five Arcathlon completions read as five practice
    /// attempts with a perfect finish rate. The categories runs are FILED
    /// under are what decides.
    #[test]
    fn big20_prep_counts_practice_apart_from_a_marathon_appearance() {
        let mut cfg = Config::for_test_with_min_final(660_000);
        cfg.game.other_category = "Other".into();
        let out = big20_prep(
            &[
                // His practice for the race.
                summary("Die Hard", "Other", 10, 5, 121_400),
                // The same game read off a damaged board, which the roster
                // folds onto the race's name rather than leaving as a game
                // of its own.
                summary("aws", "Other", 3, 1, 400_000),
                // A marathon completion of a game that is also in the race.
                summary("Jaws", "Arcathlon", 5, 5, 420_000),
                // Nothing to do with this race.
                summary("Ninja Gaiden (NES)", "Any%", 3137, 41, 695_100),
            ],
            &cfg,
        );
        let by = |name: &str| {
            out["games"]
                .as_array()
                .unwrap()
                .iter()
                .find(|g| g["game"] == name)
                .unwrap()
                .clone()
        };
        let dh = by("Die Hard");
        assert_eq!(dh["n"], 1, "first in the race order");
        assert_eq!(dh["goal"], "Any% Beginner");
        assert_eq!(dh["attempts"], 10);
        assert_eq!(dh["best_ms"], 121_400);
        assert!(dh["marathon_ms"].is_null());

        let jaws = by("Jaws");
        assert_eq!(
            jaws["attempts"], 3,
            "the damaged spelling is folded in; the Arcathlon row is not"
        );
        assert_eq!(jaws["best_ms"], 400_000);
        assert_eq!(
            jaws["marathon_ms"], 420_000,
            "his marathon time, kept apart"
        );

        // A game he has never touched is still listed, with nothing in it.
        let untouched = by("Moon Crystal");
        assert_eq!(untouched["attempts"], 0);
        assert!(untouched["best_ms"].is_null());
        assert!(untouched["last_at_ms"].is_null());
        assert_eq!(out["games"].as_array().unwrap().len(), 20);
    }
}
