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
| POST | `/mcp` | MCP server (streamable HTTP, stateless): tools for sets, accounts, posts and lists. Bearer API token only, same scope and per-token limit as `/v1`; `Idempotency-Key` does not apply |
| POST | `/v1/waitlist` | `{"email":"...","source":"..."}` joins the waitlist |
| POST | `/auth/link` | `{"email":"..."}` emails a sign-in link, `202 {"status":"sent"}` |
| GET | `/auth/magic/{token}` | The emailed link: a page with a sign-in button, does not use the link |
| POST | `/auth/magic` | Form field `token`: uses the link, sets the session cookie, redirects to `WEB_URL` |
| GET | `/auth/me` | The signed-in user, or `401 {"error":"unauthorized"}` |
| POST | `/auth/logout` | Deletes the session, `204`. Origin must be in `CORS_ORIGINS` |
| GET | `/auth/providers` | `{"google":bool}`: the optional sign-in methods this install offers |
| GET | `/auth/google/start?next=/path` | Redirects to Google (`404` when Google is not configured). `next` must be a path on the web app, else `/app` |
| GET | `/auth/google/callback` | Google's redirect back: sets the session cookie and redirects to `WEB_URL` + `next`, or `WEB_URL/welcome` for a new account, or `WEB_URL/login?error=google_failed` (or `registration_disabled`) |
| POST | `/auth/passkeys/register/start` | Needs a sign-in from the last 10 minutes: `{"challenge_id","options"}` for `navigator.credentials.create` |
| POST | `/auth/passkeys/register/finish` | `{"challenge_id","credential","name"}`: stores the passkey, `201` |
| POST | `/auth/passkeys/login/start` | `{"challenge_id","options"}` for `navigator.credentials.get` (discoverable, no email) |
| POST | `/auth/passkeys/login/finish` | `{"challenge_id","credential"}`: sets the session cookie, returns the user |
| GET | `/auth/passkeys` | The signed-in user's passkeys |
| DELETE | `/auth/passkeys/{id}` | Deletes one of the signed-in user's passkeys, `204`, else `404` |
| POST, GET | `/v1/teams` | Create a team (`{"name"}`, with its Default set; caller owns it and switches to it), or list mine |
| PUT | `/v1/current-team` | `{"team_id"}`: switch to one of my teams |
| GET, PATCH, DELETE | `/v1/team` | The current team; rename it (owner, admin); delete it (owner, sign-in from the last 10 minutes) |
| GET | `/v1/team/members` | The current team's members |
| PATCH, DELETE | `/v1/team/members/{user_id}` | `{"role"}`: change a role (owner, admin); remove a member, or leave |
| POST, GET | `/v1/team/invitations` | `{"email","role"}`: invite as `admin` or `member` (owner, admin); list pending |
| DELETE | `/v1/team/invitations/{id}` | Withdraw an invitation (owner, admin) |
| GET, PUT, DELETE | `/v1/team/webhook` | The team's outbound webhook (owner, admin): read it (no secret, `404` when none); `{"url"}` sets it (sign-in from the last 10 minutes), `201` with the `secret` once when new, `200` when changing the URL; delete it, `204` |
| POST | `/v1/team/webhook/test` | Sends a signed `ping` now: `{"status"}` from the receiver, or `{"error"}` (`blocked_address`, `timeout`, `unreachable`) |
| GET, POST | `/v1/invitations/{token}` | Public preview of an invitation; accept it as the signed-in invited email |
| GET | `/v1/connections` | The current team's connected accounts with their `set_ids`; never tokens |
| GET | `/v1/connections/providers` | Each provider with `configured`: Bluesky always, the others once their app id and secret are set |
| POST | `/v1/connections/bluesky` | `{"handle","app_password"}` (owner, admin): checks it on the account's PDS, stores it encrypted, joins the Default set; `201`, or `200` when it replaces the password of an account the team already has |
| PUT | `/v1/connections/{id}/sets` | `{"set_ids"}` (owner, admin): the sets of the current team holding the account; must include the Default set |
| DELETE | `/v1/connections/{id}` | Disconnect an account (owner, admin), `204` |
| GET, POST | `/v1/social-sets` | The current team's live sets with `account_ids`, Default first (`?deleted=true`: deleted ones still restorable); create one (`{"name"}`, owner, admin) |
| PATCH, DELETE | `/v1/social-sets/{id}` | Rename (`{"name"}`, the Default set too); delete (sign-in from the last 10 minutes, not the Default set), restorable for 30 days. Owner, admin |
| POST | `/v1/social-sets/{id}/restore` | Restore a set deleted in the last 30 days, with its accounts (owner, admin) |
| GET, POST | `/v1/posts` | The current team's posts (`?social_set_id=&status=&from=&to=` on `scheduled_at`), or create one (`{"social_set_id","platform":"bluesky","body","media":[urls],"scheduled_at"}`; any member) |
| GET, PUT, DELETE | `/v1/posts/{id}` | One post with its `targets`; replace a `draft` or `scheduled` one; delete (`204` for draft or scheduled, `200` after taking a published one down, `422 takedown_failed` with a `message`, `409` for queued or failed) |
| POST | `/v1/posts/{id}/publish` | Publish a draft or scheduled post now: `202` with the post `queued`; poll it for the result |
| GET, POST | `/v1/lists` | Post lists (ordered groups such as a series), or create one (`{"name","social_set_id"}`; no set means team-wide, which a set token cannot create) |
| GET, PATCH, DELETE | `/v1/lists/{id}` | One list with its `items` (`post_id`, 1-based `position`, `post`); rename (`{"name"}`); delete (posts stay) |
| POST | `/v1/lists/{id}/posts` | Add a post (`{"post_id","position"}`; appends by default, an existing post moves) |
| DELETE | `/v1/lists/{id}/posts/{post_id}` | Take a post out; later posts move up |
| PUT | `/v1/lists/{id}/order` | Reorder (`{"post_ids"}`: every post of the list once, else `422 invalid_order`) |
| GET, POST | `/v1/inboxes` | The team's inboxes, the undeletable "All" first; create one (`{"name","filters":{"social_set_id"\|"account_ids","kinds","platforms","keywords","unanswered"}}`, owner, admin) |
| PUT, DELETE | `/v1/inboxes/{id}` | Replace name and filters; delete (owner, admin; not "All") |
| GET | `/v1/inboxes/{id}/items` | `?state=open\|done&cursor=&limit=`: items newest first with their `replies`, and `next_cursor` |
| GET, PATCH | `/v1/inbox-items/{id}` | One item; `{"state":"open"\|"done"}` (any member) |
| POST | `/v1/inbox-items/{id}/reply` | `{"text"}` (any member): a public reply to a comment or mention, a DM for a DM; `422 {"error","message"}` when it cannot go |
| GET, POST | `/v1/automations` | The team's automations; create one (`{"name","trigger":"comment_received","conditions","actions":[{"type":"send_dm"\|"reply_publicly","text"}],"active","fallback_public_reply"}`, owner, admin) |
| GET, PUT, DELETE | `/v1/automations/{id}` | One automation; replace it; delete it (owner, admin) |
| POST | `/v1/automations/{id}/enable`, `/disable` | Switch it on (acts on comments from then on) or off (owner, admin) |
| GET | `/v1/automations/{id}/runs` | Its runs newest first (`?cursor=`): `sent`, `failed` or `skipped` with a `reason` and per-action `results` |

