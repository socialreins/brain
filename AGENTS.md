# AGENTS.md: brain

The Rust backend for SocialReins. Project overview and install docs live in
[socialreins/socialreins](https://github.com/socialreins/socialreins). The Laravel app it
replaces (`~/Code/social-media-manager`) is a read-only reference: read it, never change it.

## Run

```sh
docker compose up -d postgres
export DATABASE_URL=postgres://brain:brain@localhost:5432/socialreins_brain
export MAIL_LOG=true   # sign-in links go to the log
export ENCRYPTION_KEY=$(openssl rand -base64 32)
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p socialreins-brain
```

## Layout

- `app/`: Axum shell. `controllers/` parse and respond, `models/` hold SeaORM entities,
  `config.rs` reads every env var (defaults live there and only there), `mail.rs` sends mail.
- `app/migration/`: one file per migration, raw SQL, registered in `lib.rs`. Applied on boot.
- `crates/*`: framework-free domain code. Never depend on `axum` or `sea-orm` there.
- `/v1` routes carry `utoipa::path` annotations; `/openapi.json` is generated from them.
  `PATCH /auth/me` is documented there too, under its own `/` server (`auth::api`).
- `app/src/mcp/`: `/mcp`, whose tools call the same service fns as their `/v1` routes.
- `app/src/controllers/waitlist.rs`: the public waitlist. No captcha: each new or unverified
  email gets a confirmation link to the web app, which posts the token to
  `POST /v1/waitlist/verify` (never a state-changing GET: mail scanners open links). Mail goes
  out after the reply, at most hourly per email, 20 a day per IP and 500 a day overall
  (`rate_limit::daily_reserve`). `GET /v1/waitlist/count` is cached 30 seconds, and `GET /v1/admin/waitlist` is for `ADMIN_EMAILS` (`config.rs`) with a
  session cookie only.
- `app/src/controllers/admin.rs`: the admin console, `/v1/admin/*` (totals, users, teams, waitlist
  approve and delete, sign-up rules). Every handler takes the `Admin` extractor (`ADMIN_EMAILS`,
  session cookie only) and changing ones `FromWeb`; never return post bodies, tokens or
  credentials from it. Sign-up rules are read in one place, `auth::signup_rules`: the
  `signup_settings` row once an admin saved one, else `REGISTRATION_DISABLED` and
  `SIGNUP_DOMAINS`. `OWNER_EMAIL`, invited emails and approved waitlist emails
  (`waitlist_signups.approved_at`) pass in every mode. Its tests each run on their own database.
- `app/src/media.rs`: post media. Publishing downloads each URL (Drive share links rewritten)
  once into local disk (`MEDIA_DIR`) or an S3-compatible bucket (`S3_*`, `config.rs`), saved in
  `post_media`, and the publisher reads the stored bytes (LinkedIn, YouTube, X and Bluesky upload them themselves; a PDF is LinkedIn's document post only, any other platform's target fails it in `publish::send`), or (Threads, Facebook, Instagram and TikTok,
  which download media themselves) gets an unguessable `/media/` link to them (`media::link`, `controllers/media.rs`). Instagram (posts, carousel items, covers) takes JPEG images only and TikTok photos JPEG or WebP, so `publish::ingest` swaps a PNG (and for Instagram a WebP) for a JPEG (`media::to_jpeg`: quality 90, same size, upright by EXIF, RGB colour profile kept, transparency on white); one that does not decode is refused as before. A still image over a platform's limit for one (`post::IMAGE_LIMITS`: Bluesky 1 MB, X 5 MB) is shrunk the same way (`media::fit`: JPEG quality down to 70, then scaled down, a bounded number of tries); a GIF never is, nor an animated WebP or PNG (`media::animated`, which the save-time probe also reads), and `publish::media_problem` refuses what still does not fit. Conversions run two at a time in the process (`media::CONVERTING`). Every download goes through
  `PublicOnly`. Drive answers a large file with a confirmation page: `Media::open` follows its form once, to a Drive host only (`drive_page`), and tells a file that is not shared (`NotShared`) from one over its download quota (`DriveQuota`) and a page it could not get past (`DriveConfirm`). A video (up to `MEDIA_MAX_BYTES`, 1 GiB) is never held whole: `download` streams it to the store in `PART_BYTES` parts within a deadline that grows with its size (`INGEST_DEADLINE`, `MIN_SPEED`, `STALL`, `INGEST_CEILING`), `publish::load` hands it on as a `media::Blob::Stored` that connectors read by range (`Blob::range`, `Blob::body`), and `/media/{name}` streams it with `Range`. Only images, covers and PDFs (`MAX_STILL_BYTES`) are read whole. Posts of one group take turns per URL (`media::TURNS`, per process) so a video is downloaded once and copied in the store for the others (`Media::copied`); each post still owns its file. `publish::patience` gives a publish with a large video more time and `connectors::extra_polls` more processing checks. Files go once the post settles or is deleted, and the hourly sweep deletes the
  rest after 7 days; a storage error is only logged. Cleanup lists `posts/<post id>/` in the
  store, so it also catches files no row names. Local disk assumes one brain instance: run
  more than one replica only with the `S3_*` bucket.
- `app/src/publish.rs`: a post's `first_comment` (Facebook, Instagram, LinkedIn, YouTube) goes out
  once, after its target is saved as `published`: `post_targets.comment_status` is claimed
  (`posting`) before the call and never reset, so it is never retried; a failure only sets
  `comment_error`. Take down deletes the post alone, the platform removes its comments.
- `app/src/publish.rs`: a post's `cover` (one video; Instagram, Facebook, YouTube) is downloaded
  with the media and checked before any target is posted (a bad one fails the target, nothing
  goes out). Instagram gets it as `cover_url` in the publish call; Facebook and YouTube get it
  set once on the live video after `published`, and a refusal only sets `post_targets.cover_error`.
- Saving a post (`posts::checked_fully`, also the group, retry and MCP paths) runs `checked` (rules without calls, `socialreins_core::post`), then `preflight` outside any transaction: `Media::probe_all` types and sizes each media URL and the cover for `post::validate_media_facts` and `validate_cover_facts`, and a TikTok level is checked against live creator info. Only a definite violation is a `422` with `platform` and `detail`; no answer passes, and publish-time checks stay. Tests point probes at a mock with `Media::probe_rewrite`. Creates (`create_post`, `create_post_group`) run the same checks through `posts::examine`, which keeps every problem (`post::check*`; `validate*` is the first of each): a create answers the first (`first_refusal`), `?validate_only=true` answers them all as a `Validation` and rolls its insert back (`posts::report`), which stores every post (a blank `stand_in` for an invalid one) so the cap and the list are reported either way; the flag is query-only on the routes, and a body carrying `validate_only` is `422 validate_only_in_body` (`Creating`), and `on_invalid: "skip"` leaves the failed platforms out. Add a new check to `rule_problems` or `media_problems`, never beside them, so the three cannot disagree. `api_tokens::validates_only` is what lets a read-only token through and keeps the idempotency layer out.
- TikTok per-post settings (`posts.tiktok`, `connectors::tiktok::Options`): validated on save by `post::validate_tiktok` (privacy required, disclosure complete, branded content not private); `Publisher::publish_with_tiktok` hands them to `TikTok::post`, which checks the chosen level against the live creator info and keeps an interaction off when the creator disabled it. No settings (API posts): the old most-public, all-off behaviour. `GET /v1/connections/{id}/tiktok` is the editor's creator info.
- YouTube per-post settings (`posts.youtube`, `connectors::youtube::Options`): `privacy` is `private` (also the default with none), `unlisted` or `public`, checked on save by `post::validate_youtube`; `Publisher::publish_with_youtube` sends it as `privacyStatus`.
- `app/src/meta_inbox.rs` and `controllers/meta_webhook.rs`: `/webhooks/meta`. The signature of
  the raw body is checked before anything is parsed; items are stored before the `200` (unique
  on account and remote id, so redeliveries are harmless), names, links and automations run
  after it. Meta replies and DMs go out through `inbox::send` like Bluesky ones, once, never
  retried.
- `app/src/webhooks.rs`: outbound webhooks. Every `post_targets` status change goes through
  `webhooks::with_event`, so its event is queued in the same statement; README documents the
  payload and signature.

## Rules

- No em dashes in any text: code, comments, docs, commit messages, PR bodies. Use a comma, a
  colon or two sentences. CI fails on one.
- Ship through a pull request with green CI. No direct pushes to `main`.
- Do not re-run CI on an older `main` commit once a newer one is merged: Railway deploys the
  re-run commit over the newer one, and re-running CI on the newer commit does not deploy it again.
- Never force-push.
- Never commit or log secrets. `.env` is gitignored; `Config` has no `Debug` because
  `DATABASE_URL` holds a password.
- Logging is `tracing` only. Clippy denies `println!`, `eprintln!`, `dbg!`, `todo!` and
  `unimplemented!` workspace-wide.
- Every new endpoint gets a request test in `app/tests/` against a real Postgres.
- Copy patterns from the owner's other Rust API (OpsAgentDev/opsagent-brain), not its features.
