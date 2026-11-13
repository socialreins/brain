<p align="center"><img src="https://raw.githubusercontent.com/socialreins/socialreins/main/brand/logo.svg" width="96" height="96" alt="SocialReins logo"></p>

<h1 align="center">brain</h1>

The Rust backend for [SocialReins](https://socialreins.com), a social media manager for
scheduling and publishing posts across platforms. For the project overview, roadmap and the
other repos, see the main repo [socialreins/socialreins](https://github.com/socialreins/socialreins).

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
| POST | `/mcp` | MCP server (streamable HTTP, stateless): tools for sets, accounts, posts and lists. Bearer API token only, same scope and per-token limit as `/v1`, bodies up to 64 KiB; `Idempotency-Key` does not apply. `create-post` and `create-post-group` take `validate_only` (see "Validate only"), which a read-only token may send |
| POST | `/v1/waitlist` | `{"email":"...","source":"..."}` joins the waitlist and emails a confirmation link |
| POST | `/v1/waitlist/verify` | `{"token"}` from the emailed link: marks the signup verified, `200 {"status":"verified"}`, else `422 {"error":"invalid_link"}` |
| GET | `/v1/waitlist/count` | `{"count":n}`: every signup, verified or not, cached for 30 seconds |
| GET | `/v1/admin/waitlist` | `ADMIN_EMAILS` only, session cookie: `{"total","verified","signups":[{"id","email","source","created_at","verified_at","approved_at"}]}`, newest first, at most 1000 (`?verified=true\|false` and `?q=` (part of the email, any case) filter the list) |
| GET | `/v1/admin/overview` | Admin console (every `/v1/admin/*` route: `ADMIN_EMAILS` only, session cookie). Totals: `{"users","new_users_7d","teams","accounts":{"total","by_provider":{"bluesky":n}},"posts":{"scheduled","published","failed",...},"waitlist":{"total","verified","approved"}}` |
| GET | `/v1/admin/users` | `?q=` (part of the email or name, any case), `?limit=` (1 to 200, default 50), `?offset=`: `{"total","users":[{"id","email","name","is_admin","created_at","last_sign_in_at","teams":[{"id","name","role"}]}]}`, newest first. `last_sign_in_at` is the start of the newest session still stored, else `null` |
| GET | `/v1/admin/teams` | `?q=` (part of the name, any case), `?limit=`, `?offset=`: `{"total","teams":[{"id","name","created_at","members","accounts","posts","owner_email"}]}`, newest first; `posts` leaves out deleted ones |
| POST | `/v1/admin/waitlist/{id}/approve` | Approves a signup: its email may sign up whatever the sign-up rules say and is mailed a link to `WEB_URL/signup`. `{"signup":{...},"mail":"sent"\|"failed"\|"already_approved"}`; a second call sends nothing, a failed mail keeps the approval. `404 not_found` |
| DELETE | `/v1/admin/waitlist/{id}` | Removes a signup (and its approval, if it has not signed up yet), `204`. `404 not_found` |
| GET, PUT | `/v1/admin/settings/signup` | The sign-up rules: `{"mode":"open"\|"closed"\|"domains","domains":[...],"source":"env"\|"saved"}`. `PUT {"mode","domains"}` saves them (sign-in from the last 10 minutes, else `403 reauth_required`): at most 50 domains or email addresses, trimmed, lowercased, deduplicated, at least one for `domains` (`422 invalid_mode`, `invalid_domains`) |
| POST | `/v1/contact` | `{"name","email","message"}` mails `CONTACT_TO`, `202 {}` |
| POST | `/auth/link` | `{"email":"...","name":"..."}` emails a sign-in link, `202 {"status":"sent"}`. `name` is optional, 1 to 80 characters (else `422 invalid_name`), and only names an account this link creates. With `"login_only":true`, an email without an account is sent a pointer to sign up instead of a link |
| POST | `/auth/link/me` | Session cookie. Emails a sign-in link to the signed-in user's own address, `202 {"status":"sent"}`. No body and no captcha; opening the link starts a fresh session, for actions that answer `403 reauth_required` |
| GET | `/auth/magic/{token}` | The emailed link: a page with a sign-in button, does not use the link |
| POST | `/auth/magic` | Form field `token`: uses the link, sets the session cookie, redirects to `WEB_URL` |
| GET | `/auth/me` | The signed-in user with `is_admin` (in `ADMIN_EMAILS`), or `401 {"error":"unauthorized"}` |
| PATCH | `/auth/me` | `{"name":"..."}`: trimmed, 1 to 80 characters, no control characters, else `422 {"error":"invalid_name"}`. Replies like `GET /auth/me`. Origin must be in `CORS_ORIGINS` |
| POST | `/auth/logout` | Deletes the session, `204`. Origin must be in `CORS_ORIGINS` |
| GET | `/auth/providers` | `{"google":bool,"turnstile_site_key":"..."\|null}`: the optional sign-in methods this install offers, and the Turnstile key when the captcha is on |
| GET | `/auth/google/start?next=/path&mode=login` | Redirects to Google (`404` when Google is not configured). `next` must be a path on the web app, else `/app`. `mode=login` never creates an account: a new Google account lands on `WEB_URL/signup?error=no_account` |
| GET | `/auth/google/callback` | Google's redirect back: sets the session cookie and redirects to `WEB_URL` + `next`, or `WEB_URL/welcome` for a new account, or `WEB_URL/login?error=google_failed` (or `registration_disabled` with `mode=login`), or `WEB_URL/signup?waitlisted=1` when sign-up is refused and the email joined the waitlist |
| GET | `/review-login/{token}` | App Review sign-in, only while `REVIEW_LOGIN_TOKEN` is set (else `404`, as for a wrong token): signs the browser in to the demo user `review@socialreins.com` and redirects (`303`) to `WEB_URL/app`. See [App Review login](#app-review-login) |
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
| GET | `/connections/threads/redirect` | A page navigation from web (session cookie; owner, admin of the current team): sends the browser to Threads to connect a profile. Lands back on `{WEB_URL}/app/connections` |
| GET | `/connect/threads/callback` | Where Threads sends the browser back (register `{BRAIN_URL}/connect/threads/callback` in the Threads app); lands on `{WEB_URL}/app/connections?connected=threads` or `?connect_error=<code>&provider=threads` |
| GET | `/connections/linkedin/redirect` | Like the Threads one: sends the browser to LinkedIn (scopes `openid profile email w_member_social`) to connect a member's profile |
| GET | `/connect/linkedin/callback` | Where LinkedIn sends the browser back (register `{BRAIN_URL}/connect/linkedin/callback` in the LinkedIn app); lands on `{WEB_URL}/app/connections?connected=linkedin` or `?connect_error=<code>&provider=linkedin` |
| GET | `/connections/youtube/redirect` | Like the Threads one: sends the browser to Google (scopes `youtube.upload`, `youtube.readonly`, `youtube.force-ssl`; `access_type=offline`, `prompt=consent`, `include_granted_scopes=false`) to connect a YouTube channel |
| GET | `/connect/youtube/callback` | Where Google sends the browser back (register `{BRAIN_URL}/connect/youtube/callback` in the Google OAuth client, besides the sign-in one); lands on `{WEB_URL}/app/connections?connected=youtube` or `?connect_error=<code>&provider=youtube` |
| GET | `/connections/x/redirect` | Like the Threads one, with PKCE: sends the browser to `x.com/i/oauth2/authorize` (scopes `tweet.read tweet.write users.read offline.access media.write`, `code_challenge_method=S256`) to connect an X account |
| GET | `/connect/x/callback` | Where X sends the browser back (register `{BRAIN_URL}/connect/x/callback` in the X app); lands on `{WEB_URL}/app/connections?connected=x` or `?connect_error=<code>&provider=x` |
| GET | `/v1/connections/{id}/tiktok` | What a connected TikTok account may post now (any member; a set token only for its set's account): `{"nickname","username","privacy_level_options","comment_disabled","duet_disabled","stitch_disabled","max_video_post_duration_sec","can_post","reason"}`, read live from TikTok's creator info. `can_post` false (a daily limit, a ban, the app's user cap, no level offered) comes with `reason`, still `200`. `409 reconnect_required` for a dead token, `502 tiktok_failed` when TikTok cannot be reached |
| GET | `/connections/tiktok/redirect` | Like the Threads one: sends the browser to TikTok (scopes `user.info.basic,video.upload,video.publish`) to connect an account |
| GET | `/connect/tiktok/callback` | Where TikTok sends the browser back (register `{BRAIN_URL}/connect/tiktok/callback` in the TikTok app); lands on `{WEB_URL}/app/connections?connected=tiktok` or `?connect_error=<code>&provider=tiktok` |
| GET | `/connections/meta/redirect` | Like the Threads one: sends the browser to Facebook Login for Pages and their linked Instagram accounts |
| GET, POST | `/webhooks/meta` | Meta's webhook (register `{BRAIN_URL}/webhooks/meta` in the Meta app). `GET` is the subscribe handshake: `hub.mode=subscribe` and a `hub.verify_token` equal to `META_WEBHOOK_VERIFY_TOKEN` get `hub.challenge` back as plain text, else `403`; `404` while the variable is unset. `POST` needs `X-Hub-Signature-256` (HMAC-SHA256 of the raw body under `META_APP_SECRET`, `403` otherwise, `404` without the secret), takes at most 256 KiB, and answers `200` once the items are stored |
| GET | `/connect/meta/callback` | Where Meta sends the browser back (register `{BRAIN_URL}/connect/meta/callback` in the Meta app); lands on `{WEB_URL}/app/connections?pick=meta` when there is something to pick, `?connected=meta` when not, or `?connect_error=<code>&provider=meta` |
| GET, POST, DELETE | `/v1/connections/meta/pick` | The caller's pending Meta pick (owner, admin): the Pages and Instagram accounts not connected yet (`{"facebook":[{"id","name","picture_url"}],"instagram":[...]}`, never tokens, `404` when none or over 15 minutes old); `{"facebook":[ids],"instagram":[ids]}` connects them (`201` with the accounts; `422 nothing_picked`, `invalid_pick`); drop it, `204` |
| GET | `/media/{name}` | A stored post file at an unguessable link, for platforms that download media themselves (Threads, Facebook, Instagram); `404` once the post settles |
| PUT | `/v1/connections/{id}/sets` | `{"set_ids"}` (owner, admin): the sets of the current team holding the account; must include the Default set |
| DELETE | `/v1/connections/{id}` | Disconnect an account (owner, admin), `204` |
| GET, POST | `/v1/social-sets` | The current team's live sets with `account_ids`, Default first (`?deleted=true`: deleted ones still restorable); create one (`{"name"}`, owner, admin) |
| PATCH, DELETE | `/v1/social-sets/{id}` | Rename (`{"name"}`, the Default set too); delete (sign-in from the last 10 minutes, not the Default set), restorable for 30 days. Owner, admin |
| POST | `/v1/social-sets/{id}/restore` | Restore a set deleted in the last 30 days, with its accounts (owner, admin) |
| GET, POST | `/v1/posts` | The current team's posts (`?social_set_id=&status=&group_id=&from=&to=` on `scheduled_at`; the Default set's id gives every set's posts), or create one (`{"social_set_id","platform":"bluesky","body","media":[urls],"thread":[{"body","media"}],"first_comment","cover","tiktok","youtube","scheduled_at"}`; any member). `first_comment`: see "First comment" below; `cover`: see "Video cover"; `tiktok`: see "TikTok settings"; `youtube`: see YouTube below. `thread`: up to 25 follow-up posts, each a reply to the one before and checked like the post (`422 {"error","part"}` names a bad one); `?validate_only=true` checks without creating (see "Validate only"); each part is saved on its target as it goes out, a target whose part fails is `failed` naming it with the parts before it kept, and a takedown deletes the parts last first, then the root |
| GET, PUT, DELETE | `/v1/posts/{id}` | One post with its `targets`; replace a `draft` or `scheduled` one; delete (`204` for draft or scheduled, `200` after taking a published one down, `422 takedown_failed` with a `message`, `409` for queued or failed) |
| POST | `/v1/post-groups` | One post per platform of a set in one call (`{"social_set_id","platforms","body","media","scheduled_at","list_id","position","thread","first_comment","cover","tiktok","youtube","overrides":{"<platform>":{"body","media","thread","first_comment","cover","tiktok","youtube"}}}`; `platforms` defaults to those brain publishes to with a connected account; bodies up to 64 KiB): `201 {"group_id","posts","skipped"}`; an invalid post creates nothing and its error names the `platform`, unless `"on_invalid":"skip"` (see "Validate only"). `?validate_only=true` checks without creating |
| POST | `/v1/posts/{id}/publish` | Publish a draft or scheduled post now: `202` with the post `queued`; poll it for the result |
| GET, POST | `/v1/lists` | Post lists (ordered groups such as a series), or create one (`{"name","social_set_id"}`; no set means team-wide, or the token's own set for a set token) |
| GET, PATCH, DELETE | `/v1/lists/{id}` | One list with its `items` (`post_id`, 1-based `position`, `post`); rename (`{"name"}`); delete (posts stay) |
| POST | `/v1/lists/{id}/posts` | Add a post (`{"post_id","position"}`; appends by default, an existing post moves) |
| DELETE | `/v1/lists/{id}/posts/{post_id}` | Take a post out; later posts move up |
| PUT | `/v1/lists/{id}/order` | Reorder (`{"post_ids"}`: every post of the list once, else `422 invalid_order`) |
| GET, POST | `/v1/inboxes` | The team's inboxes, the undeletable "All" first; create one (`{"name","filters":{"social_set_id"\|"account_ids","kinds","platforms","keywords","unanswered"}}`, owner, admin) |
| PUT, DELETE | `/v1/inboxes/{id}` | Replace name and filters; delete (owner, admin; not "All") |
| GET | `/v1/inboxes/{id}/items` | `?state=open\|done&cursor=&limit=`: items newest first with their `replies`, and `next_cursor` |
| GET, PATCH | `/v1/inbox-items/{id}` | One item; `{"state":"open"\|"done"}` (any member) |
| POST | `/v1/inbox-items/{id}/reply` | `{"text"}` (any member): a public reply to a comment or mention, a DM for a DM; `422 {"error","message"}` when it cannot go (on Facebook and Instagram `meta_reconnect_required`, `meta_failed`, `outside_window`, `dm_unavailable`) |
| GET, POST | `/v1/automations` | The team's automations; create one (`{"name","trigger":"comment_received","conditions","actions":[{"type":"send_dm"\|"reply_publicly","text"}],"active","fallback_public_reply"}`, owner, admin) |
| GET, PUT, DELETE | `/v1/automations/{id}` | One automation; replace it; delete it (owner, admin) |
| POST | `/v1/automations/{id}/enable`, `/disable` | Switch it on (acts on comments from then on) or off (owner, admin) |
| GET | `/v1/automations/{id}/runs` | Its runs newest first (`?cursor=`): `sent`, `failed` or `skipped` with a `reason` and per-action `results` |

`POST /v1/waitlist` answers `200 {"status":"ok"}` for a new email and for one already on the
list, so it never reveals who signed up. A bad email gets `422 {"error":"invalid_email"}`. Each
client IP (IPv6 by /64) gets 10 requests a minute, then `429`. A new or still unverified email
gets a confirmation email ("Confirm your spot on the SocialReins waitlist") after the reply, at
most once an hour however often it signs up; a verified one gets none. At most 20 confirmation
emails go out per client IP (IPv6 by /64) and 500 across all signups per rolling 24 hours; past
either, and when a mail fails, the
signup is still saved and answered ok, nothing is sent, and the next signup of that email may
try again. The link opens the web app at `WEB_URL/waitlist/verify?token=...`, which posts the
token to `POST /v1/waitlist/verify` (so a mail scanner opening the link changes nothing): the
first post sets `verified_at`, later ones answer the same `200`, and a missing, malformed or
unknown token gets `422 invalid_link`. A resend replaces the token. It shares the waitlist's
10 requests a minute per IP. Only the token's SHA-256 hash is stored. `GET /v1/waitlist/count` is public. `GET /v1/admin/waitlist` needs a session cookie
(not an API token) of a user in `ADMIN_EMAILS`: `401 unauthorized` signed out, `403 forbidden`
otherwise. So does every other `/v1/admin/*` route (the admin console); the ones that change
something also need an `Origin` in `CORS_ORIGINS`. None of them returns post bodies, tokens or
credentials.

`POST /v1/contact` (on when `CONTACT_TO` is set, else `503 {"error":"contact_unavailable"}`)
takes a name of 1 to 100 characters on one line, an email and a message of 1 to 5000
characters (`422` `invalid_name`, `invalid_email` or `invalid_message`), and mails them to
`CONTACT_TO` with the sender as Reply-To. Each client IP gets 5 an hour, then `429`.

With `TURNSTILE_SITE_KEY` and `TURNSTILE_SECRET_KEY` set, `POST /auth/link` and `/v1/contact`
need a `turnstile_token` from the widget, checked with Cloudflare after the
rate limit and before anything is stored or sent: a missing or rejected token gets
`422 {"error":"captcha_failed"}`, and Cloudflare unreachable for 5 seconds gets
`503 {"error":"captcha_unavailable"}`. With Turnstile off the field is ignored, and
`/v1/waitlist` always ignores it.

Sign-in is by magic link, Google or passkey; there are no passwords. `POST /auth/link` answers the same `202`, at the same speed,
whether or not the email has an account (or is refused by `REGISTRATION_DISABLED` or `SIGNUP_DOMAINS`, see sign-up rules below): the lookup
and the mail run after the reply. Opening the link shows a button that posts it back, so mail
scanners that prefetch links cannot use it up, and `POST /auth/magic` only accepts a post
whose `Origin` (or `Referer`) is `BRAIN_URL`. One email holds at most 3 unexpired links. The link works once, for 15 minutes; using it
creates the account if needed, then a 30-day session in the HttpOnly,
SameSite=Lax `sr_session` cookie (`Secure` unless `BRAIN_URL` is `http://`). A dead link
redirects to `WEB_URL/login?error=invalid_link` (or `registration_disabled`). Only SHA-256
hashes of link and session tokens are stored. `POST /auth/link` and `POST /auth/magic` share 5 requests a minute per IP.

Sign-up rules: an existing account always signs in. A new account needs its whole address or
its domain in `SIGNUP_DOMAINS` (anyone when unset) and open registration (`REGISTRATION_DISABLED`
off); `OWNER_EMAIL`, invited emails and waitlist emails an admin approved
(`POST /v1/admin/waitlist/{id}/approve`) are always admitted. Once an admin saves rules with
`PUT /v1/admin/settings/signup` they replace `REGISTRATION_DISABLED` and `SIGNUP_DOMAINS` (mode
`open`: anyone, `closed`: nobody new, `domains`: the listed domains and addresses) without a
restart, and the same three exemptions hold; a server where nothing was saved follows the two
variables as before. Anyone else who tries to sign up
goes on the waitlist instead. By email link (not `login_only`): the same `202`, and after the
reply the address joins the waitlist (source `signup`) with the waitlist's own confirmation
email, which starts "Sign-ups are closed for now. We've added you to the waitlist and will email
you when SocialReins is live." It follows the waitlist's rules: unverified until the link is
used, at most one email an hour per address, none once verified, 20 a day per IP and 500 a day
overall. By Google: the address joins the waitlist as verified (source `google`; a known
unverified signup becomes verified) and the browser lands on `WEB_URL/signup?waitlisted=1`. A
refused login-only attempt (the login page's email link, or Google with `mode=login`) joins
nothing: the link sends nothing and Google lands on `WEB_URL/login?error=registration_disabled`.

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

Teams: every sign-in gives a user with no team a personal team named after the first word of
their name, owned by them, with its Default social set. The team routes use the session cookie, and the state-changing ones the same origin
check. They only reach the caller's own teams: anything else is `404 {"error":"not_found"}`,
and a role that may not act gets `403 {"error":"forbidden"}`. The last owner cannot be demoted
or leave (`409 last_owner`), and a user's last team cannot be deleted (`409 last_team`).
Deleting a team needs a sign-in from the last 10 minutes. Invitations email a
link to `WEB_URL/invitations/{token}` that works once within 7 days, for the invited email only
(`403 email_mismatch`); only its SHA-256 is stored. An invited email may sign up even with
`REGISTRATION_DISABLED` or outside `SIGNUP_DOMAINS`. Each user sends at most 10 invitations a minute and 50 a day, each
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

Posts go to every account of their platform in their social set; Bluesky (300 graphemes, 4
images from public https URLs, a still image over 1 MB shrunk to fit, links become facets) and Threads (500
characters, up to 20 JPEG or PNG images or MP4 or MOV videos as a carousel), LinkedIn (3000
characters, up to 20 JPEG, PNG or GIF images, one MP4 video or one PDF as a document, no threads), Facebook (63206
characters, up to 10 JPEG, PNG or GIF images or one MP4 or MOV video) and Instagram (2200
characters, 1 to 20 JPEG images or MP4 or MOV videos, `media_required` without one), YouTube
(one MP4 or MOV video, a title and a description, no threads), X (280 characters as X counts
them, up to 4 images or one GIF or one video, threads) and TikTok (2200 characters, one MP4 or MOV
video or 1 to 35 JPEG or WebP photos, `media_required` without one, no threads) publish today.
Saving with `scheduled_at` makes a post `scheduled`, else `draft`; only those two can change.
A tick every minute claims due posts (one instance at a time, advisory lock), creates one
target per account and claims each target once, so nothing posts twice; a publish is never
retried. A target cut off mid-publish for 31 minutes (longer than the slowest healthy post: 12 minutes of downloads, 14 of publishing, and a margin) fails as "unknown, check the account".

**Checks before saving.** `POST /posts`, `PUT /posts/{id}`, `POST /post-groups`, both retries
and the MCP tools that share them refuse a post that can never publish, before anything is
stored, with `422 {"error", "platform", "field"}` and, where the check knows them, `part` (the
1-based thread part), `limit`, `actual` and `detail`. Each media
URL and the cover are probed (a ranged GET of the first bytes through the same public-address
checks as publishing, Drive links included, all within 4 seconds) and typed by their magic bytes:
`media_type_not_supported` for a type the platform's publisher does not take (a PDF anywhere but
LinkedIn, a GIF on Instagram, Threads, TikTok or YouTube, a MOV on LinkedIn; a PNG or WebP on
Instagram and a PNG on TikTok pass, since publishing turns them into JPEGs),
`mixed_media` for a video next to other media on Bluesky, Facebook, LinkedIn, X or TikTok (a PDF
on LinkedIn, a GIF on X), `media_too_large` for a GIF over 1 MB on Bluesky or 15 MB on X, an animated WebP or PNG over Bluesky's 1 MB or X's 5 MB, a video over 1 GiB (100 MB on Bluesky, 512 MB on X), or any other file over 100 MB (a
still image over Bluesky's 1 MB or X's 5 MB passes, since publishing shrinks it), `invalid_cover_type` and `cover_too_large` for a
cover, and `cover_needs_one_video` when the one media item is not a video. A TikTok post's
`privacy_level` is checked against each TikTok account of the set (creator info, live, 5
seconds): `tiktok_privacy_not_offered` names the account and carries its
`privacy_level_options`. A probe or a TikTok call that gets no clear answer (unreachable, slow,
not 200 or 206, an unknown type, a daily limit) never refuses: publishing checks again. A retry
checks only a post with a failed target.

**Validate only.** `POST /v1/posts?validate_only=true` and `POST /v1/post-groups?validate_only=true`
take the create's body, run the create's own checks through the same code, create no post, post
group, list entry, media, idempotency record or webhook, and answer `200` (the TikTok privacy
check may renew and save a connected TikTok account's token, as on any save):

```json
{"ok": false,
 "platforms": [
  {"platform": "bluesky", "account_ids": ["..."], "ok": false, "problems": [
    {"code": "too_long", "field": "body", "limit": 300, "actual": 412, "detail": "..."},
    {"code": "too_many_images", "field": "media", "limit": 4, "actual": 7, "detail": "..."}]},
  {"platform": "facebook", "account_ids": ["..."], "ok": true, "problems": []}],
 "problems": []}
```

Every problem of every platform is listed, not only the first, in the order a create checks
them. `code` is the `error` a create answers. `field` is `platform`, `body`, `media`, `thread`,
`first_comment`, `cover`, `tiktok` or `youtube`; `part` is the 1-based thread part and absent
for the post itself; a key the check does not know is left out. `limit` and `actual` come with
`too_long` (in the platform's own unit, which `detail` names), `too_many_images` (media items),
`too_many_parts`, `first_comment_too_long`, `media_too_large` and `cover_too_large` (bytes);
other codes have none. `account_ids` are the set's accounts of that platform (none is not a
problem, as for a create). The top `problems` holds what is wrong with the request as a whole,
in the same shape: `invalid_set` and `no_accounts` (both leave `platforms` empty),
`invalid_override`, `too_many_posts` and a list's `not_found`, `invalid_position` or
`invalid_set`. The post cap and the list are checked whether or not the posts themselves are
valid, and the first such problem is listed. A post with `too_many_images` or `too_many_parts`
has none of its files fetched, so it lists no media type or size problem. `ok` is whether the same request without the flag would create every post. A
body that cannot be read is still `422 invalid_body`, and a flag other than `true` or `false`
`422 invalid_query`. The flag is read from the query only: a body with a `validate_only` key
(what the MCP tools take) is `422 validate_only_in_body` on both routes, whatever its value,
and creates nothing. A read-only token may send it (and is still refused the create), a set
token stays in its set, and an `Idempotency-Key` is ignored.

**Skip instead of refuse.** `POST /v1/post-groups` takes `"on_invalid": "refuse"` (the default:
one invalid post creates nothing) or `"skip"`: invalid platforms are left out, the rest are
created, and the `201` lists them in `skipped`, each `{"platform","account_ids","ok":false,
"problems":[...]}` as above (`skipped` is `[]` when nothing was left out). When every platform is
invalid, and for a problem of the request as a whole, `skip` answers the same `422` as `refuse`.
An idempotent replay answers the same body, `skipped` included. A response stored for replay
before this version replays as it was stored, without `skipped`.

**Video cover.** A post with exactly one media item, a video, may carry `cover`, a public https
link to a JPEG or PNG fetched at publish time like `media` (Google Drive share links included, same
public-address and magic-byte checks). Only three platforms have an API for it; any other answers
`422 cover_not_supported` (TikTok can only pick a frame, `video_cover_timestamp_ms`, so there is no
image cover there), a post without exactly one media item `cover_needs_one_video`, and a bad link
`invalid_cover`. Instagram: `cover_url` on the Reel's container, JPEG only, 8 MB. Facebook: after
the Reel is published, `POST /{video-id}/thumbnails` with `source` and `is_preferred=true`
(JPEG or PNG, 10 MB; the call Meta's Reels guide points to, whose page does not say it works for
Reels). YouTube: `thumbnails.set` after the upload (JPEG or PNG, 2 MB), which needs a verified
channel. A cover that cannot be downloaded or is the wrong type, or a media item that is not a
video, fails the target before anything is posted. On Facebook and YouTube the cover is set once
and never retried after the video is `published`: a refusal leaves the post published and shows
as the target's `cover_error`. A post group takes `cover` with a per-platform override
(`overrides.<platform>.cover`, `""` for none).

**First comment.** A post may carry `first_comment`, text its own account comments right after
the post is saved as published on that target, e.g. for sources and caveats. Only Facebook,
Instagram, LinkedIn and YouTube do; any other platform answers `422 first_comment_not_supported`
(like `thread_not_supported`), and a longer text `422 first_comment_too_long`. Limits, in
characters: Facebook 8000, Instagram 2200, LinkedIn 1250, YouTube 10000. None of the four
documents a number for comments through its API (Google only names `commentTextTooLong`), so these
are the widely reported figures, and a text over the real limit fails as a comment, never as a
post. Blank counts as none. A post group takes it at group level with a per-platform override
(`overrides.<platform>.first_comment`, `""` for none), so a group that includes Bluesky, X, Threads
or TikTok needs `""` there. A group without `platforms` covers every connected platform, so with
a `first_comment` it fails with `first_comment_not_supported` naming the first platform that
cannot take it: pass `platforms`. The comment is posted at most once per target and never retried, since
a retry could comment twice: the target claims it (`comment_status` `posting`) before the call,
then records `posted` with `comment_id` or `failed` with `comment_error`. A refused, timed out or
cut-off comment leaves the post and its target `published` (a cut-off one is failed by the same
sweep that recovers stuck targets, as "unknown, check the account"); the web app shows the error.
Nothing is posted if the target never reached `published`. A takedown deletes the post only: the
platform removes its comments with it. Calls (all with the account's own token): Facebook `POST
/{post-id}/comments` with `message` (the Page token, `pages_manage_engagement`), Instagram `POST
/{media-id}/comments` with `message` (`instagram_manage_comments`), LinkedIn `POST
/rest/socialActions/{post urn}/comments` with `actor` (the member), `object` and `message.text`
(plain text, not escaped; `LinkedIn-Version` `202610`), YouTube `POST
/youtube/v3/commentThreads?part=snippet` with the channel and video ids (`youtube.force-ssl`;
brain waits 10 seconds after the upload first, and a refusal such as `processingFailure` is the
comment's failure). LinkedIn's docs name `w_member_social_feed` for member comments while the
connector asks for `w_member_social`; if LinkedIn refuses, the comment error says so and the
scope needs a decision (it is a separate LinkedIn product permission). Webhook payloads are
unchanged.
Posts of a deleted set are hidden and held back until it is restored. Errors never carry URLs
or tokens. A team keeps at most 2000 posts that are not deleted; publish now and take-down are
10 a minute per user. A team publishes at most 100 posts a day, scheduled or now (`429
publish_quota` on publish now); a due post over the limit stays `scheduled` and goes out once
it frees up. A tick claims at most 50 posts; 8 posts and 8 targets publish at once. A past
`scheduled_at` means now.

A token created with `"read_only": true` (whole-team or set) only reads: every `POST`, `PUT`,
`PATCH` or `DELETE` it sends, and every `/mcp` write tool, is `403 {"error":"read_only_token"}`
before anything runs, except a create with `validate_only`, which creates nothing. `GET /v1/posts/{id}` lists the post's `accounts` (id, provider, handle,
display name, target id and status), also before it publishes and has targets.

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

Threads (when `THREADS_APP_ID` and `THREADS_APP_SECRET` are set) connects by OAuth. The web app
navigates to `GET /connections/threads/redirect`, which needs an owner or admin of the current
team (5 a minute per user) and sends the browser to `threads.com/oauth/authorize` with the
scopes `threads_basic`, `threads_content_publish`, `threads_manage_replies` and `threads_delete`. The attempt (the state's
SHA-256, the team and the user) is kept in Postgres for 10 minutes and used once; the browser
holds the state in an HttpOnly cookie scoped to `/connect/threads`, compared in constant time.
The callback needs the same signed-in user, still an owner or admin of that team, swaps the code
at `graph.threads.com/oauth/access_token` for a 60-day token (`th_exchange_token`), reads `me`
(id, username, name, picture) and saves a `threads` account in that team's Default set; a
reconnect updates the same row. It lands on `{WEB_URL}/app/connections?connected=threads`, or
`?connect_error=` `not_configured`, `unauthorized`, `forbidden`, `rate_limited`, `expired`,
`cancelled`, `threads_failed` or `internal` with `&provider=threads`, never with details. A daily
job extends tokens with 30 days or less left and not yet expired (`th_refresh_token`).

A Threads post is a container (`TEXT`, `IMAGE`, `VIDEO`, or `CAROUSEL` of item containers)
made with `POST {user}/threads`, polled until `FINISHED`, then published with
`threads_publish` after a 30-second wait (a finished text container still failed when published
sooner). Each thread part replies to the one before (`reply_to_id`). The permalink is looked up
after publishing; a failed lookup only leaves the link out. A takedown deletes each part, then
the root (`DELETE /v1.0/{id}`, 100 a day per profile). One post may take up to 6 minutes.
Threads downloads media itself, so brain hands it `{BRAIN_URL}/media/<sealed key>.<ext>`: the
stored file's key sealed with `ENCRYPTION_KEY`, served only while the file is stored (until the
post settles). `BRAIN_URL` must be reachable from the internet for Threads posts with media.

LinkedIn (when `LINKEDIN_CLIENT_ID` and `LINKEDIN_CLIENT_SECRET` are set) connects a member's
own profile through the same OAuth checks as Threads (state cookie `sr_linkedin`, scoped to
`/connect/linkedin`). The app needs the products Sign In with LinkedIn using OpenID Connect and
Share on LinkedIn. The callback swaps the code at `www.linkedin.com/oauth/v2/accessToken` for a
token (about 60 days), reads `GET /v2/userinfo` (`sub`, name, picture) and saves a `linkedin`
account (`external_id` is the `sub`, the author is `urn:li:person:{sub}`) in the team's Default
set. Only partner apps get a refresh token; when there is one it is sealed in the account's
`meta` and a daily job renews tokens with 14 days or less left. Without one the token simply
runs out: the account shows its `token_expires_at`, publishing fails with "Reconnect the account
under Connections" and the member signs in again. Error code `linkedin_failed` besides the
Threads ones.

A LinkedIn post (3000 characters, up to 20 JPEG, PNG or GIF images, one MP4 video, or one PDF; no
threads) goes out with the Posts API (`POST /rest/posts`, `LinkedIn-Version` `202610`, see
`connectors/linkedin.rs`): text only, one image as `content.media`, several as
`content.multiImage`, a video or a document as `content.media`. Images go through `POST /rest/images?action=
initializeUpload` and a `PUT` of the bytes; a video through the Videos API (initialize, a `PUT`
per part, finalize, poll until `AVAILABLE`). The text is escaped for LinkedIn's "little" format
(hashtags stay links). A PDF is a document (the swipeable carousel): `POST /rest/documents?action=
initializeUpload`, a `PUT` of the bytes, poll `GET /rest/documents/{urn}` until `AVAILABLE`, then
`content.media` `{id, title}`. The title is the body's first non-empty line cut to 100 graphemes,
else `Document`. LinkedIn takes up to 100 MB and 300 pages; brain does not count pages, so a file
LinkedIn refuses fails the target with LinkedIn's reason. A document post takes exactly one PDF:
a PDF with any other file fails the target ("A LinkedIn document post takes exactly one PDF and
nothing else."), as it is only known once the files are stored (types come from their first
bytes, not their names). Every other platform fails a PDF's target ("Image N is a PDF. Only
LinkedIn posts take a PDF, as a document."). Taking the post down deletes the post URN only; the
document asset stays. The post URN comes from the `x-restli-id` header and is saved first; the
permalink is `https://www.linkedin.com/feed/update/{urn}`. A takedown is `DELETE
/rest/posts/{encoded urn}`. brain uploads the stored bytes itself, so LinkedIn needs no public
media link.

YouTube (when `GOOGLE_CLIENT_ID` and `GOOGLE_CLIENT_SECRET` are set, the same Google client as
sign-in) connects one channel per consent through the same OAuth checks as Threads, with its own
redirect URI `{BRAIN_URL}/connect/youtube/callback`, state and cookie (`sr_youtube`, scoped to
`/connect/youtube`), so it never signs anyone in. Add the three YouTube scopes to the OAuth
consent screen and enable the YouTube Data API v3 in the Google Cloud project. The callback swaps
the code at `oauth2.googleapis.com/token`, needs the refresh token `prompt=consent` brings, reads
`GET /youtube/v3/channels?part=snippet&mine=true` (id, title, `@handle`, thumbnail) and saves a
`youtube` account (`external_id` is the channel id) in the team's Default set. Google always shows its
account chooser (`prompt=select_account consent`), which lists Brand Account channels too, so
pick the channel there; connect again for another. Error codes `youtube_failed`, `no_channel` (the Google account has no channel) and
`missing_scopes` (granular consent: the token Google returned lacks one of the three scopes, so
nothing is saved) besides the Threads ones. The access token lasts an hour and is stored with its expiry; the
refresh token is sealed in the account's `meta`. Before each publish or take down, a token with
under 20 minutes left is renewed and saved first, and one that ends during a long upload is renewed once and the upload goes on (there is no daily job). A refused refresh
token (revoked access) or a refused call (`401`, `insufficientPermissions`) fails the target with
"Reconnect the channel under Connections".

A YouTube post is one MP4 or MOV video (no threads; `media_required` without one,
`too_many_images` with several). The first line of the body is the title (at most 100
characters, required: `empty_post` without one) and the rest the description (at most 5000
bytes); `too_long` over either, and `invalid_text` for a `<` or `>`, which YouTube refuses. Shorts
are just vertical videos up to three minutes, so there is no separate call. brain downloads the
video into its storage (1 GiB at most) and uploads the bytes itself: `POST
/upload/youtube/v3/videos?uploadType=resumable&part=snippet,status` with the metadata
(`privacyStatus` from the post's settings, `selfDeclaredMadeForKids` `false`), then the bytes to the
session URL from the `Location` header, 8 MiB a `PUT` with its `Content-Range`, each read from storage as it goes (`308` until the last, which answers the video). brain sends the token only to a session URL that is https
on `googleapis.com`. The video id is saved the moment YouTube answers, before anything else; the
permalink is `https://www.youtube.com/watch?v={id}`; no chunk is sent again blindly: after one with no answer or a `5xx`, brain asks the session how far it got (`Content-Range: bytes */total`) and goes on from there, at most 4 times in a row without progress, so nothing posts twice. A YouTube
post may carry `youtube`: `{"privacy"}` (saved as `posts.youtube`, returned on the post), the
video's visibility: `private`, `unlisted` or `public`. Without `youtube`, or without `privacy`,
the video is `private`. Another value is `422 invalid_youtube_privacy`, and other platforms answer
`youtube_not_supported`. A group takes `youtube` for its YouTube post only, with an override
(`overrides.youtube.youtube`). Until Google audits the API project, YouTube locks every upload
from it to private whatever the post chose, so the video shows up private in YouTube Studio;
apply for the audit before relying on `unlisted` or `public`. A video upload costs
1,600 of the project's 10,000 daily quota units, shared by every team on the install, so about six
a day (a first comment costs 50 more, `commentThreads.insert`); when Google answers `quotaExceeded` the target fails with a message saying so. A takedown
is `DELETE /youtube/v3/videos?id={id}` (a video already gone counts as down; a channel connected
before `youtube.force-ssl` was requested must reconnect).

TikTok (when `TIKTOK_CLIENT_KEY` and `TIKTOK_CLIENT_SECRET` are set) connects an account through
the same OAuth checks as Threads, with its own redirect URI `{BRAIN_URL}/connect/tiktok/callback`,
state and cookie (`sr_tiktok`, scoped to `/connect/tiktok`). The callback swaps the code at
`open.tiktokapis.com/v2/oauth/token/`, needs the refresh token and all three scopes (error code
`missing_scopes` when the user unticks one; `tiktok_failed` besides the Threads codes), reads
`GET /v2/user/info/` (open id, name, picture) and the account's `@username` from the creator info
call, and saves a `tiktok` account (`external_id` is the open id) in the team's Default set. The
access token lasts a day and is stored with its expiry; the refresh token (a year) is sealed in the
account's `meta`. Before each publish, a token with under ten minutes left is renewed and saved
first under a lock on the account row, with the new refresh token saved in the same transaction (there is no daily job). A refused refresh
token or a refused call (`401`, `access_token_invalid`, `scope_not_authorized`) fails the target
with "Reconnect the account under Connections".

**TikTok settings.** TikTok's Content Sharing Guidelines for Direct Post make the user choose
how a post goes out, so a TikTok post may carry `tiktok`: `{"privacy_level","allow_comment","allow_duet","allow_stitch","commercial_content","your_brand","branded_content","ai_generated"}`
(saved as `posts.tiktok`, returned on the post; every switch defaults to off). `privacy_level` is
required, has no default, and must be one of the account's `privacy_level_options`
(`GET /v1/connections/{id}/tiktok`); brain checks the four known values on save
(`tiktok_privacy_required`, `invalid_tiktok_privacy`) and that the account is offered the chosen
one at publish, failing the target before anything is sent otherwise. Comment, duet and stitch map
to `disable_comment`, `disable_duet` and `disable_stitch`, and stay off when the creator turned
them off in TikTok; a photo post has no duet or stitch. The disclosure is `commercial_content`
with `your_brand` ("Promotional content", `brand_organic_toggle`), `branded_content` ("Paid
partnership", `brand_content_toggle`) or both: the switch needs one of them
(`tiktok_disclosure_incomplete`), the two need the switch (`invalid_tiktok_disclosure`), and branded
content cannot be `SELF_ONLY` (`tiktok_branded_content_private`). `ai_generated` sends `is_aigc`
(in `post_info` for a video, beside it for photos, as TikTok documents it). Other platforms answer
`tiktok_not_supported`. A group takes `tiktok` for its TikTok post only, with an override
(`overrides.tiktok.tiktok`). The video length against `max_video_post_duration_sec` is not
checked by brain (it does not read the video); TikTok refuses a longer one.

A TikTok post is one MP4 or MOV video, or 1 to 35 JPEG or WebP photos (no threads, no text-only
posts: `media_required` without media; the caption is the body, at most 2200 UTF-16 code units, so an emoji counts two). brain
does not upload: TikTok downloads the media from brain's unguessable `/media/` links
(`PULL_FROM_URL`, so the media domain must be verified under the TikTok app's URL properties).
The post is `POST /v2/post/publish/video/init/` (or `content/init/` for photos, with the first
line of the body as the photo title) as a direct post, then the publish status is asked every
three seconds for up to 4.5 minutes until TikTok says `PUBLISH_COMPLETE` (or `FAILED`, with its
reason); a status call that errors is asked again, since the post already exists, and after the
4.5 minutes the target fails with "check the account before posting again". A post without `tiktok` settings (made through the API) goes out with the most
public privacy level the account is offered and comments, duets and stitches off. Until TikTok audits the app, it only lets it post privately
(`SELF_ONLY`) and from private accounts: when TikTok refuses a level for that reason and offers
`SELF_ONLY`, the init is asked again with it (the refused one made no post); the video then has
no public id and no link (a branded content post is never asked again, it cannot be private). The saved
id is the publish id; the permalink is `https://www.tiktok.com/@{username}/video/{id}` when the
video is public. TikTok gives apps no way to delete a post, so a take down fails with a message
that says to delete it in the TikTok app and the post stays `published`.

X (when `X_CLIENT_ID` and `X_CLIENT_SECRET` are set) connects an account through the same OAuth
checks as Threads (state cookie `sr_x`, scoped to `/connect/x`), as a confidential Web App client
with PKCE. The app needs User authentication settings with read and write permission and the
redirect URI above. `GET /connections/x/redirect` makes a fresh code verifier and keeps it with
the state, sealed with `ENCRYPTION_KEY` like the tokens (the URL carries only its S256
challenge). The callback swaps the code and the verifier at `POST api.x.com/2/oauth2/token` with
the client id and secret in HTTP Basic auth, reads `GET /2/users/me` (id, username, name,
picture) and saves an `x` account (`external_id` is the user id, `handle` the username) in the
team's Default set. Error codes `x_failed` and `missing_scopes` (the consent screen let the user
untick a scope, so nothing is saved) besides the Threads ones.

An access token lasts two hours and is stored with its expiry; the refresh token is sealed in the
account's `meta`. X refresh tokens are single use, so renewing locks the account row
(`SELECT ... FOR UPDATE`) and saves the new access and refresh token in the same transaction: two
publishes renewing at once call X once and both use the result. A response without a new refresh
token keeps the current one. A token with under 15 minutes left is renewed before a publish, before
each part of a thread and before each delete of a take down (a post can take minutes, so a long
run never reuses a token that ran out), and a job every 15 minutes (one instance at a time, advisory lock) renews tokens with
30 minutes or less left. A refresh X refuses (revoked access, a spent token) sets `meta.reconnect`;
the account then fails every publish with "Reconnect the account under Connections" without
calling X, `GET /v1/connections` shows `reconnect: true`, and connecting it again clears the flag.
A network or server error while renewing changes nothing. If the process dies between X's answer
and the commit, the new refresh token is lost and the account asks for a reconnect.

An X post (280 weighted characters, up to 4 JPEG, PNG or WebP images, or one GIF, or one MP4 or
MOV video, not a mix; threads of up to 25 follow-up posts) goes out with API v2
(`connectors/x.rs`). Length is counted the X way (`socialreins_core::post::x_weight`): most
Latin, Greek and Cyrillic characters count 1, CJK and emoji 2, every link 23. The count is
conservative: a bare domain counts as a link even where X would not, so a post may be refused
(`too_long`) that X would take, never the reverse. Media is uploaded first with
`POST /2/media/upload/initialize` (`media_type`, `total_bytes`, `media_category` `tweet_image`,
`tweet_gif` or `tweet_video`), one `POST .../{id}/append` per 4 MB chunk (multipart, `segment_index`
and `media`), `POST .../{id}/finalize`, and `GET /2/media/upload?command=STATUS` until a video is
processed; images are at most 5 MB (a larger still image is shrunk first), a GIF 15 MB, a video 512 MB, each chunk read from storage as it goes. Then
`POST /2/tweets` with `text`, `media.media_ids` and, for a thread part, `reply.in_reply_to_tweet_id`
set to the part before it. Each tweet id is saved the moment X answers, before the next part is
sent; a part that fails keeps the ones before it up, and nothing is retried. The permalink is
`https://x.com/{username}/status/{id}`. A takedown is `DELETE /2/tweets/{id}` per part, last
first, then the root (a tweet already gone counts as down). X bills API use by credits: when X
answers `402`, or an error whose type, title or detail speaks of credits, the target fails with
"X says this app is out of API credits ... Add credits in the X developer console". A `429`
fails it with the reset time from `x-rate-limit-reset` ("resets at 2026-11-04 12:30 UTC. It was
not retried."), and a `429` of type `usage-capped` says the usage cap is reached. A `403` shows X's
own reason (for example duplicate content). A `401` is a dead token and asks for a reconnect.

Meta (when `META_APP_ID` and `META_APP_SECRET` are set) connects Facebook Pages and the
Instagram professional accounts linked to them with one Facebook Login, through the same OAuth
checks as Threads (state cookie `sr_meta` scoped to `/connect/meta`). The dialog is
`www.facebook.com/{META_GRAPH_VERSION}/dialog/oauth` (default `v26.0`) with the publishing scopes
(`pages_show_list`, `pages_read_engagement`, `pages_manage_posts`, `business_management`,
`instagram_basic`, `instagram_content_publish`, `instagram_manage_contents`) and those comment
automations will need. The callback swaps the code for a short-lived user token, then a
long-lived one (`fb_exchange_token`), and reads `me/accounts` (up to 1000 Pages, following the
cursor). Accounts the team already has get the new Page token at once. When anything is left,
the Pages with their tokens are sealed with `ENCRYPTION_KEY` for 15 minutes (one pick per user
and team) and the browser lands on `?pick=meta`, where web lists them from
`GET /v1/connections/meta/pick` and connects the chosen ones with `POST`. A Page becomes a
`facebook` account holding the Page token; an Instagram account becomes an `instagram` account
holding its Page's token with `meta.page_id`. Both have no expiry: Page tokens from a long-lived
user token do not expire, so there is no refresh job, and reconnecting renews them. Errors:
`meta_failed` and `no_pages` besides the Threads codes.

Facebook posts go to the Page: text to `{page}/feed`, one image to `{page}/photos`, several as
unpublished photos attached to one feed post, one video as a Reel (`video_reels` start, upload
from its URL to `rupload.facebook.com`, poll, finish); a video cannot be mixed with other media.
Instagram posts are a container (`image_url`, `REELS` for one video, a `CAROUSEL` of `IMAGE`
and `VIDEO` items for several), polled until `FINISHED`, then `media_publish`; the permalink is
looked up after. Both fetch media from brain's `/media/` link, take no thread parts
(`thread_not_supported`), and come down with `DELETE /{id}` on the Page token. One post may take
up to 7 minutes.

Meta's inbox (`POST /webhooks/meta`, `app/src/meta_inbox.rs`): Facebook Page comments (`feed`,
`item=comment`, `verb=add`, replies to comments included) and Messenger messages
(`entry.messaging`), and Instagram `comments` and `messages`, are stored in `inbox_items` for
every connected account named by the entry (a Page id for `facebook`, the Instagram account id
for `instagram`), in every team that connected it. The unique key is the account and the
comment or message id, so a repeated delivery adds nothing. Skipped: unknown accounts, what the
account wrote itself (its id is the entry's, or `is_echo`), and events that are not comments or
messages. A comment's `parent_remote_id` is the post (or the comment it answers) and
`root_remote_id` the post, so conditions and web can tell which post; a DM's `convo_id` is the
sender's id. Names, the Instagram link and automations come after the `200`: a DM sender's
profile is read once, an Instagram comment's link is the media permalink, and new comments run
the team's automations. Connecting (and reconnecting) a Page calls `POST {page}/subscribed_apps`
with `subscribed_fields=feed,messages` (`pages_manage_metadata`); a refusal never fails the
connect, it is logged and kept as `webhook_error` on the connection. Disconnecting the last
account of a Page (in any team) unsubscribes it, best effort. Replies: a comment gets a public
reply (`{comment}/comments` on Facebook, `{comment}/replies` on Instagram), a DM item a message
from the Page (`{page}/messages`, `recipient.id`, `messaging_type=RESPONSE`, within 24 hours of
the person's last message; Instagram goes through its linked Page). An automation's `send_dm`
on a comment is Meta's private reply (`recipient.comment_id`): within 7 days of the comment and
once per comment. Neither is ever retried. Reasons not sent: `outside_window`, `already_replied`,
`dm_unavailable` (these skip the DM and let the fallback public reply go), `rate_limited`.
Permissions: `pages_manage_metadata` (subscribe), `pages_read_user_content` and
`pages_read_engagement` (comments), `pages_manage_engagement` (Facebook public reply),
`pages_messaging` (Messenger messages and private replies), `instagram_manage_comments`
(Instagram comments and replies), `instagram_manage_messages` (Instagram messages).

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
`group_id` ties the posts of one `POST /v1/post-groups` call and is `null` for a single post. To verify a delivery, compute HMAC-SHA256 with
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
| `REGISTRATION_DISABLED` | no | `false`. `true` closes sign-up: only `OWNER_EMAIL` and invited emails can sign up (with no `OWNER_EMAIL`, the first account to sign up, then invited emails); existing users still sign in. Replaced by rules saved in the admin console (see sign-up rules) |
| `SIGNUP_DOMAINS` | no | Comma-separated email domains and email addresses, e.g. `harun.dev,socialreins.com,jane@example.com` (trimmed, any case). When set, a new account's email domain must be one of the domains exactly (a subdomain does not count) or its whole address one of the emails; `OWNER_EMAIL` and invited emails are exempt, existing users still sign in, and anyone else who signs up goes on the waitlist (see sign-up rules). An entry with an `@` must be a valid email address, any other must be a plain domain, else startup fails. Unset or blank: anyone. Replaced by rules saved in the admin console (see sign-up rules) |
| `OWNER_EMAIL` | no | With `REGISTRATION_DISABLED=true`, the one new email allowed to sign up besides invited ones, any case. Recommended for personal installs: otherwise whoever signs up first owns the install |
| `RESEND_API_KEY` | no | Send mail through the Resend HTTPS API (works where outbound SMTP is blocked, like Railway) |
| `SMTP_URL` | no | Send mail through any SMTP server, e.g. `smtps://user:pass@smtp.example.com`. Used when `RESEND_API_KEY` is unset |
| `MAIL_LOG` | if no mail | `false`. With no Resend key and no SMTP, brain refuses to start unless this is `true`; then sign-in links are written to the log at info level. Meant for a single-user self-host |
| `MAIL_FROM` | with mail | Sender, e.g. `SocialReins <login@example.com>` |
| `RAILWAY_GIT_COMMIT_SHA` | no | Commit `/version` reports (Railway sets it) |
| `GIT_COMMIT` | no | Used when `RAILWAY_GIT_COMMIT_SHA` is unset; baked into published images. Else `dev` |
| `RUST_LOG` | no | `info` |
| `META_APP_ID`, `META_APP_SECRET` | no | Both to connect Facebook Pages and Instagram accounts (redirect URI `{BRAIN_URL}/connect/meta/callback`). Publishing with connected accounts needs neither |
| `META_GRAPH_VERSION` | no | `v26.0`. The Graph API version for Meta calls |
| `META_WEBHOOK_VERIFY_TOKEN` | no | Any secret string, typed into the Meta app's webhook setup too. Turns on `GET /webhooks/meta`; `POST /webhooks/meta` is signed with `META_APP_SECRET` |
| `THREADS_APP_ID`, `THREADS_APP_SECRET` | no | Both to connect Threads accounts (redirect URI `{BRAIN_URL}/connect/threads/callback`). Publishing with connected accounts needs neither |
| `LINKEDIN_CLIENT_ID`, `LINKEDIN_CLIENT_SECRET` | no | Both to connect LinkedIn profiles (redirect URI `{BRAIN_URL}/connect/linkedin/callback`). Publishing with connected accounts needs neither |
| `X_CLIENT_ID`, `X_CLIENT_SECRET` | no | Both to connect X accounts (a Web App client, redirect URI `{BRAIN_URL}/connect/x/callback`). Publishing with a live token needs neither, renewing one does |
| `TIKTOK_CLIENT_KEY`, `TIKTOK_CLIENT_SECRET` | no | Connect TikTok accounts (redirect URI `{BRAIN_URL}/connect/tiktok/callback`) |
| `GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET` | no | Both or neither. Turn on Google sign-in (OAuth client of type Web application, redirect URI `{BRAIN_URL}/auth/google/callback`). They also turn on connecting YouTube channels (add the redirect URI `{BRAIN_URL}/connect/youtube/callback` to the same client) |
| `TURNSTILE_SITE_KEY`, `TURNSTILE_SECRET_KEY` | no | Both or neither. Turn on a Cloudflare Turnstile captcha: `POST /auth/link` and `/v1/contact` then need a `turnstile_token`. Off when unset |
| `ADMIN_EMAILS` | no | Comma-separated emails, any case, whose signed-in users may use the admin console, `/v1/admin/*` (and get `is_admin: true` from `/auth/me`). None when unset |
| `REVIEW_LOGIN_TOKEN` | no | Turns on `GET /review-login/{token}`, a sign-in to a demo account for Meta App Review. Unset (the default) the route is a `404`. Remove it after the review: see [App Review login](#app-review-login) |
| `MEDIA_DIR` | no | `./media` (`/var/lib/brain/media` in the image). Where post media is kept while a post publishes, when no bucket is set. Files go once the post settles, so an ephemeral disk is fine. A download cut off by a restart leaves a temp file, which the hourly sweep removes once it is an hour old. Assumes a single brain instance: with more than one replica, use the `S3_*` bucket |
| `S3_BUCKET`, `S3_ENDPOINT`, `S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY` | no | All or none. Keep post media in an S3-compatible bucket instead of `MEDIA_DIR`, path-style. Cloudflare R2: `S3_ENDPOINT=https://<account>.r2.cloudflarestorage.com`; AWS S3: `https://s3.<region>.amazonaws.com` with `S3_REGION`. Files are written as multipart uploads, which brain aborts when a download fails; give the bucket a lifecycle rule that aborts incomplete multipart uploads after a day (R2 does so after 7 days by default) for the ones a restart cuts off |
| `S3_REGION` | no | `auto` (what R2 wants) |
| `MEDIA_MAX_BYTES` | no | `1073741824` (1 GiB), which is also the most. The largest video a post may carry, in bytes. Videos are streamed to and from storage, so this is disk or bucket space, not memory. It caps videos only, whatever its value: images and PDFs stay at 100 MB. A video over it is refused when the post is saved (`422 media_too_large` with `limit` and `actual`) where its server says its size, else when it publishes |
| `CONTACT_TO` | no | Turn on `POST /v1/contact`, which mails each message to this address (Reply-To: the sender). Off when unset |

## App Review login

Meta App Review needs a reviewer to sign in without receiving email. To allow it:

1. Set `REVIEW_LOGIN_TOKEN` to a long random value (`openssl rand -hex 32`) and redeploy.
2. Give the reviewer `{BRAIN_URL}/review-login/<token>`. Opening it signs the browser in to the
   demo user `review@socialreins.com` ("App Review Demo", the owner of its own team with the
   default social set, nothing connected) and lands on `{WEB_URL}/app`. The reviewer connects
   their own test accounts from there.
3. Once the review is over, remove `REVIEW_LOGIN_TOKEN` and redeploy: the route is a `404` again.
   The demo user and its data stay until you delete them.

A wrong token and an unset one both give the same bare `404`, and the token is compared in
constant time and never logged. The route shares the per-IP sign-in rate limit. The demo user
signs in even with `REGISTRATION_DISABLED=true`, is never an admin, and brain refuses (with a
warning in the log) if `review@socialreins.com` is listed in `ADMIN_EMAILS`. Anyone holding the
link has full use of the demo account, so treat it like a password.

## Media

A post's `media` are URLs: public `https://` links or Google Drive share links
(`/file/d/<id>/view`, `open?id=`, `uc?id=`; the file must be shared with anyone with the link).
Nothing is fetched when a post is saved. When it publishes, brain downloads each URL once into
its storage (`MEDIA_DIR` or the `S3_*` bucket): names must resolve to public addresses, at most 3
redirects, each to a public https URL, 4 at a time, streamed to storage in 5 MB parts (a download
holds about 15 MB however large the file) and typed by its first bytes. A video may be up to 1 GiB
(`MEDIA_MAX_BYTES` lowers it; Bluesky takes 100 MB and X 512 MB), any other file 100 MB; a file
over its cap is refused by its `Content-Length`, else the moment its bytes pass it, and nothing of
it stays stored. Each download has 60 seconds plus its size at 2 MB a second (about 3.5 minutes
for 300 MB; when its server does not say its size, the size is the bytes that came so far, so a slow one ends early), all of a post's 12 minutes together, and a server silent for 30 seconds fails at
once. Posts of one group that carry the same video download it once: the others copy it inside
the storage (per brain instance). Types are JPEG, PNG, WebP, GIF, MP4, MOV and PDF (only LinkedIn
takes a PDF). A still image (JPEG, PNG or WebP) over the platform's limit for one image (Bluesky
1 MB, X 5 MB; `post::IMAGE_LIMITS`) is re-encoded as a JPEG that fits: quality 90, 80, then 70 at
its own size, then scaled down at quality 80, its shape kept (Bluesky: longest side at most 2000
px), transparency on white, turned upright by its EXIF orientation, its RGB colour profile kept.
One within the limit keeps its bytes. A GIF is never re-encoded, nor an animated WebP or PNG, and
an image that cannot be shrunk (those, one that does not decode or is over 25 megapixels) fails
its target with the reason. Platforms are then served from the stored copy: LinkedIn, YouTube, X and Bluesky get a video read
from storage a part at a time, and Threads, Facebook, Instagram and TikTok fetch it from its
`/media/` link, which streams it and honours `Range` (`206` with `Content-Range`, `416` past the
end). A publish with a video gets more time: its platform's limit plus the bytes at 2 MB a second
and 50 seconds per 100 MB, 14 minutes at most, and the platform's processing poll one more check
per 100 MB. A URL that fails fails its
target naming the image, before anything goes out. A post's files are deleted once it settles
(published, failed or deleted); an hourly sweep deletes any left a week later whose post no
longer waits to publish, whether or not a row still names it. Local-disk storage assumes one
brain instance; run more replicas only with an S3-compatible bucket.

## Deploy

Railway builds the `Dockerfile` (see `railway.json`) and health-checks `/health`. Migrations
run when the server starts; applied ones are recorded and skipped, so restarts and upgrades are
safe.

Self-hosters use the published image, `ghcr.io/socialreins/brain` (`linux/amd64` and
`linux/arm64`): `:main` and `:<commit sha>` follow `main`, `:vX.Y.Z` and `:latest` follow
releases (`.github/workflows/image.yml`). The install guide lives in
[socialreins/socialreins](https://github.com/socialreins/socialreins#self-host).

## License

[AGPL-3.0](LICENSE).
