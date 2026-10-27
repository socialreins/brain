<p align="center"><img src="https://raw.githubusercontent.com/socialreins/.github/main/brand/logo.svg" width="96" height="96" alt="SocialReins logo"></p>

<h1 align="center">brain</h1>

The Rust backend for [SocialReins](https://socialreins.com), a social media manager for
scheduling and publishing posts across platforms. For the project overview, roadmap and the
other repos, see the main repo [socialreins/web](https://github.com/socialreins/web).

Right now brain serves the waitlist behind the socialreins.com landing page and magic link
sign-in. Publishing, teams and the MCP server come later.

## Stack

Rust, [Axum](https://github.com/tokio-rs/axum) 0.8, [SeaORM](https://www.sea-ql.org/SeaORM/) 2
on Postgres, OpenAPI docs generated with [utoipa](https://github.com/juhaku/utoipa).

```
app/             HTTP app: config, controllers, models, the binary
app/migration/   SeaORM migrations, applied on every boot
crates/core/     framework-free domain rules (email checks, tokens)
```

## Endpoints

| Method | Path | What it does |
|---|---|---|
| GET | `/health` | `{"status":"ok"}` |
| GET | `/version` | `{"commit":"<sha>"}`, from `RAILWAY_GIT_COMMIT_SHA`, `dev` locally |
| GET | `/openapi.json` | OpenAPI document for `/v1` |
| POST | `/v1/waitlist` | `{"email":"...","source":"..."}` joins the waitlist |
| POST | `/auth/link` | `{"email":"..."}` emails a sign-in link, `202 {"status":"sent"}` |
| GET | `/auth/magic/{token}` | Redeems a link, sets the session cookie, redirects to `WEB_URL` |
| GET | `/auth/me` | The signed-in user, or `401 {"error":"unauthorized"}` |
| POST | `/auth/logout` | Deletes the session, `204` |

`POST /v1/waitlist` answers `200 {"status":"ok"}` for a new email and for one already on the
list, so it never reveals who signed up. A bad email gets `422 {"error":"invalid_email"}`. Each
client IP (IPv6 by /64) gets 10 requests a minute, then `429`.

Sign-in is by magic link only. `POST /auth/link` answers the same `202` whether or not the
email has an account (or is refused by `REGISTRATION_DISABLED`). The link works once, for 15
minutes; opening it creates the account if needed, then a 30-day session in the HttpOnly,
SameSite=Lax `sr_session` cookie (`Secure` unless `WEB_URL` is `http://`). A dead link
redirects to `WEB_URL/login?error=invalid_link` (or `registration_disabled`). Only SHA-256
hashes of link and session tokens are stored. Both link routes share 5 requests a minute per IP.

## Local development

You need Rust (stable) and Docker.

```sh
docker compose up -d postgres
export DATABASE_URL=postgres://brain:brain@localhost:5432/socialreins_brain
cargo run -p socialreins-brain        # http://localhost:8080
cargo test --workspace                # uses the same DATABASE_URL
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings
```

Port 5432 taken? Run `PG_PORT=55433 docker compose up -d postgres` and use that port in
`DATABASE_URL`.

## Environment

| Variable | Required | Default |
|---|---|---|
| `DATABASE_URL` | yes | |
| `PORT` | no | `8080` |
| `CORS_ORIGINS` | no | `https://socialreins.com,https://www.socialreins.com,http://localhost:3000` |
| `TRUST_PROXY` | no | `false`. Set `true` on Railway so the rate limit keys on the real client IP from `X-Forwarded-For`; leave it off anywhere a client could reach brain directly |
| `BRAIN_URL` | no | `http://localhost:8080`. Public URL of this API, used in emailed links |
| `WEB_URL` | no | `http://localhost:3000`. Public URL of the web app: sign-in redirects there, and an `http://` value drops the cookie's `Secure` flag |
| `REGISTRATION_DISABLED` | no | `false`. `true` stops new accounts; existing users still sign in |
| `RESEND_API_KEY` | no | Send mail through Resend (its SMTP relay) |
| `SMTP_URL` | no | Send mail through any SMTP server, e.g. `smtps://user:pass@smtp.example.com`. Used when `RESEND_API_KEY` is unset. With neither, sign-in links are written to the log at info level |
| `MAIL_FROM` | with mail | Sender, e.g. `SocialReins <login@example.com>` |
| `RAILWAY_GIT_COMMIT_SHA` | no | `dev` (Railway sets it) |
| `RUST_LOG` | no | `info` |

## Deploy

Railway builds the `Dockerfile` (see `railway.json`) and health-checks `/health`. Migrations
run when the server starts.

## License

[AGPL-3.0](LICENSE).
