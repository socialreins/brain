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
| GET | `/auth/magic/{token}` | The emailed link: a page with a sign-in button, does not use the link |
| POST | `/auth/magic` | Form field `token`: uses the link, sets the session cookie, redirects to `WEB_URL` |
| GET | `/auth/me` | The signed-in user, or `401 {"error":"unauthorized"}` |
| POST | `/auth/logout` | Deletes the session, `204`. Origin must be in `CORS_ORIGINS` |
| POST | `/auth/login` | `{"email","password"}`: sets the session cookie, returns the user |
| PUT | `/auth/password` | `{"password","current_password"}`: sets or changes the signed-in user's password, `204` |
| POST | `/auth/passkeys/register/start` | `{}` or `{"password"}`: `{"challenge_id","options"}` for `navigator.credentials.create` |
| POST | `/auth/passkeys/register/finish` | `{"challenge_id","credential","name"}`: stores the passkey, `201` |
| POST | `/auth/passkeys/login/start` | `{"challenge_id","options"}` for `navigator.credentials.get` (discoverable, no email) |
| POST | `/auth/passkeys/login/finish` | `{"challenge_id","credential"}`: sets the session cookie, returns the user |
| GET | `/auth/passkeys` | The signed-in user's passkeys |
| DELETE | `/auth/passkeys/{id}` | Deletes one of the signed-in user's passkeys, `204`, else `404` |
| POST, GET | `/v1/teams` | Create a team (`{"name"}`, with its Default set; caller owns it and switches to it), or list mine |
| PUT | `/v1/current-team` | `{"team_id"}`: switch to one of my teams |
| GET, PATCH, DELETE | `/v1/team` | The current team; rename it (owner, admin); delete it (owner, `{}` or `{"password"}`) |
| GET | `/v1/team/members` | The current team's members |
| PATCH, DELETE | `/v1/team/members/{user_id}` | `{"role"}`: change a role (owner, admin); remove a member, or leave |
| POST, GET | `/v1/team/invitations` | `{"email","role"}`: invite as `admin` or `member` (owner, admin); list pending |
| DELETE | `/v1/team/invitations/{id}` | Withdraw an invitation (owner, admin) |
| GET, POST | `/v1/invitations/{token}` | Public preview of an invitation; accept it as the signed-in invited email |
| GET | `/v1/connections` | The current team's connected accounts with their `set_ids`; never tokens |
| GET | `/v1/connections/providers` | Each provider with `configured`: Bluesky always, the others once their app id and secret are set |
| POST | `/v1/connections/bluesky` | `{"handle","app_password"}` (owner, admin): checks it on the account's PDS, stores it encrypted, joins the Default set; `201`, or `200` when it replaces the password of an account the team already has |
| PUT | `/v1/connections/{id}/sets` | `{"set_ids"}` (owner, admin): the sets of the current team holding the account; must include the Default set |
| DELETE | `/v1/connections/{id}` | Disconnect an account (owner, admin), `204` |

`POST /v1/waitlist` answers `200 {"status":"ok"}` for a new email and for one already on the
list, so it never reveals who signed up. A bad email gets `422 {"error":"invalid_email"}`. Each
client IP (IPv6 by /64) gets 10 requests a minute, then `429`.

Sign-in is by magic link, password or passkey. `POST /auth/link` answers the same `202`, at the same speed,
whether or not the email has an account (or is refused by `REGISTRATION_DISABLED`): the lookup
and the mail run after the reply. Opening the link shows a button that posts it back, so mail
scanners that prefetch links cannot use it up, and `POST /auth/magic` only accepts a post
whose `Origin` (or `Referer`) is `BRAIN_URL`. One email holds at most 3 unexpired links. The link works once, for 15 minutes; using it
creates the account if needed, then a 30-day session in the HttpOnly,
SameSite=Lax `sr_session` cookie (`Secure` unless `BRAIN_URL` is `http://`). A dead link
redirects to `WEB_URL/login?error=invalid_link` (or `registration_disabled`). Only SHA-256
hashes of link and session tokens are stored. `POST /auth/link` and `POST /auth/magic` share 5 requests a minute per IP.

