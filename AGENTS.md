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
- `app/src/billing.rs` and `controllers/billing.rs`: Cloud billing. `BILLING_ENABLED` (off by default and on
  self-host, where nothing is capped) makes a team's subscription decide what it may use (`effective`; no live
  plan means nothing). `plans` holds the limits (NULL is unlimited), `subscriptions` one row per team, written by
  the payment provider's webhook (not built yet), and `credit_ledger` prepaid credits for `x` posts and `ai`
  calls: signed amounts, balance is the sum, `grant` and `spend` take an idempotency `ref` unique per team and
  kind, and `spend` locks the team row so the balance never goes below 0. `GET /v1/team/billing` and
  `GET /v1/team/credits` are read-only for any member. With billing on, `save_account` (new accounts, under a team row lock), invitations and their acceptance, and the create or send handlers of automations, broadcasts and newsletters answer `402 plan_limit` (`controllers::billing::plan_limit`, `needs_automation`); inbox create counts custom inboxes against `plans.max_inboxes` under the team row lock (`limit: "inboxes"`); OAuth callbacks redirect with `connect_error=plan_limit`. Contact limits are not enforced yet. With billing on, an X target holds one `x` credit per post (the root and each thread part still to send) before anything is sent, under a fresh ref (`publish_target_held`), and `release_x` gives back the ones that did not go out; a team without credits for the whole thread fails the target before sending, saying how many it needs. A crash between the two keeps the held credits. `POST /v1/admin/teams/{id}/credits` grants credits by hand.
- `app/src/stripe.rs` and `controllers/stripe.rs`: Stripe for Cloud. `STRIPE_SECRET_KEY`, `STRIPE_WEBHOOK_SECRET`, `STRIPE_PRICE_SCHEDULER`, `STRIPE_PRICE_AUTOMATION` and their `_YEARLY` twins (shape-checked and distinct at start; checkout needs the key and all four prices, the webhook only its secret). `POST /v1/team/billing/checkout` and `/portal` are owner only with a recent sign-in. `POST /webhooks/stripe` is the only writer of `subscriptions`: raw body signature first, the event id in `stripe_events` in the same transaction as the change, `provider_event_at` drops older events, and the team comes only from the metadata brain put on the subscription (or the row already holding that subscription id). A price that is no plan, an unstarted (`incomplete`) subscription or an unknown team is logged and ignored, and its event id is not stored, so it can be resent from Stripe once mended. A second live subscription for a team (two checkouts open at once) is never written: it is cancelled in Stripe and logged (a person refunds any charge). `POST /v1/team/billing/credits` sells X credit packs (`stripe::PACKS`, prices `STRIPE_PRICE_X100`/`X500`) as one-time Checkouts; the paid `checkout.session.completed` grants them through `billing::grant` keyed by the session id, the team and pack only from the session metadata brain set. Comped plans: `subscriptions.provider = 'comp'`, `active`, no period; given by the comp migration to every team that existed then and by `PUT /v1/admin/teams/{id}/plan`, never over a live Stripe plan. Billing emails (`tell_owners`) go to the team owners after the event commits: trial ending (`customer.subscription.trial_will_end`, not for a trial already set to cancel), the first `invoice.payment_failed` of an invoice (`attempt_count` 1), and a live plan ending. Stripe sends the receipts.
- `app/src/controllers/ai_key.rs`: BYOK. `team_ai_keys` holds one sealed provider key per team (`anthropic` or
  `openai`, plus `last4`); `/v1/team/ai-key` is owner and admin only and never answers the key. `key_of` gives
  the clear key to the AI steps, which use it before any credit is spent.
