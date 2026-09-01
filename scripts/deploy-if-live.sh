#!/usr/bin/env bash
# Deploy the site only while a live session is open — run from cron every
# 10 minutes to make ng.lexicone.com near-real-time during streams.
set -euo pipefail
cd "$(dirname "$0")/.."
open=$(sqlite3 ninja-gaiden.db "SELECT COUNT(*) FROM sessions WHERE ended_at_ms IS NULL AND source='hls'" 2>/dev/null || echo 0)
[ "$open" -gt 0 ] || exit 0
exec ./scripts/deploy-site.sh
