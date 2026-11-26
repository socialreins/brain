#!/usr/bin/env bash
# One backup run: pg_dump (custom format) -> age (public key only) -> S3-compatible bucket, then prune.
# Any failure exits non-zero, which is the alarm (Railway marks the run crashed). See README.md.
set -euo pipefail

: "${DATABASE_URL:?}" "${AGE_RECIPIENT:?}" "${S3_ENDPOINT:?}" "${S3_BUCKET:?}"
: "${S3_ACCESS_KEY_ID:?}" "${S3_SECRET_ACCESS_KEY:?}"
export AWS_ACCESS_KEY_ID=$S3_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY=$S3_SECRET_ACCESS_KEY
export AWS_DEFAULT_REGION=${S3_REGION:-auto} AWS_EC2_METADATA_DISABLED=true
PREFIX=${BACKUP_PREFIX:-backups}
s3() { aws --endpoint-url "$S3_ENDPOINT" "$@"; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
started=$(date +%s)
stamp=$(date -u +%Y%m%dT%H%M%SZ)
key="$PREFIX/db/socialreins-$stamp.dump.age"

# The plaintext lives only in this container's temp dir; --list proves the archive is complete.
timeout "${DUMP_TIMEOUT:-1800}" pg_dump --format=custom --no-owner --no-acl --dbname "$DATABASE_URL" --file "$work/db.dump"
pg_restore --list "$work/db.dump" >/dev/null
age -r "$AGE_RECIPIENT" -o "$work/db.dump.age" "$work/db.dump"
rm -f "$work/db.dump"

size=$(stat -c %s "$work/db.dump.age")
# A wrong or empty DATABASE_URL still dumps cleanly; a floor stops it replacing good history.
(( size >= ${MIN_DUMP_BYTES:-1} )) || { echo "dump is $size bytes, under MIN_DUMP_BYTES" >&2; exit 1; }
sha=$(sha256sum "$work/db.dump.age" | cut -d' ' -f1)
s3 s3 cp --only-show-errors --metadata "sha256=$sha" "$work/db.dump.age" "s3://$S3_BUCKET/$key"
remote=$(s3 s3api head-object --bucket "$S3_BUCKET" --key "$key" --query ContentLength --output text)
[[ $remote == "$size" ]] || { echo "uploaded $remote bytes, expected $size" >&2; exit 1; }

printf '{"time":"%s","key":"%s","size":%s,"sha256":"%s","seconds":%s}\n' \
  "$(date -u +%FT%TZ)" "$key" "$size" "$sha" "$(($(date +%s) - started))" >"$work/latest.json"
s3 s3 cp --only-show-errors "$work/latest.json" "s3://$S3_BUCKET/$PREFIX/latest.json"
echo "backup ok: $key $size bytes sha256=$sha in $(($(date +%s) - started)) s"

# Prune last, so a prune failure never hides a good backup (it still fails the run).
old=$(s3 s3api list-objects-v2 --bucket "$S3_BUCKET" --prefix "$PREFIX/db/" --query 'Contents[].Key' --output text | tr '\t' '\n' | grep -v '^None$' | prune.sh)
n=0
while IFS= read -r k; do [[ -n $k ]] && { s3 s3 rm --only-show-errors "s3://$S3_BUCKET/$k"; n=$((n + 1)); }; done <<<"$old"
echo "pruned $n old backups"

# Optional dead-man's switch: a missed ping (job never ran) is noticed by the monitor, not by us.
[[ -z ${HEARTBEAT_URL:-} ]] || curl -fsS -m 10 --retry 3 -o /dev/null "$HEARTBEAT_URL"
