#!/usr/bin/env bash
# Reads backup keys (one per line, names hold UTC `YYYYMMDDTHHMMSSZ`) on stdin and prints the ones to delete.
# Keeps every dump for 2 days, then the first of each UTC day up to 14 days, then the first of each
# ISO week up to 30 days; everything older goes. `NOW` (epoch seconds) is for the test.
set -euo pipefail
now=${NOW:-$(date -u +%s)}
last_day= last_week=
while read -r key; do
  [[ $key =~ ([0-9]{8})T([0-9]{2})([0-9]{2})([0-9]{2})Z ]] || continue # a foreign object: never touch
  d=${BASH_REMATCH[1]}
  t=$(date -u -d "${d:0:4}-${d:4:2}-${d:6:2} ${BASH_REMATCH[2]}:${BASH_REMATCH[3]}:${BASH_REMATCH[4]}" +%s)
  age=$((now - t))
  week=$(date -u -d "${d:0:4}-${d:4:2}-${d:6:2}" +%G%V)
  first_of_day=0 first_of_week=0
  [[ $d != "$last_day" ]] && first_of_day=1
  [[ $week != "$last_week" ]] && first_of_week=1
  last_day=$d last_week=$week
  if ((age >= 30 * 86400)); then echo "$key"
  elif ((age >= 14 * 86400 && !first_of_week)); then echo "$key"
  elif ((age >= 2 * 86400 && !first_of_day)); then echo "$key"
  fi
done < <(sort)