- `app/src/ai.rs`: AI steps in automations (`ai_send_dm`, `ai_reply_publicly`, as actions or flow steps;
  `automations::ai_send`). The team's key first (free, unmetered); else `ANTHROPIC_API_KEY` (ours), which
  with billing on is paid by `billing::spend_ai` under the ref `ai:{run}:{step or kind}`: the plan's
  `plans.ai_monthly` allowance per calendar month (UTC, `ai_plan` ledger rows) first, then 1 `ai` credit,
  given back when nothing usable came back; a ref given back is never paid or written again. The stranger's
  name and message go to the model capped, escaped and fenced in `<incoming_message>`. A reply that repeats
  12 words of the prompt in a row (`ai_leak`) is never sent. Models live only in `ai.rs` (`ANTHROPIC_MODEL`, `OPENAI_MODEL`); `anthropic_url` and
  `openai_url` are overridable for tests. A reply that is empty, too long for the network (`inbox::fits_on`)
  or holds a link, domain, email or @handle that is not in the owner's prompt (exact normalised hosts, after
  NFKC, zero-width characters and look-alike or spelled-out dots) is never sent. Keys and prompts are never logged.
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
  admins. Mail only confirmed rows without `unsubscribed_at`. `POST /v1/public/newsletter/unsubscribe`
  sets it (token in the body or, one-click, in the query; same `200` for any token; never a GET) and only
  confirming a link mailed after it clears it.
- `app/src/newsletters.rs` and `controllers/newsletters.rs`: newsletters, one email to every confirmed,
  not unsubscribed subscriber (owners and admins only, session only). It goes out from our own mail
  domain, so everything errs on the side of not sending. `checked` and `render` are the whole markup
  (paragraphs, `**bold**`, bare `https://` links; everything escaped, never pass HTML through), `compose`
  the mail with its legal footer, `mail` the one place a newsletter mail is sent (test included), with
  the `List-Unsubscribe` headers (`Mailer::send_list`). `start` needs the team's sender
  (`teams.newsletter_sender_*`, copied onto the newsletter; the optional `logo` is a public `https` link, checked in `teams::logo`, never fetched and only ever the escaped `src` of the image above the mail), a test since the last edit and
  `confirm_total` equal to the snapshot's row count (one a team per 24 hours, 5000 recipients).
  `newsletters::tick` (every minute, own advisory lock held for the claim only) moves up to 50
  recipients a newsletter `pending` to `sending` with their hit of the cap over all teams
  (`MAILS_PER_DAY`, `claim`, `SKIP LOCKED`), commits, then `attempt` reads the subscriber again
  (`addressee`, which also makes their unsubscribe token: its hash for lookups, a sealed copy so each
  mail carries the same link) and sends. Any mailer error (`Tried::Trip`) or 5 failed sends in a row
  set it `paused`; only `resume` (a person) puts it back. Never mail a recipient without claiming it,
  and never retry one. `attempt` commits `mailing` right before the mail call: after 15 minutes
  `recover` fails a `mailing` row as `interrupted` and hands a merely claimed (`sending`) one back
  (`HAND_BACK`: `pending`, or `skipped` when the newsletter was cancelled, like `release`).
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
  `PublicOnly`. An upload (`controllers/uploads.rs`, `POST /v1/uploads`) is already stored: a post names it by its `/media/` link and publishing copies it in the store (`Media::upload_key`, which matches the link by its decoded path, never its host). Its row is written `pending` with `MEDIA_MAX_BYTES` reserved before the file streams (3 at once per team), and `media::release_uploads` deletes a row under `FOR UPDATE` after a re-check, while a post save holds its uploads `FOR SHARE` (`uploads::foreign`); `release_orphans` deletes stored uploads with no row. Drive answers a large file with a confirmation page: `Media::open` follows its form once, to a Drive host only (`drive_page`), and tells a file that is not shared (`NotShared`) from one over its download quota (`DriveQuota`) and a page it could not get past (`DriveConfirm`). A video (up to `MEDIA_MAX_BYTES`, 1 GiB) is never held whole: `download` streams it to the store in `PART_BYTES` parts within a deadline that grows with its size (`INGEST_DEADLINE`, `MIN_SPEED`, `STALL`, `INGEST_CEILING`), `publish::load` hands it on as a `media::Blob::Stored` that connectors read by range (`Blob::range`, `Blob::body`), and `/media/{name}` streams it with `Range`. Only images, covers and PDFs (`MAX_STILL_BYTES`) are read whole. Posts of one group take turns per URL (`media::TURNS`, per process) so a video is downloaded once and copied in the store for the others (`Media::copied`); each post still owns its file. `publish::patience` gives a publish with a large video more time and `connectors::extra_polls` more processing checks. Files go once the post settles or is deleted, and the hourly sweep deletes the
  rest after 7 days; a storage error is only logged. Cleanup lists `posts/<post id>/` in the
  store, so it also catches files no row names. Local disk assumes one brain instance: run
  more than one replica only with the `S3_*` bucket.