Passwords are optional and hashed with Argon2id (12 to 256 characters). `POST /auth/login`
answers the same `401 {"error":"invalid_credentials"}` for an unknown email, an account without
a password and a wrong password, and takes about as long for each. Changing a password needs the
current one; setting the first one, or adding or deleting a passkey, needs a session started in the last 10
minutes (sign in again otherwise; adding a passkey also accepts the password). Password and
passkey sign-in share 10 attempts a minute per IP. Per email only failed passwords count: 5 a
minute closes the email until the window ends, and a success clears the count, so strangers
cannot lock out an owner who knows the password. A signed-in user's current-password checks
have their own budget of 5 failures a minute per account, which outsiders cannot drain; each
failure also counts against the email's login budget. Setting or changing a password, or deleting a passkey, ends every other
session of the user; setting or changing a password also voids unused sign-in links.

Passkeys use WebAuthn through the web app's pages: each `start` keeps the ceremony state in
Postgres for 5 minutes and the matching `finish` uses it once. The state-changing endpoints in
this section only accept requests whose `Origin` (or `Referer`) is one of `CORS_ORIGINS`, else
`403 {"error":"forbidden_origin"}`.

Teams: every sign-in gives a user with no team a personal team, owned by them, with its Default
social set. The team routes use the session cookie, and the state-changing ones the same origin
check. They only reach the caller's own teams: anything else is `404 {"error":"not_found"}`,
and a role that may not act gets `403 {"error":"forbidden"}`. The last owner cannot be demoted
or leave (`409 last_owner`), and a user's last team cannot be deleted (`409 last_team`).
Deleting a team needs a sign-in from the last 10 minutes or the password. Invitations email a
link to `WEB_URL/invitations/{token}` that works once within 7 days, for the invited email only
(`403 email_mismatch`); only its SHA-256 is stored. An invited email may sign up even with
`REGISTRATION_DISABLED`. Each user sends at most 10 invitations a minute and 50 a day, each
team 50 a day, and one address gets at most 3 a day from one team and 20 a day overall; only
sent invitations count. Re-inviting a pending address replaces its invitation. Each user creates at most 10 teams a day. The daily caps are
counted in Postgres (`rate_limit_hits`), so deletes and restarts do not reset them. Team names are 1 to 60 characters. An invitation dies with its inviter's right to
invite: when they are removed, leave or drop below admin, theirs are deleted.

