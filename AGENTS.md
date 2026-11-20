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
- `app/src/controllers/pages.rs`: public pages of posts. `teams.public_pages` (`PATCH /v1/team`,
  owner or admin) switches them on; `post_pages` holds one row per `subject` (the post's
  `group_id`, else its `id`), so a group has one page under one permanent `code`.
  `GET /v1/public/pages/{code}` takes no sign-in (`page_limiter`, per IP) and is a `404` until the
  page is enabled, the team has pages on and a post of the subject is `published`; never return
  ids, errors, first comments or unpublished versions from it.
- `app/src/controllers/subscribers.rs`: newsletter sign-ups from public pages. `teams.newsletter`
  (`PATCH /v1/team`, owner or admin) puts an email box on live pages; `POST /v1/public/pages/{code}/subscribe`
  stores `subscribers (team_id, email)` unconfirmed and mails a link (once an hour per email, 20 a day per IP,
  200 per team, 5000 overall) to the web app, which posts its token to `POST /v1/public/newsletter/verify`.
  Same reply for a known email. `GET /v1/subscribers` and `DELETE /v1/subscribers/{id}` are for owners and
  admins. Mail only confirmed rows; nothing but the confirmation is sent yet.
- `app/src/controllers/admin.rs`: the admin console, `/v1/admin/*` (totals, users, teams, waitlist
  approve and delete, sign-up rules). Every handler takes the `Admin` extractor (`ADMIN_EMAILS`,
  session cookie only) and changing ones `FromWeb`; never return post bodies, tokens or
  credentials from it. Sign-up rules are read in one place, `auth::signup_rules`: the
  `signup_settings` row once an admin saved one, else `REGISTRATION_DISABLED` and
  `SIGNUP_DOMAINS`. `OWNER_EMAIL`, invited emails and approved waitlist emails
  (`waitlist_signups.approved_at`) pass in every mode. Its tests each run on their own database.
- `app/src/media.rs`: post media. Publishing downloads each URL (Drive and Dropbox share links rewritten)
  once into local disk (`MEDIA_DIR`) or an S3-compatible bucket (`S3_*`, `config.rs`), saved in
  `post_media`, and the publisher reads the stored bytes (LinkedIn, YouTube, X and Bluesky upload them themselves; a PDF is LinkedIn's document post only, any other platform's target fails it in `publish::send`), or (Threads, Facebook, Instagram and TikTok,
  which download media themselves) gets an unguessable `/media/` link to them (`media::link`, `controllers/media.rs`). Instagram (posts, carousel items, covers) takes JPEG images only and TikTok photos JPEG or WebP, so `publish::ingest` swaps a PNG (and for Instagram a WebP) for a JPEG (`media::to_jpeg`: quality 90, same size, upright by EXIF, RGB colour profile kept, transparency on white); one that does not decode is refused as before. A still image over a platform's limit for one (`post::IMAGE_LIMITS`: Bluesky 1 MB, X 5 MB) is shrunk the same way (`media::fit`: JPEG quality down to 70, then scaled down, a bounded number of tries); a GIF never is, nor an animated WebP or PNG (`media::animated`, which the save-time probe also reads), and `publish::media_problem` refuses what still does not fit. Conversions run two at a time in the process (`media::CONVERTING`). Every download goes through
  `PublicOnly`. Drive answers a large file with a confirmation page: `Media::open` follows its form once, to a Drive host only (`drive_page`), and tells a file that is not shared (`NotShared`) from one over its download quota (`DriveQuota`) and a page it could not get past (`DriveConfirm`). A video (up to `MEDIA_MAX_BYTES`, 1 GiB) is never held whole: `download` streams it to the store in `PART_BYTES` parts within a deadline that grows with its size (`INGEST_DEADLINE`, `MIN_SPEED`, `STALL`, `INGEST_CEILING`), `publish::load` hands it on as a `media::Blob::Stored` that connectors read by range (`Blob::range`, `Blob::body`), and `/media/{name}` streams it with `Range`. Only images, covers and PDFs (`MAX_STILL_BYTES`) are read whole. Posts of one group take turns per URL (`media::TURNS`, per process) so a video is downloaded once and copied in the store for the others (`Media::copied`); each post still owns its file. `publish::patience` gives a publish with a large video more time and `connectors::extra_polls` more processing checks. Files go once the post settles or is deleted, and the hourly sweep deletes the
  rest after 7 days; a storage error is only logged. Cleanup lists `posts/<post id>/` in the
  store, so it also catches files no row names. Local disk assumes one brain instance: run
  more than one replica only with the `S3_*` bucket.
