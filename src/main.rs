//! The `ngtwitchtimer` binary: one subcommand per job. `run` is the bot
//! (`app.rs`); `report` projects the database for the site and the feed
//! (`report.rs`, `api.rs`); `locate`, `calibrate` and `pane` are the layout
//! tools (`locate.rs`, `calibrate.rs`, `pane.rs`); `glyphs` trains and tests
//! the timer's template reader (`glyph.rs`); `audit` replays the marathon
//! tracker over recorded board logs (`audit.rs`). Logging goes to stderr so
//! `report --json` stays valid JSON on stdout. See docs/overview.md.

mod api;
mod app;
mod audit;
mod board;
mod calibrate;
mod capture;
mod chat;
mod config;
mod counter;
mod db;
mod glyph;
mod identity;
mod locate;
mod lock;
mod marathon;
mod ocr;
mod pane;
mod report;
mod roster;
mod sanity;
mod signature;
mod splits;
mod state;
mod stats;
mod timeparse;
mod twitch_hls;
mod util;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Tracks a Twitch streamer's speedrun attempts by OCR-reading the on-screen
/// LiveSplit timer from the public stream.
#[derive(Parser)]
#[command(name = "ngtwitchtimer", version, about)]
struct Cli {
    /// Path to the TOML config file
    #[arg(short, long, default_value = "config.toml")]
    config: std::path::PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Watch the stream, detect runs, log them to SQLite (default)
    Run,
    /// Tune the crop rectangle: saves cropped/preprocessed PNGs and prints
    /// live OCR readings
    Calibrate {
        /// Capture the whole canvas-scaled frame instead of the crop
        #[arg(long)]
        full_frame: bool,
    },
    /// Summarize the collected data (PBs, today, recent runs)
    Report {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
        /// Write the per-day feed here instead: days/<day>.json, history.json,
        /// schema.json and manifest.json (the site's api/v1 directory)
        #[arg(long, value_name = "DIR", conflicts_with = "json")]
        api_dir: Option<std::path::PathBuf>,
    },
    /// Find the LiveSplit pane in a frame (needs the tesseract CLI): prints
    /// crop rectangles as a ready-to-paste [[layouts]] entry and how far it
    /// sits from each configured layout; saves calibration/locate.png with
    /// the boxes drawn
    Locate {
        /// Analyze this PNG/JPEG instead of grabbing a frame from the source
        #[arg(long)]
        image: Option<std::path::PathBuf>,
        /// Give up after analyzing this many frames without finding a timer
        #[arg(long, default_value_t = 12)]
        frames: u32,
    },
    /// What the pane pass would read off one frame, for every layout: the
    /// board as read, what classify calls it, and the board probe's score at
    /// each threshold. Reads a canvas-scaled PNG or grabs one frame
    Pane {
        /// Analyze this PNG instead of grabbing a frame from the source
        #[arg(long)]
        image: Option<std::path::PathBuf>,
        /// Binarisation threshold(s) to try; default: the [splits] threshold, then 100
        #[arg(long = "threshold")]
        thresholds: Vec<u8>,
        /// Only this layout
        #[arg(long)]
        layout: Option<String>,
        /// Write a board-reader fixture (tests/fixtures/board format) of the
        /// pane as read for --layout at the first --threshold, with an
        /// `expected` block to fill in by hand
        #[arg(long, value_name = "FILE")]
        dump_fixture: Option<std::path::PathBuf>,
    },
    /// The purpose-built timer digit reader: harvest templates from replay
    /// corpora (NG_DUMP_TIMER=all) and score them. Crops are segmented at
    /// the --config's [timer] threshold, so pass the config the corpus was
    /// dumped with
    Glyphs {
        #[command(subcommand)]
        action: GlyphsAction,
    },
    /// Check every replayed marathon broadcast against the answer key its own
    /// board derives: which of the event's games he played (the row he plays
    /// is the one whose time changes), what the tracker recorded, and the
    /// differences in both directions. Needs no video, no timer and no
    /// outside answer key — only the logs `scripts/replay-arcathlon.sh`
    /// leaves. See scripts/audit-arcathlon.sh
    Audit {
        /// The capture working set: boards-<vod>.jsonl and obs-<vod>.jsonl
        /// per broadcast
        #[arg(long, default_value = "arcathlon-db")]
        dir: std::path::PathBuf,
    },
}

