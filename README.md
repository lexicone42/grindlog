# grindlog (`ngtwitchtimer`)

Watches a Twitch stream, OCR-reads the on-screen LiveSplit timer, and logs
every speedrun attempt to SQLite. Needs nothing from the streamer — no
capture card, no LiveSplit server, no chat presence. Everything it knows it
read off the public video.

It publishes a static records site (<https://ng.lexicone.com>) with daily
heartbeat timelines, death charts, gold segments and click-into-run splits,
and a JSON feed for bots.

The reference deployment follows **arcus** — one of the fastest *Ninja
Gaiden* (NES) runners in the world — but the game, layout rectangles and
record semantics are all configuration.

## Why it is more than a timer scraper

- **It survives the layout moving.** Streamers switch OBS scenes and nudge
  the LiveSplit window. The bot probes layouts across a grid of pixel
  offsets, re-anchors on drift, and re-measures the pane's row pitch every
  minute, so the splits column follows the window rather than a
  hand-measured rectangle.
- **It knows what it is looking at.** The pane's own header, category,
  attempt counter and row names are read on every pass and weighed against
  the tracked game, so a broadcast of something else is not recorded as
  this one. See [docs/detection.md](docs/detection.md).
- **It says how well it saw.** Every session records what share of the feed
  it read and every layout event; the site shows it per day, so a thin day
  is visible rather than silently thin.
- **It reads speedrun semantics, not just digits.** Seasonal best vs
  lifetime PB, run identity by the streamer's own LiveSplit attempt
  counter, and a plausibility floor so a frozen timer is never a finish.
- **It backfills.** Old VODs stream straight from Twitch and land on the
  original broadcast timeline.

Rust, no Python in the toolchain. The bot shells out only to `ffmpeg` and
`tesseract`; the scripts also use `sqlite3`, `curl`, `jq`, `flock` and the
AWS CLI for the site deploy.

**Status:** in daily use. Expect rough edges on layouts it was not
calibrated for — `locate` and `calibrate` exist to make new ones quick.

## Requirements

- **ffmpeg**
- **tesseract** — CLI on `$PATH`, or a user-space AppImage, or compiled in
  with `--features leptess-ocr` (about 1.4x faster). Root is not required.
  [docs/install.md](docs/install.md) has all three routes.

SQLite, HLS resolution and Twitch chat are compiled in.

## Setup

```sh
cargo build --release
cp config.example.toml config.toml   # edit: channel, crop rectangle
```

Every field is commented with its default in `config.example.toml`. Point
it at a new stream with `locate` and `calibrate`; see
[docs/detection.md](docs/detection.md#setting-it-up-for-a-new-streamer).

**Try it on a VOD before pointing it at a live stream.** Set `source =
"vod"` with a VOD id, or `source = "file"` with a local recording, and the
run lands on the original broadcast timeline.

## Running

```sh
ngtwitchtimer                    # watch, detect, log (config.toml in cwd)
ngtwitchtimer -c live.toml run   # any config file
ngtwitchtimer report             # PBs, today's stats, recent runs
ngtwitchtimer report --json      # machine-readable (feeds the site)
ngtwitchtimer locate             # find the LiveSplit pane in a frame
ngtwitchtimer calibrate          # tune the timer crop by eye
ngtwitchtimer glyphs train|test|boxes   # the timer's template reader
RUST_LOG=ngtwitchtimer=debug ngtwitchtimer   # per-frame tracing
```

It survives stream drops, goes dormant while the channel is offline, and
stores everything as millisecond integers in SQLite (`sessions`, `runs`,
`splits`, `transitions`, `settings`).

For a deployment that outlives the terminal, `scripts/run-live.sh`
supervises it under tmux and `scripts/rollout.sh` ships a new build. See
[docs/operations.md](docs/operations.md).

## Chat commands

Optional, under `[chat]`, with a bot account and an IRC OAuth token. Point
`channel` at your own channel to test without posting in the streamer's.
`command_prefix` namespaces everything for shared channels.

Viewers: `!pb`, `!lastrun`, `!today`, `!attempts`, `!deaths`, `!pace`,
`!splits`, `!golds`, `!status`. Mods (broadcaster, badge mods, or logins in
`chat.mods`): `!correct`, `!void`, `!note`. Each has a cooldown shared by
all viewers.

## Machine-readable data

Static JSON under `https://ng.lexicone.com/api/v1/`, rebuilt on every
deploy:

| file | size | what |
|---|---|---|
| `latest.json` | ~4 KB | the grind in one document — start here |
| `summary.json` | ~35 KB | every aggregate the site shows, no per-run rows |
| `report.json` | ~1 MB | every run, split and session |
| `manifest.json` + `days/<date>.json` | — | per-day feed, for fetching only what changed |

`/llms.txt` is the entry point for assistants;
[`site/static/api/v1/README.md`](site/static/api/v1/README.md) is the field
reference. Within `/api/v1/` only fields are added — a change of meaning
gets a new version path.

## Documentation

| | |
|---|---|
| [docs/detection.md](docs/detection.md) | how video becomes runs, and why each step is shaped that way |
| [docs/operations.md](docs/operations.md) | running the deployed bot: scripts, schedule, known footguns |
| [docs/install.md](docs/install.md) | getting ffmpeg and tesseract in place, including without root |
| [CLAUDE.md](CLAUDE.md) | ground rules for changing this repo safely |

## Contributing

Issues and PRs welcome. The best first contributions are layout
configurations for other streamers (send a `locate` printout and a frame)
and detection edge cases with an `obs_log` excerpt.

Two standing constraints. **Keep the bot Rust + ffmpeg + tesseract** — the
scripts may use `sqlite3`, `jq`, `curl`, `flock` and the AWS CLI, but
please add no new interpreters. And **for any change to OCR, locking or
detection, include `scripts/replay-window.sh` output for the old and the
new binary on the same VOD window**; a claim about detection without a
replay is not reviewable.

CI runs `cargo fmt --all -- --check`, `cargo clippy --release --all-targets
-- -D warnings`, `cargo build --release`, `cargo test --release` and
`bash -n scripts/*.sh`. Run those locally first — a formatting warning
fails the check. CI builds with default features, so the in-process OCR
path (`--features leptess-ocr`) is *not* compiled there.

The tests carry the behaviour: the state machine, timer parser, counter
tracker, geometry, glyph reader and roster matching are unit-tested against
real crops and real board logs under `tests/fixtures/`, and need neither
tesseract nor ffmpeg. Add a test with any behaviour change.

## License

Dual licensed under [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at
your option. Contributions are dual licensed the same way unless you say
otherwise.

Not affiliated with Twitch, LiveSplit, or the streamers it follows. It
reads publicly broadcast video only.
