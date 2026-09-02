#!/usr/bin/env bash
# Build the production binary (target/release/ngtwitchtimer). When the
# in-process OCR toolchain is staged under ~/.local/opt/ocr-dev (headers,
# pkg-config files and the AppImage's libtesseract/libleptonica — see README
# "Requirements"), build with the `leptess-ocr` feature: same engine, but the
# language model is loaded once instead of per call, ~2x faster overall. The
# runtime library path is baked in (rpath), so nothing else needs setting.
# Without that directory this is a plain `cargo build --release`.
set -euo pipefail
cd "$(dirname "$0")/.."
DEV="${OCR_DEV:-$HOME/.local/opt/ocr-dev}"
if [ -f "$DEV/lib/pkgconfig/tesseract.pc" ] && [ -f "$DEV/rt/libtesseract.so.5" ]; then
  echo "building with in-process OCR (leptess-ocr) against $DEV"
  PKG_CONFIG_PATH="$DEV/lib/pkgconfig" \
  BINDGEN_EXTRA_CLANG_ARGS="-I$DEV/include -I$DEV/include/leptonica" \
  RUSTFLAGS="-C link-arg=-Wl,-rpath,$DEV/rt" \
    cargo build --release --features leptess-ocr "$@"
  ldd target/release/ngtwitchtimer | grep -E 'tesseract|leptonica' | sed 's/^/  /'
else
  echo "building with the tesseract CLI engine (no $DEV toolchain)"
  cargo build --release "$@"
fi
