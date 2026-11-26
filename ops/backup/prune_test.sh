#!/usr/bin/env bash
# bash ops/backup/prune_test.sh: needs GNU date (the image has it; on macOS use `brew install coreutils` and PATH).
set -euo pipefail
cd "$(dirname "$0")"
NOW=$(date -u -d 2026-12-01T00:00:00Z +%s)
export NOW
k() { echo "backups/db/socialreins-$1.dump.age"; }
keys=$( {
  # 0-2 days old: all four of the day kept
  for h in 000000 060000 120000 180000; do k 20261130T${h}Z; k 20261129T${h}Z; done
  # 3 days old: only the first of the day survives
  for h in 000000 060000 120000 180000; do k 20261128T${h}Z; done
  # 20 days old, one ISO week: 2026-11-09 is a Monday; first of the week kept, the rest deleted
  k 20261110T000000Z; k 20261112T060000Z; k 20261109T060000Z
  # 40 days old
  k 20261021T000000Z
  echo backups/latest.json
} )
got=$(echo "$keys" | ./prune.sh | sort)
want=$( {
  for h in 060000 120000 180000; do k 20261128T${h}Z; done
  k 20261110T000000Z; k 20261112T060000Z
  k 20261021T000000Z
} | sort )
[[ $got == "$want" ]] || { echo "FAIL"; diff <(echo "$want") <(echo "$got") || true; exit 1; }
echo ok
