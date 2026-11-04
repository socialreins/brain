# AGENTS.md: brain

The Rust backend for SocialReins. Project overview and install docs live in
[socialreins/web](https://github.com/socialreins/web). The Laravel app it
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
- `app/src/media.rs`: post media. Publishing downloads each URL (Drive share links rewritten)
  once into local disk (`MEDIA_DIR`) or an S3-compatible bucket (`S3_*`, `config.rs`), saved in
  `post_media`, and the publisher reads the stored bytes (LinkedIn and YouTube upload them themselves), or (Threads, Facebook and Instagram,
  which download media themselves) gets an unguessable `/media/` link to them (`media::link`, `controllers/media.rs`). Every download goes through
  `PublicOnly`. Files go once the post settles or is deleted, and the hourly sweep deletes the
  rest after 7 days; a storage error is only logged. Cleanup lists `posts/<post id>/` in the
  store, so it also catches files no row names. Local disk assumes one brain instance: run
  more than one replica only with the `S3_*` bucket.
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
- Never force-push.
- Never commit or log secrets. `.env` is gitignored; `Config` has no `Debug` because
  `DATABASE_URL` holds a password.
- Logging is `tracing` only. Clippy denies `println!`, `eprintln!`, `dbg!`, `todo!` and
  `unimplemented!` workspace-wide.
- Every new endpoint gets a request test in `app/tests/` against a real Postgres.
- Copy patterns from the owner's other Rust API (OpsAgentDev/opsagent-brain), not its features.