- `app/src/publish.rs`: the scheduler's round is `publish::begin`: under the advisory lock it claims due posts, starts each as its own task and lets the lock go without waiting (`tick` waits, for tests), so a large video never delays posts due after it. `IN_FLIGHT` keeps a round from taking a post the process still runs, `POST_SLOTS` and `TARGET_SLOTS` bound what runs at once, and `STUCK_MINUTES` is derived from `media::INGEST_CEILING` and `PUBLISH_LIMIT`. On SIGTERM (`bin/main.rs`) `publish::stop` ends claiming and hands posts not yet started back to `scheduled` (`release`), and `publish::drain` waits up to `DRAIN` (120 seconds, a slow Threads post can take a minute or two) for the ones publishing; Railway kills at once unless `RAILWAY_DEPLOYMENT_DRAINING_SECONDS` is set above it (180 on SocialReins).
- `app/src/publish.rs`: a scheduled post more than an hour past its time is never published:
  each tick puts it back to `draft` with the reason in `error` (`draft_late`), and its repeat
  copy is still made; a post left `queued` is drafted only if it was never fanned out into targets (a retried one keeps its old `scheduled_at` and is picked up as left behind instead); a `rejected` post is left alone, so its series ends, and so is a post the
  process is still publishing. `draft_late` locks the posts before it reads their targets and
  `claim_target` takes a shared lock on a still `queued` post, so a post never turns draft while
  a target starts. A claim takes the posts of one `group_id` whole or not at all, so they are
  all judged by the same clock. A target retries once by itself, 5 minutes on (`post_targets.retry_at`,
  `retried`), only after an error a connector marked `connectors::UNSENT` (a rate limit, a
  connection that never opened) and never once its remote id is saved.
