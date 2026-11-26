# Database backups

A small job that copies the brain Postgres database off the platform: `pg_dump` (custom format), encrypted with [age](https://age-encryption.org) to a public key, uploaded to an S3-compatible bucket (Cloudflare R2, MinIO, S3). The server never holds the private key, so a leaked bucket or token exposes nothing readable.

Run it as a cron job every 6 hours (`0 */6 * * *`). Each run:

1. dumps the database (`pg_dump --format=custom`) and checks the archive reads back (`pg_restore --list`),
2. encrypts it, deletes the plaintext, uploads `backups/db/socialreins-<UTC time>.dump.age` with its sha256 stored as object metadata, and checks the stored size,
3. writes `backups/latest.json` (`time`, `key`, `size`, `sha256`, `seconds`),
4. prunes: every dump for 2 days, then the first of each UTC day until 14 days, then the first of each ISO week until 30 days, then nothing (`prune.sh`, tested by `prune_test.sh`). Only keys that match the dump name pattern under `backups/db/` are ever deleted.
5. pings `HEARTBEAT_URL` if set.

Any failure exits non-zero. The recovery point is therefore at most 6 hours, plus the length of one run (a minute or less at 250 MB).

## Variables

| Variable | Meaning |
|---|---|
| `DATABASE_URL` | Postgres to dump. On Railway reference the database service: `${{Postgres.DATABASE_URL}}` (private network, nothing copied). |
| `AGE_RECIPIENT` | The age **public** key (`age1...`). Not a secret. |
| `S3_ENDPOINT`, `S3_BUCKET`, `S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY`, `S3_REGION` | The bucket. The same names brain uses for media (`S3_REGION` is `auto` on R2). |
| `BACKUP_PREFIX` | Optional, default `backups`. Brain's media cleanup only reads `posts/`, so keep this off that prefix. |
| `HEARTBEAT_URL` | Optional. Pinged after a good run (a [healthchecks.io](https://healthchecks.io) free check, UptimeRobot heartbeat, ...). A monitor that alerts when the ping is missing also catches a job that stopped running. |
| `DUMP_TIMEOUT` | Optional seconds, default 1800. |

## Keys

```sh
age-keygen -o backup-age.key        # prints the public key; keep the file OFF the server
```

Put the `age1...` line into `AGE_RECIPIENT` and the file in a password manager and one offline copy. Without it no backup can be read. To rotate: make a new key, set the new `AGE_RECIPIENT`, and keep the old private key until the last dump encrypted to it has been pruned (30 days). Dumps are not re-encrypted.

## Run on Railway

New service from this repository, **Root Directory** `ops/backup`, builder Dockerfile, **Cron Schedule** `0 */6 * * *` (UTC), restart policy Never, no public domain. A cron service starts the container, runs it once and stops; a run still going at the next tick is skipped. Railway marks a non-zero exit as crashed: turn on deploy-crash notifications for the project (Settings, Notifications, or a webhook) so you are told.

## Run anywhere else

```sh
docker build -t backup ops/backup
docker run --rm --env-file backup.env backup      # cron it every 6 hours
```

The dump is written to the container's temp dir, so it needs room for the compressed database.

## Restore

```sh
aws --endpoint-url "$S3_ENDPOINT" s3 cp s3://BUCKET/backups/db/socialreins-STAMP.dump.age .
sha256sum socialreins-STAMP.dump.age                       # matches backups/latest.json, or the object's sha256 metadata
age -d -i backup-age.key socialreins-STAMP.dump.age > db.dump
createdb socialreins_restore
pg_restore --no-owner --no-acl --exit-on-error --jobs=4 --dbname socialreins_restore db.dump
```

Then follow the restore runbook (reconcile due posts before a brain starts on it) and delete `db.dump`. Restore with a `pg_restore` of the server's major version or newer.

## Where to look when it fails

- Railway, the backup service, Deployments: the last run's log ends with `backup ok: ...` or the failing command.
- `backups/latest.json` in the bucket: its `time` is the last good run. Older than 7 hours means the job is not working.