- `app/src/publish.rs`: the scheduler's round is `publish::begin`: under the advisory lock it claims due posts, starts each as its own task and lets the lock go without waiting (`tick` waits, for tests), so a large video never delays posts due after it. `IN_FLIGHT` keeps a round from taking a post the process still runs, `POST_SLOTS` and `TARGET_SLOTS` bound what runs at once, and `STUCK_MINUTES` is derived from `media::INGEST_CEILING` and `PUBLISH_LIMIT`. On SIGTERM (`bin/main.rs`) `publish::stop` ends claiming and hands posts not yet started back to `scheduled` (`release`), and `publish::drain` waits up to `DRAIN` (25 seconds) for the ones publishing; Railway kills at once unless `RAILWAY_DEPLOYMENT_DRAINING_SECONDS` is set (30 or more).
- `app/src/publish.rs`: a post's `first_comment` (Facebook, Instagram, LinkedIn, YouTube) goes out
  once, after its target is saved as `published`: `post_targets.comment_status` is claimed
  (`posting`) before the call and never reset, so it is never retried; a failure only sets
  `comment_error`. Take down deletes the post alone, the platform removes its comments.
- `app/src/publish.rs`: a post's `cover` (one video; Instagram, Facebook, YouTube) is downloaded
  with the media and checked before any target is posted (a bad one fails the target, nothing
  goes out). Instagram gets it as `cover_url` in the publish call; Facebook and YouTube get it
  set once on the live video after `published`, and a refusal only sets `post_targets.cover_error`.
- Saving a post (`posts::checked_fully`, also the group, retry and MCP paths) runs `checked` (rules without calls, `socialreins_core::post`), then `preflight` outside any transaction: `Media::probe_all` types and sizes each media URL and the cover for `post::validate_media_facts` and `validate_cover_facts`, a TikTok level is checked against live creator info, and an X post over 280 weighted characters against the set's X accounts (`x_short`: each needs `x::x_long_posts`, the user's `meta.plan` (`free` or `paid`, set with `PUT /v1/connections/{id}/plan`, kept across reconnect and renewal) when set, else `meta.long_posts`, a paid X plan learned at connect or at the next token renewal; `post::check` itself holds X to 25,000). Only a definite violation is a `422` with `platform` and `detail`; no answer passes, and publish-time checks stay. Tests point probes at a mock with `Media::probe_rewrite`. Creates (`create_post`, `create_post_group`) run the same checks through `posts::examine`, which keeps every problem (`post::check*`; `validate*` is the first of each): a create answers the first (`first_refusal`), `?validate_only=true` answers them all as a `Validation` and rolls its insert back (`posts::report`), which stores every post (a blank `stand_in` for an invalid one) so the cap and the list are reported either way; the flag is query-only on the routes, and a body carrying `validate_only` is `422 validate_only_in_body` (`Creating`), and `on_invalid: "skip"` leaves the failed platforms out. Add a new check to `rule_problems` or `media_problems`, never beside them, so the three cannot disagree. `api_tokens::validates_only` is what lets a read-only token through and keeps the idempotency layer out.
- TikTok per-post settings (`posts.tiktok`, `connectors::tiktok::Options`): validated on save by `post::validate_tiktok` (privacy required, disclosure complete, branded content not private); `Publisher::publish_with_tiktok` hands them to `TikTok::post`, which checks the chosen level against the live creator info and keeps an interaction off when the creator disabled it. No settings (API posts): the old most-public, all-off behaviour. `GET /v1/connections/{id}/tiktok` is the editor's creator info.
- YouTube per-post settings (`posts.youtube`, `connectors::youtube::Options`): `privacy` is `private` (also the default with none), `unlisted` or `public`, checked on save by `post::validate_youtube`; `Publisher::publish_with_youtube` sends it as `privacyStatus`.
- `app/src/meta_inbox.rs` and `controllers/meta_webhook.rs`: `/webhooks/meta`. The signature of
  the raw body is checked before anything is parsed; items are stored before the `200` (unique
  on account and remote id, so redeliveries are harmless), names, links and automations run
  after it. Meta replies and DMs go out through `inbox::send` like Bluesky ones, once, never
  retried.
