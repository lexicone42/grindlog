#!/usr/bin/env bash
# Nightly backup of the live database, safe while the bot is writing to it:
# sqlite's online `.backup` copies a consistent snapshot through the WAL,
# where a plain `cp` of a database in WAL mode can miss the latest writes.
#
#   scripts/backup-db.sh [db]          # default ninja-gaiden.db
#
# Writes backups/<name>-YYYY-MM-DD.db.gz, keeps 30 days locally, and — when
# NG_BACKUP_S3 is set to an s3:// prefix — copies the day's file there too.
# The site bucket is refused: everything in it is public.
set -euo pipefail
cd "$(dirname "$0")/.."
db="${1:-ninja-gaiden.db}"
[ -f "$db" ] || { echo "no database at $db" >&2; exit 1; }
name=$(basename "$db" .db)
mkdir -p backups
out="backups/$name-$(date +%F).db"
sqlite3 -cmd '.timeout 20000' "$db" ".backup '$out'"
# A backup that cannot be opened is not a backup. The copy inherits WAL
# mode; switching it to a rollback journal folds the sidecars away so the
# gzip holds one self-contained file.
n=$(sqlite3 "$out" "pragma journal_mode=delete; select count(*) from runs" | tail -1)
rm -f "$out-wal" "$out-shm"
gzip -f "$out"
echo "backed up $db -> $out.gz ($n runs)"
find backups -maxdepth 1 -name "$name-*.db.gz" -mtime +30 -delete
if [ -n "${NG_BACKUP_S3:-}" ]; then
  case "$NG_BACKUP_S3" in
    *ng.lexicone.com*) echo "refusing to back up into the public site bucket" >&2; exit 1 ;;
    s3://*) ;;
    *) echo "NG_BACKUP_S3 must be an s3:// prefix" >&2; exit 1 ;;
  esac
  aws s3 cp --only-show-errors "$out.gz" "${NG_BACKUP_S3%/}/$(basename "$out").gz"
  echo "copied to ${NG_BACKUP_S3%/}/"
fi