#[derive(Subcommand, Debug)]
enum GlyphsAction {
    /// Build a template file from corpus directories
    Train {
        /// Corpus directories (each holds obs.jsonl and calibration/timer-*.png)
        #[arg(required = true)]
        corpus: Vec<std::path::PathBuf>,
        /// Where to write the templates
        #[arg(long, default_value = "assets/glyphs.json")]
        out: std::path::PathBuf,
        /// Templates kept per character
        #[arg(long, default_value_t = 24)]
        per_class: usize,
    },
    /// Show how crops segment and how each glyph scores
    Boxes {
        /// Timer crops (PNG) to segment and score
        #[arg(required = true)]
        files: Vec<std::path::PathBuf>,
        /// Template file
        #[arg(long, default_value = "assets/glyphs.json")]
        templates: std::path::PathBuf,
    },
    /// Read every confirmed frame of the corpora and compare with its label
    Test {
        /// Corpus directories, as for `train`
        #[arg(required = true)]
        corpus: Vec<std::path::PathBuf>,
        /// Template file
        #[arg(long, default_value = "assets/glyphs.json")]
        templates: std::path::PathBuf,
        /// Save the crops the reader got wrong here, named by label and reading
        #[arg(long)]
        dump_wrong: Option<std::path::PathBuf>,
        /// Try other decision floors than the reader's own (0.55 score,
        /// 0.12 margin); only takes effect together with --min-margin
        #[arg(long)]
        min_score: Option<f32>,
        /// Lowest margin over the runner-up class; only takes effect together
        /// with --min-score
        #[arg(long)]
        min_margin: Option<f32>,
    },
}

/// libtesseract is built against OpenMP, and on crops this small its threads
/// cost far more in spin-waiting than they save: several workers on one box
/// starve each other (measured: 4 in-process workers at 1.1x realtime each,
/// versus 5-10x with one thread apiece). libgomp reads its environment in a
/// load-time constructor, before `main`, so setting the variable here would
/// be too late — re-exec once with it set instead. Cheap, happens before any
/// work, and applies however the binary was launched.
fn limit_openmp_threads() {
    if std::env::var_os("OMP_THREAD_LIMIT").is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env("OMP_THREAD_LIMIT", "1")
        .env("OMP_NUM_THREADS", "1")
        .exec();
    // exec only returns on failure; carry on with OpenMP as it is.
    eprintln!("could not re-exec with OMP_THREAD_LIMIT=1 ({err}); continuing");
}

/// Name the process-level TLS crypto provider. Two are compiled in — ring
/// through reqwest, aws-lc-rs through twitch-irc's rustls default — and
/// `rustls::ClientConfig::builder()`, which twitch-irc calls, panics with both
/// present until one is installed. Idempotent.
fn install_tls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[tokio::main]
async fn main() -> Result<()> {
    // Die quietly on a closed pipe (`report | head`) like a normal unix tool
    // instead of panicking.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    limit_openmp_threads();
    install_tls_provider();
    // Logs go to stderr, every subcommand: `report --json` prints the site's
    // JSON on stdout, and a config-time INFO line (the [[games]] roster load)
    // in front of it would break the site build.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let cfg = config::Config::load(&cli.config)?;
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => app::run(cfg).await,
        Command::Calibrate { full_frame } => calibrate::run(cfg, full_frame).await,
        Command::Report { json, api_dir } => report::run(cfg, json, api_dir.as_deref()).await,
        Command::Locate { image, frames } => locate::run(cfg, image, frames).await,
        Command::Pane {
            image,
            thresholds,
            layout,
            dump_fixture,
        } => pane::run(cfg, image, thresholds, layout, dump_fixture).await,
        Command::Audit { dir } => audit::run(&cfg, &dir).map(|_| ()),
        Command::Glyphs { action } => match action {
            GlyphsAction::Train {
                corpus,
                out,
                per_class,
            } => glyph::cli_train(&corpus, &out, cfg.timer.threshold, per_class),
            GlyphsAction::Boxes { files, templates } => {
                glyph::cli_boxes(&files, &templates, cfg.timer.threshold)
            }
            GlyphsAction::Test {
                corpus,
                templates,
                dump_wrong,
                min_score,
                min_margin,
            } => glyph::cli_test(
                &corpus,
                &templates,
                cfg.timer.threshold,
                dump_wrong.as_deref(),
                min_score.zip(min_margin),
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_tls_client_config_builds_once_the_provider_is_named() {
        super::install_tls_provider();
        super::install_tls_provider();
        let _ = rustls::ClientConfig::builder();
    }
}