- `app/src/automations.rs`: an automation holds `actions` or `steps` (a flow of sends and
  waits). A flow's run claims each step before sending it (`claim_step`, with the send's hit of
  the daily cap), a `wait` parks it as `waiting`, and `automations::tick` (every minute, own
  advisory lock) takes due runs up through `resume_due`, whose one statement moves them back to
  `running`. Never send a step without claiming it, and never retry one. Every claim and park
  checks `UNCHANGED` (the flow is on and has the steps the run started on, `steps_hash`). A quick reply click is
  a `dm` item with `button_payload` (`trigger_of`: `button_clicked`, never `dm_received`).
- `app/src/controllers/queue.rs`: posting queues. `queue_schedules` holds one row per set (an IANA
  `time_zone`, `slots` as sorted minutes of the week, weekday 0 Sunday). `queue: true` on a create
  takes `queue::pick` inside the insert's transaction, under a per-set advisory lock held until
  commit; a slot is free when no `scheduled` post of the set is at that exact time.
- `app/src/controllers/library.rs`: the media library (`media_library`, URLs only, unique per team,
  at most 1000) and snippets (`snippets`, `caption`, `hashtags` or `reply`, a saved inbox reply; name unique per team and kind
  ignoring case, at most 500). Team assets: every member and token writes them, set tokens too,
  read-only tokens never; creates take a per-team advisory lock so the caps hold.
- `app/src/controllers/contacts.rs`: contacts (`contacts`, one per team, platform and author id;
  `contact_tags`, at most 20 a contact). `inbox::store` is the only place an item is stored and
  upserts the author's contact in the same statement, so never insert into `inbox_items` any
  other way; the contact keeps the profile of its newest item. Tags are written under the
  contact's row lock (`inbox::add_tag`, `PATCH /v1/contacts/{id}`), checked by `inbox::tag`.
  `DELETE /v1/contacts/{id}` (owner, admin) and a disconnect (contacts left with no items)
  remove a person's data; items outlive a deleted contact with `contact_id` null.
  Session only, like the inbox. `inbox_items.assigned_to` is set under a share lock of the
  member's `team_user` row, and `teams::change_member` clears it after deleting that row.
- `app/src/broadcasts.rs` and `controllers/broadcasts.rs`: broadcasts, one DM to every reachable
  contact with a tag (owners and admins write, session only). `ROUTE` is the one place that says
  who is reachable and through which item: never opted out (`contact_opt_outs`, a row per team,
  platform and person that outlives the contact, written by a DM that says `stop` or
  `unsubscribe` in `inbox::store` and by `PATCH /v1/contacts/{id}`), no broadcast in the last 24
  hours (`contacts.last_broadcast_at`, set before the send), an account without
  `meta.reconnect`, and on Facebook and Instagram a DM under 24 hours old (never a message tag). `start` snapshots the
  audience and checks `confirm_total` against that same statement's row count, under a per-team
  advisory lock (one `sending` a team, 3 starts a day). `broadcasts::tick` (every minute, own
  advisory lock held for the claim only) moves up to 20 recipients a broadcast `pending` to
  `sending` with their hit of the daily cap (`claim`, `SKIP LOCKED`), commits, then `attempt`
  checks the same audience clause again (tag included) and sends through `inbox::send`. A
  platform error that is not about the one person (`Tried::Trip`), or 5 failed sends in a row
  (`pause_failing`), sets the broadcast `paused`; only `resume` (a person) puts it back. Never send to a recipient without
  claiming it, and never retry one: `recover` fails a claim older than 15 minutes as
  `interrupted`. Only `release` (the round ran out of time before the send began) puts one back.