Connections are the current team's social accounts. Members list them; owners and admins connect,
regroup and disconnect them. Tokens are sealed with `ENCRYPTION_KEY` and never returned. Every
account sits in the team's Default set, which `PUT /v1/connections/{id}/sets` cannot drop
(`422 default_set_required`). Bluesky needs no app: brain resolves the handle to a DID
(`public.api.bsky.app`), the DID to its PDS (`plc.directory` or the domain's `did.json`), then
opens a session there with the app password, which is sent only to that PDS over https with
redirects off. A wrong password and an account password get the same `invalid_credentials`. Errors:
`invalid_handle`, `unsupported_pds`, `invalid_credentials` (all 422), `rate_limited`,
`bluesky_unreachable` (502). Attempts count like sign-in: per IP, 5 a minute per user, and 30
a day per user (handles that do not resolve included), 10 a day per account (DID) in one team, and 20 refused passwords a day per
account across all teams, which a team already holding the account is exempt from. The DID
document must list the handle (`alsoKnownAs`), or the answer is `invalid_handle`. Bluesky hosts must resolve to public addresses (`unsupported_pds`), and
responses over 64 KiB are dropped.

## Local development

You need Rust (stable) and Docker.

```sh
docker compose up -d postgres
export DATABASE_URL=postgres://brain:brain@localhost:5432/socialreins_brain
export MAIL_LOG=true                  # sign-in links go to the log
export ENCRYPTION_KEY=$(openssl rand -base64 32)
# or: cp .env.example .env and load it with your shell or direnv
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
| `ENCRYPTION_KEY` | yes | Encrypts platform tokens and Bluesky app passwords at rest (AES-256-GCM). 32 bytes, base64: `openssl rand -base64 32`. brain refuses to start without a valid one. Keep it with your database backups: losing or changing it makes every connected account need reconnecting |
| `PORT` | no | `8080` |
| `CORS_ORIGINS` | no | The origin of `WEB_URL`. Comma-separated; hosted sets `https://socialreins.com,https://www.socialreins.com` |
| `TRUST_PROXY` | no | `false`. Set `true` on Railway so the rate limit keys on the real client IP from `X-Forwarded-For`; leave it off anywhere a client could reach brain directly |
| `BRAIN_URL` | no | `http://localhost:8080`. Public URL of this API: emailed links point here, sign-in posts must come from its origin, and an `http://` value drops the cookie's `Secure` flag |
| `WEB_URL` | no | `http://localhost:3000`. Public URL of the web app: sign-in redirects there |
| `PASSKEY_RP_ID` | no | The host of `PASSKEY_RP_ORIGIN`. WebAuthn relying party id: the domain web and brain share, e.g. `socialreins.com` |
| `PASSKEY_RP_ORIGIN` | no | The origin of `WEB_URL`. Where the browser runs the passkey ceremonies |
| `REGISTRATION_DISABLED` | no | `false`. `true` closes sign-up: only `OWNER_EMAIL` and invited emails can sign up (with no `OWNER_EMAIL`, the first account to sign up, then invited emails); existing users still sign in |
| `OWNER_EMAIL` | no | With `REGISTRATION_DISABLED=true`, the one new email allowed to sign up besides invited ones, any case. Recommended for personal installs: otherwise whoever signs up first owns the install |
| `RESEND_API_KEY` | no | Send mail through the Resend HTTPS API (works where outbound SMTP is blocked, like Railway) |
| `SMTP_URL` | no | Send mail through any SMTP server, e.g. `smtps://user:pass@smtp.example.com`. Used when `RESEND_API_KEY` is unset |
| `MAIL_LOG` | if no mail | `false`. With no Resend key and no SMTP, brain refuses to start unless this is `true`; then sign-in links are written to the log at info level. Meant for a single-user self-host |
| `MAIL_FROM` | with mail | Sender, e.g. `SocialReins <login@example.com>` |
| `RAILWAY_GIT_COMMIT_SHA` | no | Commit `/version` reports (Railway sets it) |
| `GIT_COMMIT` | no | Used when `RAILWAY_GIT_COMMIT_SHA` is unset; baked into published images. Else `dev` |
| `RUST_LOG` | no | `info` |
| `META_APP_ID`, `META_APP_SECRET` | no | Offer Facebook and Instagram (connector not built yet) |
| `THREADS_APP_ID`, `THREADS_APP_SECRET` | no | Offer Threads (connector not built yet) |
| `LINKEDIN_CLIENT_ID`, `LINKEDIN_CLIENT_SECRET` | no | Offer LinkedIn (connector not built yet) |
| `X_CLIENT_ID`, `X_CLIENT_SECRET` | no | Offer X (connector not built yet) |
| `TIKTOK_CLIENT_KEY`, `TIKTOK_CLIENT_SECRET` | no | Offer TikTok (connector not built yet) |
| `GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET` | no | Offer YouTube (connector not built yet) |

## Deploy

Railway builds the `Dockerfile` (see `railway.json`) and health-checks `/health`. Migrations
run when the server starts; applied ones are recorded and skipped, so restarts and upgrades are
safe.

Self-hosters use the published image, `ghcr.io/socialreins/brain` (`linux/amd64` and
`linux/arm64`): `:main` and `:<commit sha>` follow `main`, `:vX.Y.Z` and `:latest` follow
releases (`.github/workflows/image.yml`). The install guide lives in
[socialreins/web](https://github.com/socialreins/web#self-host).

## License

[AGPL-3.0](LICENSE).