- `app/src/publish.rs`: on start, `release_unstarted` hands posts claimed and never started back to
  `scheduled`. A team publishes at most `TEAM_AT_ONCE` (3) posts and 3 targets at once in a
  process, taken before the shared slots, and a round claims a team no more posts than it has
  places left (counted from `IN_FLIGHT`, whole groups at a time), so a team's backlog stays
  `scheduled` and never fills the batch or sits `queued` long enough to look stuck (so another
  instance's `draft_late` leaves a claimed post alone); each post that ends claims its team's
  next at once (`refill`), so throughput is the places and the time a post takes, not one claim a
  tick. The
  cap is per process: with several instances a team gets 3 on each (a shortcut, noted in the
  README). Deleting a set fails its scheduled posts
  (`SET_DELETED`), and one already claimed the same way, with no mail either: the user did it; the 30-day purge deletes the set with all its posts (cascade). Repeats step
  in the set's queue time zone (`repeat.rs`; a copy a clock change moved, 2:30 to 3:30, keeps the
  time it was meant for in the rule's hidden `time`, so the series returns to 2:30), and a copy that cannot be made is mailed about.
- `app/src/post_mail.rs`: a post that settles as failed or partly failed, or goes back to draft
  for being late, queues a `post_notices` row in the same transaction; each publish round mails
  the queued ones in the background to the post's author and the team's owners, once, at most
  10 a team a day (`rate_limit::daily_reserve`).
- `app/src/publish.rs`: a post's `first_comment` (Facebook, Instagram, LinkedIn, YouTube) goes out
  once, after its target is saved as `published`: `post_targets.comment_status` is claimed
  (`posting`) before the call and never reset, so it is never retried; a failure only sets
  `comment_error`. Take down deletes the post alone, the platform removes its comments.
- `app/src/publish.rs`: image alt text (`posts.media_alt`, parallel to `media`; a thread part's in its
  `media_alt`; `post::check_alt` and `post::normal_alt` in core) goes to Bluesky, X, Threads, Instagram and
  LinkedIn through `Attachment::alt`. A step that fails never fails the post: connectors call
  `connectors::alt_note`, which `publish::send` collects into `post_targets.alt_error`. Only calls that
  cannot have posted are retried without the text (Threads and Instagram containers, a LinkedIn `400`/`422`).
  Facebook is left out on purpose (its photo alt text parameter is unconfirmed).
- `app/src/publish.rs`: a post's `cover` (one video; Instagram, Facebook, YouTube) is downloaded
  with the media and checked before any target is posted (a bad one fails the target, nothing
  goes out). Instagram gets it as `cover_url` in the publish call; Facebook and YouTube get it
  set once on the live video after `published`, and a refusal only sets `post_targets.cover_error`.
- Saving a post (`posts::checked_fully`, also the group, retry and MCP paths) runs `checked` (rules without calls, `socialreins_core::post`), then `preflight` outside any transaction: `Media::probe_all` types and sizes each media URL and the cover for `post::validate_media_facts` and `validate_cover_facts`, a TikTok level is checked against live creator info, and an X post over 280 weighted characters against the set's X accounts (`x_short`: each needs `x::x_long_posts`, the user's `meta.plan` (`free` or `paid`, set with `PUT /v1/connections/{id}/plan`, kept across reconnect and renewal) when set, else `meta.long_posts`, a paid X plan learned at connect or at the next token renewal; `post::check` itself holds X to 25,000). Only a definite violation is a `422` with `platform` and `detail`; no answer passes, and publish-time checks stay. Tests point probes at a mock with `Media::probe_rewrite`. Creates (`create_post`, `create_post_group`) run the same checks through `posts::examine`, which keeps every problem (`post::check*`; `validate*` is the first of each): a create answers the first (`first_refusal`), `?validate_only=true` answers them all as a `Validation` and rolls its insert back (`posts::report`), which stores every post (a blank `stand_in` for an invalid one) so the cap and the list are reported either way; the flag is query-only on the routes, and a body carrying `validate_only` is `422 validate_only_in_body` (`Creating`), and `on_invalid: "skip"` leaves the failed platforms out. Add a new check to `rule_problems` or `media_problems`, never beside them, so the three cannot disagree. `api_tokens::validates_only` is what lets a read-only token through and keeps the idempotency layer out.
- TikTok per-post settings (`posts.tiktok`, `connectors::tiktok::Options`): validated on save by `post::validate_tiktok` (privacy required, disclosure complete, branded content not private); `Publisher::publish_with_tiktok` hands them to `TikTok::post`, which checks the chosen level against the live creator info and keeps an interaction off when the creator disabled it. No settings (API posts): the old most-public, all-off behaviour. `GET /v1/connections/{id}/tiktok` is the editor's creator info.
- YouTube per-post settings (`posts.youtube`, `connectors::youtube::Options`): `privacy` is `private` (also the default with none), `unlisted` or `public`, checked on save by `post::validate_youtube`; `Publisher::publish_with_youtube` sends it as `privacyStatus`.
- `app/src/inbox.rs` (`store_new`): the Bluesky tick gives each account `Bluesky::inbox_budget` (20 s,
  notifications get half) and stops at the next page or conversation after that, or after the
  page, item and conversation caps. What is left is saved on `social_accounts`
  (`notifications_cursor` with `notifications_pending_at`, `messages_resume_at`) and read next tick:
  `notifications_seen_at` and `messages_seen_at` move only when a round reaches its end, and a
  tick always reads at least one page and one conversation. Items store once (unique on account
  and remote id), so a reread adds nothing; a refused saved cursor starts the read over. Trade-off:
  while a long backlog is read back, items newer than the round's first page wait until it ends.
  Only the 50 latest conversations are listed, so older ones with news past that are never seen.
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
  claiming it, and never retry one. `attempt` commits `delivering` right before `inbox::send`:
  after 15 minutes `recover` fails a `delivering` row as `interrupted` and hands a merely claimed
  (`sending`) one back, as `release` does when the round ran out of time before the send began
  (`HAND_BACK`: `pending`, or `skipped` when the broadcast was cancelled).
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
- `app/src/dry_run.rs`: `POST /v1/automations/test`, "Test this automation". It runs the real matching (`automations::mismatches`), `network_limits`, `{name}` and `{page}` filling and the AI step (`automations::ai_write`, shared with the real send, so the key choice, the monthly allowance and the checks are the same) on a sample from a stranger, and sends nothing, stores no inbox item or run and takes no daily hit. Keep it in step with `automations::run_matching` and `act` when a rule changes.
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