- `app/src/metrics.rs` and `controllers/analytics.rs`: post metrics (`post_metrics`, one latest
  row per published target, no history) from Bluesky, Facebook, Instagram and YouTube
  (`metrics::SUPPORTED`; never add a platform that needs a new scope). `metrics::tick` (every 15
  minutes, own advisory lock held only while it reads what is due) and
  `POST /v1/posts/{id}/metrics/refresh` both go through `claim`, one statement that marks a batch
  as tried, so a target is never read twice within a minute and no connection is held while a
  platform is asked. `fetched_at` is the last try; a failed one keeps the numbers, a missing post
  is `gone` (only once the target is an hour old), asked for again a week later, 4 times. A
  reconnect (`connections::save_account`) clears every error of the account's targets
  (`metrics::reset`). Graph v26 refuses `/?ids=`, so `meta_counts` asks one id a call (and a Reel
  once more with a video node's fields); one post's error is its own.
  `GET /v1/analytics` sums in Postgres (`analytics::SCOPED`) and loads only the top posts.
- `app/src/feeds.rs` and `controllers/feeds.rs`: RSS and Atom feeds that post on their own
  (`feeds`, `feed_items`, at most 20 per team; owners, admins and full tokens write). `Fetcher`
  is the only way a feed is read: `public_url`, `PublicOnly`, at most 3 redirects each checked
  again, 10 seconds, 1 MB; its `test_origin` (tests only, never from the environment) lets the
  tests' one loopback server through and nothing else. `feeds::tick` (every 15 minutes, own
  advisory lock) claims each due feed (`claim`, one statement, also the once-a-minute limit of
  `POST /feeds/{id}/check`) and `check` posts its new items, 5 a check and oldest first, through
  `posts::create_group_guarded`: the guard marks the item seen in the group's transaction, so
  an item never posts twice. A refused item is retried 3 times (`feed_items.failed`), 10 failed
  reads in a row switch the feed off. A round runs 8 checks at once within `ROUND` (5 minutes);
  parsing runs in `spawn_blocking` (`parsed`), so a parser panic is a failed check; an item's
  link is kept only as an absolute http(s) URL (`web`). Never read a feed inside a transaction.
- Post approvals (`teams.require_approval`, `posts.approval*`): `posts::approval_for` is the one
  place a save decides the value (member `pending`, owner, admin or token `approved`, draft or
  switch off null), `publish::claim` never takes a `rejected` post and skips `pending` only while the team's
  switch is on (so switching it off releases those), and `posts::decide` is the approve and reject of a
  post or a group in one statement. A member's publish now is `403 approval_required` unless
  the post is `approved`, anyone's is for a `rejected` post, and a manager's approves a
  `pending` one in the claim.
- `app/src/repeat.rs`: repeating posts (`posts.repeat`, set by `posts::set_repeat` on the post and
  the scheduled posts of its group). `repeat::spawn` makes the next copy and clears the rule; call
  it in the transaction of anything that takes a post out of `scheduled` to publish it
  (`publish::claim`, and publish now through `publish::reserve_and`), so a post yields one copy
  at most. A copy's group id is a UUID v5 of the old one, so group mates agree without talking.
- Team slugs (`teams.slug`, optional, set with `PATCH /v1/team`): `teams::team_by_code` is the
  one place a request's `X-Team` or `?team=` becomes a team, by `code` or by slug. A slug never
  equals a code (`rename` checks, `insert_team` draws again) and is unique on `lower(slug)`; the
  code is permanent, the slug may change (5 times a day per team). One given up stops resolving at once and is
  kept from other teams for 30 days (`team_slug_holds`, written in the same transaction).
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
