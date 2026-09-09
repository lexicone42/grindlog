# Installing the OCR stack

The bot needs `ffmpeg` and `tesseract`. This page is the detail for
getting tesseract working, including without root.

- **ffmpeg** (video decode)
- **tesseract** for OCR — any of the following. The in-process build does
  not fully replace the CLI: `locate` always shells out to it, and the
  default `ocr.engine = "auto"` falls back to it when the in-process engine
  cannot start. A binary off `$PATH` can be named with `ocr.tesseract_cmd`
  (default `"tesseract"`) instead of a wrapper script.
  - the `tesseract` CLI binary on `$PATH` (Gentoo: `emerge app-text/tesseract`,
    make sure the `eng` traineddata is installed) — used by the default build;
  - no root? A user-space install works fine (what the reference deployment
    uses): extract the [tesseract AppImage](https://github.com/AlexanderP/tesseract-appimage)
    with `--appimage-extract` into `~/.local/opt/tesseract-appimage/` and put a
    one-line wrapper at `~/.local/bin/tesseract`:
    `exec "$HOME/.local/opt/tesseract-appimage/AppRun" "$@"`;
  - or build with `cargo build --release --features leptess-ocr` for
    in-process OCR (needs libtesseract + libleptonica dev libraries). This
    is faster overall (the one measurement, on a VOD window: 669 to 964
    frames per 100 s of video, about 1.4x): ~70% of a CLI call is process
    startup and loading the language model, which the in-process engine
    pays once.
    Without system dev packages you can build against the AppImage's own
    shared libraries: fetch the matching `tesseract` and `leptonica` headers
    from their source releases into `~/.local/opt/ocr-dev/include/`, symlink
    `libtesseract.so.5` / `libleptonica.so.6` (and `libgif.so.7`) from the
    extracted AppImage into `~/.local/opt/ocr-dev/rt/`, write small
    `tesseract.pc` / `lept.pc` files pointing at them, then
    `PKG_CONFIG_PATH=~/.local/opt/ocr-dev/lib/pkgconfig RUSTFLAGS="-C link-arg=-Wl,-rpath,$HOME/.local/opt/ocr-dev/rt" cargo build --release --features leptess-ocr`.
    `scripts/build-release.sh` runs exactly this (plus
    `BINDGEN_EXTRA_CLANG_ARGS` pointing at the staged headers) whenever
    `~/.local/opt/ocr-dev`, or the directory named in `$OCR_DEV`, holds
    `lib/pkgconfig/tesseract.pc` and `rt/libtesseract.so.5`, and falls back
    to a plain `cargo build --release` (CLI engine) otherwise; extra
    arguments go through to cargo. It is what `scripts/rollout.sh` and the
    live deployment build with.
    Never put the whole AppImage `usr/lib` on `LD_LIBRARY_PATH` — it carries
    its own glibc. Set `ocr.tessdata_path` to the AppImage's
    `usr/share/tesseract-ocr/5/tessdata` and leave `ocr.engine` at its
    default `"auto"` (what the reference deployment uses): it picks the
    in-process engine when the binary has it and falls back to the CLI with
    a warning when it cannot start. `ocr.engine = "leptess"` makes that
    failure fatal instead.
- *(optional)* **streamlink**, only if you set `source = "streamlink"`.
  The default `hls` source resolves the stream URL itself in Rust.

Everything else (SQLite, HLS resolution, Twitch chat) is compiled in.
