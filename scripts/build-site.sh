#!/usr/bin/env bash
# Regenerate the records site from the database:
#   ./scripts/build-site.sh [config.toml]
# Produces site/index.html — a self-contained page (deployable anywhere
# static, e.g. lexicone.com; just copy the one file).
set -euo pipefail
cd "$(dirname "$0")/.."
cfg="${1:-live.toml}"
./target/release/ngtwitchtimer --config "$cfg" report --json > site/data.json
awk '
  /__DATA__/ { while ((getline l < "site/data.json") > 0) print l; next }
  { print }
' site/template.html > site/index.html
echo "wrote site/index.html ($(wc -c < site/index.html) bytes)"