`POST /v1/waitlist` answers `200 {"status":"ok"}` for a new email and for one already on the
list, so it never reveals who signed up. A bad email gets `422 {"error":"invalid_email"}`. Each
client IP (IPv6 by /64) gets 10 requests a minute, then `429`.

Sign-in is by magic link, Google or passkey; there are no passwords. `POST /auth/link` answers the same `202`, at the same speed,
whether or not the email has an account (or is refused by `REGISTRATION_DISABLED`): the lookup
and the mail run after the reply. Opening the link shows a button that posts it back, so mail
scanners that prefetch links cannot use it up, and `POST /auth/magic` only accepts a post
whose `Origin` (or `Referer`) is `BRAIN_URL`. One email holds at most 3 unexpired links. The link works once, for 15 minutes; using it
creates the account if needed, then a 30-day session in the HttpOnly,
SameSite=Lax `sr_session` cookie (`Secure` unless `BRAIN_URL` is `http://`). A dead link
redirects to `WEB_URL/login?error=invalid_link` (or `registration_disabled`). Only SHA-256
hashes of link and session tokens are stored. `POST /auth/link` and `POST /auth/magic` share 5 requests a minute per IP.

Google sign-in (when `GOOGLE_CLIENT_ID` and `GOOGLE_CLIENT_SECRET` are set) uses the
authorization code flow with PKCE, a state and a nonce. The attempt is kept in Postgres for 10
minutes and used once; the browser holds the state in an HttpOnly cookie scoped to
`/auth/google`, so only the browser that started can finish (login CSRF). The id_token comes
straight from Google's token endpoint over TLS, so brain checks its issuer, audience, expiry,
nonce and `email_verified` but not its signature. An account is found by its linked Google
`sub`. Otherwise the email counts only when Google vouches for the mailbox itself (a gmail.com
or googlemail.com address, or an address on the account's Workspace domain `hd`): then it finds
the account of that email (linking the `sub`; an email linked to another Google account is
refused) or creates one under the same rules as a magic link, `OWNER_EMAIL` included. Any other
address links only to the account signed in on that browser in the last 10 minutes with the
same email (sign in by magic link once, then use Google). A new account lands on
`WEB_URL/welcome`, with `?next=` when it was going elsewhere. Errors redirect with no details. Register `{BRAIN_URL}/auth/google/callback` as the redirect URI.

Adding a passkey, deleting one, deleting a team or a social set, changing a member's role,
removing another member, inviting an admin and disconnecting an account need a session started
in the last 10 minutes (`403 reauth_required`: sign in again). Passkey and Google
sign-in share 10 attempts a minute per IP. Deleting a passkey ends every other session of the
user.

Passkeys use WebAuthn through the web app's pages: each `start` keeps the ceremony state in
Postgres for 5 minutes and the matching `finish` uses it once. The state-changing endpoints in
this section only accept requests whose `Origin` (or `Referer`) is one of `CORS_ORIGINS`, else
`403 {"error":"forbidden_origin"}`.

Teams: every sign-in gives a user with no team a personal team, owned by them, with its Default
social set. The team routes use the session cookie, and the state-changing ones the same origin
check. They only reach the caller's own teams: anything else is `404 {"error":"not_found"}`,
and a role that may not act gets `403 {"error":"forbidden"}`. The last owner cannot be demoted
or leave (`409 last_owner`), and a user's last team cannot be deleted (`409 last_team`).
Deleting a team needs a sign-in from the last 10 minutes. Invitations email a
link to `WEB_URL/invitations/{token}` that works once within 7 days, for the invited email only
(`403 email_mismatch`); only its SHA-256 is stored. An invited email may sign up even with
`REGISTRATION_DISABLED`. Each user sends at most 10 invitations a minute and 50 a day, each
team 50 a day, and one address gets at most 3 a day from one team and 20 a day overall; only
sent invitations count. Re-inviting a pending address replaces its invitation. Each user creates at most 10 teams a day. The daily caps are
counted in Postgres (`rate_limit_hits`), so deletes and restarts do not reset them. Team names are 1 to 60 characters. An invitation dies with its inviter's right to
invite: when they are removed, leave or drop below admin, theirs are deleted.

Social sets group a team's accounts. Names follow the team name rules and are unique among a
team's live sets in any case (`409 name_taken`); a team holds at most 50 live sets
(`409 too_many_sets`). The Default set holds every account; it can be renamed but not deleted
(`409 default_set`). Deleting a set needs a sign-in from the last 10 minutes;
it keeps its account links, hidden, so a restore within 30 days brings them back. An hourly
job in brain hard-deletes sets deleted over 30 days ago, one instance at a time (advisory lock).

Posts go to every account of their platform in their social set; only Bluesky publishes
today (300 graphemes, 4 images of at most 1 MB from public https URLs, links become facets).
Saving with `scheduled_at` makes a post `scheduled`, else `draft`; only those two can change.
A tick every minute claims due posts (one instance at a time, advisory lock), creates one
target per account and claims each target once, so nothing posts twice; a publish is never
retried. A target cut off mid-publish for 15 minutes fails as "unknown, check the account".
Posts of a deleted set are hidden and held back until it is restored. Errors never carry URLs
or tokens. A team keeps at most 2000 posts that are not deleted; publish now and take-down are
10 a minute per user. A team publishes at most 100 posts a day, scheduled or now (`429
publish_quota` on publish now); a due post over the limit stays `scheduled` and goes out once
it frees up. A tick claims at most 50 posts; 8 posts and 8 targets publish at once. A past
`scheduled_at` means now.

API token writes (`POST`, `PUT`, `PATCH`, `DELETE`) take an optional `Idempotency-Key` header,
1 to 255 printable ASCII characters (else `422 invalid_idempotency_key`). Keys belong to the
token's team. A retry with the same key, method, path and body gets the first answer again with
`Idempotent-Replayed: true`, or `409 idempotency_in_progress` while the first still runs; the
same key with anything else is `422 idempotency_key_reused`. 5xx and 429 answers are not kept,
so a retry runs again; every other answer is kept for 30 days. A first run unfinished after 10
minutes is taken as crashed and runs again. Sessions ignore the header.

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

Webhooks: a team may set one public `https://` URL (at most 2000 characters, no credentials,
not `localhost` or a private address). Each post target fires `post.target.published`,
`post.target.failed` and `post.target.deleted` at most once each, so a post that went out to
some accounts and not others shows as one event per account. Every delivery is a `POST` of
JSON with these headers: `SocialReins-Event` (the type), `SocialReins-Delivery` (the delivery
id, the same on retries) and `SocialReins-Signature: t=<unix seconds>,v1=<hex>`. The body:

```json
{"id": "<delivery id>", "type": "post.target.published", "created_at": "<time>",
 "data": {"post_id": "...", "group_id": null, "target_id": "...", "social_account_id": "...",
          "platform": "bluesky", "status": "published", "remote_url": "https://...", "error": null}}
```

A 2xx answer within 10 seconds counts as delivered; redirects are not followed and the body is
not read. Otherwise brain tries again after 1 minute, 5 minutes, 30 minutes, 2, 6, 12 and 24
hours, then gives up. Brain only connects to public addresses, checked when the name resolves.
`group_id` stays `null` until posts have groups. To verify a delivery, compute HMAC-SHA256 with
the whole secret (`whsec_...`) as the key over `<t>.<raw body>` and compare it with `v1`;
refuse a `t` more than 5 minutes old:

```python
import hashlib, hmac, time
t, v1 = (p.split("=", 1)[1] for p in header.split(","))
mac = hmac.new(secret.encode(), f"{t}.".encode() + raw_body, hashlib.sha256).hexdigest()
assert hmac.compare_digest(mac, v1) and abs(time.time() - int(t)) < 300
```

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
| `GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET` | no | Both or neither. Turn on Google sign-in (OAuth client of type Web application, redirect URI `{BRAIN_URL}/auth/google/callback`); YouTube will use them too |

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
