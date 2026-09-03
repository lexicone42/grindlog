mod app;
mod calibrate;
mod capture;
mod chat;
mod config;
mod counter;
mod db;
mod locate;
mod ocr;
mod report;
mod sanity;
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
    },
    /// Find the LiveSplit pane in a frame: prints crop rectangles as a
    /// ready-to-paste [[layouts]] entry and how far it sits from each
    /// configured layout; saves calibration/locate.png with the boxes drawn
    Locate {
        /// Analyze this PNG/JPEG instead of grabbing a frame from the source
        #[arg(long)]
        image: Option<std::path::PathBuf>,
        /// Give up after analyzing this many frames without finding a timer
        #[arg(long, default_value_t = 12)]
        frames: u32,
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

#[tokio::main]
async fn main() -> Result<()> {
    // Die quietly on a closed pipe (`report | head`) like a normal unix tool
    // instead of panicking.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    limit_openmp_threads();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let cfg = config::Config::load(&cli.config)?;
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => app::run(cfg).await,
        Command::Calibrate { full_frame } => calibrate::run(cfg, full_frame).await,
        Command::Report { json } => report::run(cfg, json).await,
        Command::Locate { image, frames } => locate::run(cfg, image, frames).await,
    }
}
