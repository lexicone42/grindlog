mod app;
mod calibrate;
mod capture;
mod chat;
mod config;
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

#[tokio::main]
async fn main() -> Result<()> {
    // Die quietly on a closed pipe (`report | head`) like a normal unix tool
    // instead of panicking.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
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
