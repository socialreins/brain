<p align="center"><img src="https://raw.githubusercontent.com/socialreins/socialreins/main/brand/logo.svg" width="96" height="96" alt="SocialReins logo"></p>

<h1 align="center">brain</h1>

The Rust backend for [SocialReins](https://socialreins.com), a social media manager for
scheduling and publishing posts across platforms. For the project overview, roadmap and the
other repos, see the main repo [socialreins/socialreins](https://github.com/socialreins/socialreins).

brain is the whole backend: sign-in (email link, passkey, Google), teams and invitations,
connections to Facebook, Instagram, Threads, LinkedIn, Bluesky, X, TikTok, YouTube and Pinterest, and the
workers that publish and schedule posts and take them down where the platform allows it. It serves the REST API under `/v1`, the
MCP server at `/mcp` for AI assistants, and the waitlist behind the socialreins.com landing
page. To install it or connect an assistant, start with the
[docs](https://socialreins.com/docs).

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
| POST | `/mcp` | MCP server (streamable HTTP, stateless): tools for sets, posting queues, accounts, posts (with `approve-post`, `reject-post`, `set-post-repeat`, `clear-post-repeat` and `takedown-post-group`, write tools), lists, the media library (`list-media`, `add-media`), feeds (`list-feeds`, `add-feed`, `remove-feed`), analytics (`get-analytics`, a read tool) and snippets (`list-snippets`, `save-snippet` creating or, with an `id`, replacing, `delete-snippet`). Bearer API token only, same scope and per-token limit as `/v1`, bodies up to 64 KiB; `Idempotency-Key` does not apply. `create-post` and `create-post-group` take `validate_only` (see "Validate only"), which a read-only token may send |
| POST | `/v1/waitlist` | `{"email":"...","source":"..."}` joins the waitlist (unverified) and emails a confirmation link; answers `{"status":"ok","name_token"}` |
| POST | `/v1/waitlist/name` | `{"token":<name_token>,"name"}` adds the joiner's optional name (1 to 80 characters, no control, invisible or direction-changing characters) within an hour of joining and until the email is verified; `200 {"status":"ok"}` for any well-formed token, `422 invalid_link` or `invalid_name` |
| POST | `/v1/waitlist/verify` | `{"token"}` from the emailed link: marks the signup verified, `200 {"status":"verified"}`, else `422 {"error":"invalid_link"}` |
| GET | `/v1/waitlist/count` | `{"count":n}`: verified signups only, cached for 30 seconds |
| GET | `/v1/admin/waitlist` | `ADMIN_EMAILS` only, session cookie: `{"total","verified","signups":[{"id","email","source","name","created_at","verified_at","approved_at"}]}`, newest first, at most 1000 (`?verified=true\|false`, `?hide_stale=true` (leaves out unverified, unapproved signups older than 30 days) and `?q=` (part of the email, any case) filter the list) |
| GET | `/v1/admin/overview` | Admin console (every `/v1/admin/*` route: `ADMIN_EMAILS` only, session cookie). Totals: `{"users","new_users_7d","teams","accounts":{"total","by_provider":{"bluesky":n}},"posts":{"scheduled","published","failed",...},"waitlist":{"total","verified","approved"}}` |
| GET | `/v1/admin/users` | `?q=` (part of the email or name, any case), `?limit=` (1 to 200, default 50), `?offset=`: `{"total","users":[{"id","email","name","is_admin","created_at","last_sign_in_at","teams":[{"id","name","role"}]}]}`, newest first. `last_sign_in_at` is the start of the newest session still stored, else `null` |
| GET | `/v1/admin/teams` | `?q=` (part of the name, any case), `?limit=`, `?offset=`: `{"total","teams":[{"id","name","created_at","members","accounts","posts","owner_email"}]}`, newest first; `posts` leaves out deleted ones |
| POST | `/v1/admin/teams/{id}/credits` | Admin console. `{"kind":"x"\|"ai","amount":1 to 1000000}` adds credits by hand (each call adds again; sign-in from the last 10 minutes): `201 {"balance"}`. Owners can also buy packs (`/v1/team/billing/credits`). |
| PUT, DELETE | `/v1/admin/teams/{id}/plan` | Admin console, sign-in from the last 10 minutes. PUT `{"plan":"scheduler"\|"automation"}` gives the team a comped plan (free, no Stripe, no end; `plan.comped` in `/v1/team/billing`), `409 has_paid_plan` over a live paid plan, `422 invalid_plan`. DELETE ends a comped plan (`404` when there is none). Every team that existed at migration `m20261214_000001_comp_plans` got a comped Scheduler and Automation plan, so switching billing on locks no early team out. |
| POST | `/v1/admin/waitlist/{id}/approve` | Approves a signup: its email may sign up whatever the sign-up rules say and is mailed a link to `WEB_URL/signup`. `{"signup":{...},"mail":"sent"\|"failed"\|"already_approved"}`; a second call sends nothing, a failed mail keeps the approval. `404 not_found` |
| DELETE | `/v1/admin/waitlist/{id}` | Removes a signup (and its approval, if it has not signed up yet), `204`. `404 not_found` |
| GET, PUT | `/v1/admin/settings/signup` | The sign-up rules: `{"mode":"open"\|"closed"\|"domains","domains":[...],"source":"env"\|"saved"}`. `PUT {"mode","domains"}` saves them (sign-in from the last 10 minutes, else `403 reauth_required`): at most 50 domains or email addresses, trimmed, lowercased, deduplicated, at least one for `domains` (`422 invalid_mode`, `invalid_domains`) |
| POST | `/v1/contact` | `{"name","email","message"}` mails `CONTACT_TO`, `202 {}` |
| POST | `/auth/link` | `{"email":"...","name":"..."}` emails a sign-in link, `202 {"status":"sent"}`. `name` is optional, 1 to 80 characters (else `422 invalid_name`), and only names an account this link creates. With `"login_only":true`, an email without an account is sent a pointer to sign up instead of a link |
| POST | `/auth/link/me` | Session cookie. Emails a sign-in link to the signed-in user's own address, `202 {"status":"sent"}`. No body and no captcha; opening the link starts a fresh session, for actions that answer `403 reauth_required` |
| GET | `/auth/magic/{token}` | The emailed link: a page with a sign-in button, does not use the link |
| POST | `/auth/magic` | Form field `token`: uses the link, sets the session cookie, redirects to `WEB_URL` |
| GET | `/auth/me` | The signed-in user with `current_team` (the team as `GET /v1/team` gives it) and `is_admin` (in `ADMIN_EMAILS`), or `401 {"error":"unauthorized"}`. When the request names a team (see "Team codes"), that team is returned, and with the `X-Team` header it becomes the current team |
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
| POST, GET | `/v1/teams` | Create a team (`{"name"}`, with its default set, named "All sets"; caller owns it and switches to it), or list mine |
| PUT | `/v1/current-team` | `{"team_id"}`: switch to one of my teams |
| GET, PATCH, DELETE | `/v1/team` | The current team (`{"id","name","code","slug","public_pages","newsletter","newsletter_sender","require_approval","role"}`), or the one the request names; rename it, set or clear its slug, or switch its public pages, their newsletter sign-up or post approvals (`{"name","slug","public_pages","newsletter","newsletter_sender","require_approval"}`, any of them; owner, admin; see "Team slugs" and "Approvals"). `newsletter_sender` is who the team's newsletters are from, `{"name": 1 to 80 characters on one line without `@`, `<`, `>`, `:` or `/`, "address": the postal address the law wants under every newsletter, 10 to 300 characters, line breaks allowed, "logo": optional, the team's logo above every newsletter in place of the name, a public `https` image link of at most 2000 characters with a host and no user name or password; `null`, `""` or leaving it out means no logo}` or `null` to clear it (`422 {"error":"invalid_sender","field"}`, `field` being `name`, `address` or `logo`). The whole object is replaced, so send `logo` with every change that should keep it; the reply shows it as stored (`"logo": null` without one). The logo is never fetched here, only put in the mail as an image; no newsletter is tested or sent without it, and changing it clears `tested_at` of the team's drafts; delete it (owner, sign-in from the last 10 minutes) |
| GET | `/v1/team/members` | The current team's members |
| PATCH, DELETE | `/v1/team/members/{user_id}` | `{"role"}`: change a role (owner, admin); remove a member, or leave |
| POST, GET | `/v1/team/invitations` | `{"email","role"}`: invite as `admin` or `member` (owner, admin); list pending |
| DELETE | `/v1/team/invitations/{id}` | Withdraw an invitation (owner, admin) |
| GET, PUT, DELETE | `/v1/team/webhook` | The team's outbound webhook (owner, admin): read it (no secret, `404` when none); `{"url"}` sets it (sign-in from the last 10 minutes), `201` with the `secret` once when new, `200` when changing the URL; delete it, `204` |
| GET | `/v1/team/billing` | Any member. The team's plan (`null` when none), what it allows (`limits`, `null` is unlimited) and what it uses. `enabled: false` on a self-hosted install, where nothing is capped (`BILLING_ENABLED`). With billing on and no live plan, nothing is allowed. Read-only. Where the limits bite, the answer is `402 {"error":"plan_limit","limit":"accounts"|"members"|"automation"|"inboxes"}`: connecting a new account (an OAuth connect that would pass it redirects with `connect_error=plan_limit`), inviting or accepting a member (open invitations hold a seat), creating or sending automations, broadcasts and newsletters, or enabling an automation (`automation`), and creating an inbox past the plan's `inboxes` (Scheduler 1, Scheduler and Automation 10; "All" never counts, a downgrade keeps the inboxes made). `usage.inboxes` counts them. Every plan reads and replies in the inbox. A team without a live plan (`trialing`, `active`, or `past_due` while the payment is retried) publishes nothing: due posts stay scheduled with the reason and go out once the team pays, and publishing now or retrying answers `402` with `limit: "plan"`. Existing automations keep running; contact limits are not enforced yet. |
| GET | `/v1/team/credits` | Any member. The team's prepaid credits: `x` (posts to X) and `ai` (AI calls on our key). |
| POST | `/v1/team/billing/checkout` | Owner, sign-in from the last 10 minutes. `{"plan":"scheduler"\|"automation","interval":"year"\|"month"}` (`interval` defaults to `year`): `{"url"}`, a Stripe Checkout page for a subscription (the team's first has a 7-day trial; `client_reference_id` and the subscription metadata carry the team id). `409 already_subscribed` while a plan is live, `422 invalid_plan`, `503 billing_unavailable` (billing off or Stripe unset), `502 stripe_failed` |
| POST | `/v1/team/billing/portal` | Owner, same checks. `{"url"}`, the Stripe customer portal (card, plan, cancel). `409 no_customer` before a subscription |
| POST | `/v1/team/billing/credits` | Owner, same checks. `{"pack":"x100"\|"x500"}` (100 or 500 X posts, never expire): `{"url"}`, a one-time Stripe Checkout. The credits are granted by the webhook once paid. `422 invalid_pack`, `503 billing_unavailable` also when `STRIPE_PRICE_X100`/`X500` are unset |
| POST | `/webhooks/stripe` | Stripe's events, signed (`Stripe-Signature`, 5 minute tolerance, `404` while `STRIPE_WEBHOOK_SECRET` is unset). `customer.subscription.created`, `.updated`, `.deleted` set the team's plan; `checkout.session.completed` and `.async_payment_succeeded` of a paid credit pack grant its credits once (the session id is the ledger key); `charge.refunded` and `charge.dispute.created` are logged as errors for a person (credits are not taken back automatically). The owners are mailed on `customer.subscription.trial_will_end`, the first `invoice.payment_failed` of an invoice and when a live plan ends. Each event id is taken once, and an event older than the last one applied is dropped. Register `{BRAIN_URL}/webhooks/stripe` in Stripe with those nine events |
| GET, PUT, DELETE | `/v1/team/ai-key` | The team's own AI provider key (BYOK; owner, admin). `{"provider","key"}` sets it (`anthropic` key `sk-ant-…` or `openai` key `sk-…`; `422 invalid_key` on a wrong shape; sign-in from the last 10 minutes); read answers the provider and the last 4 characters only, `404` when none; delete it, `204`. Sealed at rest. AI steps in automations use it before any credit is spent. |
| POST | `/v1/team/webhook/test` | Sends a signed `ping` now: `{"status"}` from the receiver, or `{"error"}` (`blocked_address`, `timeout`, `unreachable`) |
| GET, POST | `/v1/invitations/{token}` | Public preview of an invitation; accept it as the signed-in invited email |
| GET | `/v1/connections` | The current team's connected accounts with their `set_ids`; never tokens |
| GET | `/v1/connections/providers` | Each provider with `configured`: Bluesky always, the others once their app id and secret are set |
| POST | `/v1/connections/bluesky` | `{"handle","app_password"}` (owner, admin): checks it on the account's PDS, stores it encrypted, joins the default set; `201`, or `200` when it replaces the password of an account the team already has |
| GET | `/connections/threads/redirect` | A page navigation from web (session cookie; owner, admin of the current team): sends the browser to Threads to connect a profile. Lands back on `{WEB_URL}/app/connections` |
| GET | `/connect/threads/callback` | Where Threads sends the browser back (register `{BRAIN_URL}/connect/threads/callback` in the Threads app); lands on `{WEB_URL}/app/connections?connected=threads` or `?connect_error=<code>&provider=threads` |
| GET | `/connections/linkedin/redirect` | Like the Threads one: sends the browser to LinkedIn (scopes `openid profile email w_member_social`) to connect a member's profile |
| GET | `/connect/linkedin/callback` | Where LinkedIn sends the browser back (register `{BRAIN_URL}/connect/linkedin/callback` in the LinkedIn app); lands on `{WEB_URL}/app/connections?connected=linkedin` or `?connect_error=<code>&provider=linkedin` |
| GET | `/connections/youtube/redirect` | Like the Threads one: sends the browser to Google (scopes `youtube.upload`, `youtube.readonly`, `youtube.force-ssl`; `access_type=offline`, `prompt=consent`, `include_granted_scopes=false`) to connect a YouTube channel |
| GET | `/connect/youtube/callback` | Where Google sends the browser back (register `{BRAIN_URL}/connect/youtube/callback` in the Google OAuth client, besides the sign-in one); lands on `{WEB_URL}/app/connections?connected=youtube` or `?connect_error=<code>&provider=youtube` |
| GET | `/connections/x/redirect` | Like the Threads one, with PKCE: sends the browser to `x.com/i/oauth2/authorize` (scopes `tweet.read tweet.write users.read offline.access media.write`, `code_challenge_method=S256`; with `?inbox=1` also `dm.read dm.write like.write tweet.moderate.write` for the inbox, which the X app must allow (Read, write and Direct message) and a person may untick without losing publishing) to connect an X account |
| GET | `/connect/x/callback` | Where X sends the browser back (register `{BRAIN_URL}/connect/x/callback` in the X app); lands on `{WEB_URL}/app/connections?connected=x` or `?connect_error=<code>&provider=x` |
| GET | `/v1/connections/{id}/tiktok` | What a connected TikTok account may post now (any member; a set token only for its set's account): `{"nickname","username","privacy_level_options","comment_disabled","duet_disabled","stitch_disabled","max_video_post_duration_sec","can_post","reason"}`, read live from TikTok's creator info. `can_post` false (a daily limit, a ban, the app's user cap, no level offered) comes with `reason`, still `200`. `409 reconnect_required` for a dead token, `502 tiktok_failed` when TikTok cannot be reached |
| GET | `/connections/tiktok/redirect` | Like the Threads one: sends the browser to TikTok (scopes `user.info.basic,video.upload,video.publish`) to connect an account |
| GET | `/connect/tiktok/callback` | Where TikTok sends the browser back (register `{BRAIN_URL}/connect/tiktok/callback` in the TikTok app); lands on `{WEB_URL}/app/connections?connected=tiktok` or `?connect_error=<code>&provider=tiktok` |
| GET | `/v1/connections/{id}/pinterest/boards` | The boards of a connected Pinterest account (up to 1000; secret boards are not listed) and its default board, read live: `{"boards":[{"id","name","privacy"}],"default_board_id"}` (any member; a set token only for its set's account). `409 reconnect_required` for a dead token, `502 pinterest_failed` when Pinterest cannot be reached |
| PUT | `/v1/connections/{id}/pinterest` | Sets the account's default board, the one a pin goes to when its post names none: `{"default_board_id": "..." \| null}` answers `{"default_board_id","default_board_name"}`. The board must be one of the account's (`422 invalid_board`). Owners and admins, web origin only |
| GET | `/connections/pinterest/redirect` | Like the Threads one: sends the browser to Pinterest (scopes `boards:read,boards:write,pins:read,pins:write,user_accounts:read`) to connect an account |
| GET | `/connect/pinterest/callback` | Where Pinterest sends the browser back (register `{BRAIN_URL}/connect/pinterest/callback` in the Pinterest app); lands on `{WEB_URL}/app/connections?connected=pinterest` or `?connect_error=<code>&provider=pinterest` |
| GET | `/connections/meta/redirect` | Like the Threads one: sends the browser to Facebook Login for Pages and their linked Instagram accounts |
| GET, POST | `/webhooks/meta` | Meta's webhook (register `{BRAIN_URL}/webhooks/meta` in the Meta app). `GET` is the subscribe handshake: `hub.mode=subscribe` and a `hub.verify_token` equal to `META_WEBHOOK_VERIFY_TOKEN` get `hub.challenge` back as plain text, else `403`; `404` while the variable is unset. `POST` needs `X-Hub-Signature-256` (HMAC-SHA256 of the raw body under `META_APP_SECRET`, `403` otherwise, `404` without the secret), takes at most 256 KiB, and answers `200` once the items are stored |
| GET | `/connect/meta/callback` | Where Meta sends the browser back (register `{BRAIN_URL}/connect/meta/callback` in the Meta app); lands on `{WEB_URL}/app/connections?pick=meta` when there is something to pick, `?connected=meta` when not, or `?connect_error=<code>&provider=meta` |
| GET, POST, DELETE | `/v1/connections/meta/pick` | The caller's pending Meta pick (owner, admin): the Pages and Instagram accounts not connected yet (`{"facebook":[{"id","name","picture_url"}],"instagram":[...]}`, never tokens, `404` when none or over 15 minutes old); `{"facebook":[ids],"instagram":[ids]}` connects them (`201` with the accounts; `422 nothing_picked`, `invalid_pick`); drop it, `204` |
| GET | `/media/{name}` | A stored post file at an unguessable link, for platforms that download media themselves (Threads, Facebook, Instagram); `404` once the post settles |
| PUT | `/v1/connections/{id}/sets` | `{"set_ids"}` (owner, admin): the sets of the current team holding the account; must include the default set |
| PUT | `/v1/connections/{id}/plan` | `{"plan": "free"\|"paid"\|null}` (owner, admin; X accounts only): which X plan the account is on, which decides if its posts may take 25,000 characters instead of 280 and beats what X reported (`null` removes the setting). The connection shows `long_posts` (the effective answer), `plan` (the setting) and `detected_long_posts` (what X reported) |
| DELETE | `/v1/connections/{id}` | Disconnect an account (owner, admin), `204` |
| GET, POST | `/v1/social-sets` | The current team's live sets with `account_ids`, the default set first (`?deleted=true`: deleted ones still restorable); create one (`{"name"}`, owner, admin) |
| PATCH, DELETE | `/v1/social-sets/{id}` | Rename (`{"name"}`, the default set too); delete (sign-in from the last 10 minutes, not the default set), restorable for 30 days. Owner, admin |
| POST | `/v1/social-sets/{id}/restore` | Restore a set deleted in the last 30 days, with its accounts (owner, admin) |
| GET, PUT | `/v1/social-sets/{id}/queue` | The set's posting queue: `{"time_zone","slots":[{"weekday","time"}],"next"}` (weekday 0 is Sunday, `time` is `HH:MM` in the IANA `time_zone`, `UTC` until saved; `next` is the next 10 slot times with no `scheduled` post of the set). `PUT {"time_zone","slots"}` replaces both (any member): `422 invalid_time_zone`, `invalid_slot`, `duplicate_slot`, `too_many_slots` (over 50). A slot missing on a spring-forward day is skipped, an ambiguous fall-back one is its first occurrence |
| GET, POST | `/v1/posts` | The current team's posts (`?social_set_id=&status=&group_id=&approval=&repeat=&from=&to=` on `scheduled_at`, `approval` being `pending`, `approved` or `rejected`, `repeat=true` the posts with a repeat rule; the default set's id gives every set's posts), or create one (`{"social_set_id","platform":"bluesky","account_ids":[ids],"body","media":[urls],"thread":[{"body","media"}],"first_comment","cover","tiktok","youtube","pinterest","scheduled_at","queue"}`; any member). `"queue": true` instead of `scheduled_at` schedules it at the earliest free slot of the set's queue, picked under a lock per set so two calls never share one (`422 queue_with_scheduled_at` with both, `422 queue_empty` without slots; `PUT` answers `422 queue_not_supported`). `first_comment`: see "First comment" below; `cover`: see "Video cover"; `tiktok`: see "TikTok settings"; `youtube`: see YouTube below; `pinterest`: see Pinterest below. `thread`: up to 25 follow-up posts, each a reply to the one before and checked like the post (`422 {"error","part"}` names a bad one); `?validate_only=true` checks without creating (see "Validate only"); each part is saved on its target as it goes out, a target whose part fails is `failed` naming it with the parts before it kept, and a takedown deletes the parts last first, then the root |
| GET, PUT, DELETE | `/v1/posts/{id}` | One post with its `targets`; replace a `draft` or `scheduled` one; delete (`204` for draft or scheduled, `200` after taking a published one down, `422 takedown_failed` with a `message`, `409` for queued or failed) |
| GET, PUT | `/v1/posts/{id}/page` | The post's public page, one per post or per group: `{"enabled","details","code","live"}` (`code` is `null` before the first save, `live` says whether the public route serves it now). `PUT {"enabled","details"}` saves it whatever the post's status and makes the permanent 10-character `code` the first time: `422 details_too_long` over 5000 characters, `422 public_pages_off` for `enabled: true` while the team has public pages off |
| GET | `/v1/public/pages/{code}` | Public, no sign-in: `{"team","body","media","thread","details","published_at","links":[{"platform","url"}]}` from the first published post of the page, `links` from every published target. `404` unless the page is enabled, its team has public pages on and a post of it is `published`. `Cache-Control: public, max-age=60`; 120 requests a minute per IP |
| POST | `/v1/post-groups` | One post per platform of a set in one call (`{"social_set_id","platforms","account_ids","body","media","scheduled_at","queue","list_id","position","thread","first_comment","cover","tiktok","youtube","pinterest","overrides":{"<platform>":{"body","media","thread","first_comment","cover","tiktok","youtube","pinterest"}}}`; `platforms` defaults to those brain publishes to with a connected account; bodies up to 64 KiB): `201 {"group_id","posts","skipped"}`; `"queue": true` puts the whole group at one free slot of the queue; an invalid post creates nothing and its error names the `platform`, unless `"on_invalid":"skip"` (see "Validate only"). `?validate_only=true` checks without creating |
| POST | `/v1/posts/{id}/publish` | Publish a draft or scheduled post now: `202` with the post `queued`; poll it for the result. `403 approval_required` for a member while the team requires approval, unless the post is `approved`, and for anyone when the post is `rejected`. An owner's, admin's or token's publish of a `pending` post approves it |
| POST | `/v1/posts/{id}/metrics/refresh` | Read the post's metrics from its platforms now: `200` with the post. At most once a minute per post (`429 refreshed_recently`); a post with nothing to read comes back as it is. Any member or a token that is not read-only. See "Analytics" |
| GET | `/v1/analytics` | Metrics of the posts published in a range of UTC days (`?social_set_id=&from=YYYY-MM-DD&to=YYYY-MM-DD`, default the last 30 days, at most 366): `{"supported":[..],"totals":{"posts","likes","comments","shares","views"},"by_platform":[{"platform","posts","likes","comments","shares","views","supported"}],"by_day":[{"date","posts","likes","comments","shares","views"}],"top_posts":[..]}`. Any member, read-only tokens too; `422 invalid_range`. See "Analytics" |
| POST | `/v1/posts/{id}/approve`, `/v1/posts/{id}/reject` | Approve or reject a `scheduled` post (optional `{"note"}`, trimmed, at most 500 characters, else `422 invalid_note`): `200` with the post. Owner, admin or a token that is not read-only (`403 forbidden` for a member); `409 not_awaiting_approval` unless the post is `scheduled` with an `approval`. See "Approvals" |
| PUT, DELETE | `/v1/posts/{id}/repeat` | Make a `scheduled` post repeat (`{"every":"day"\|"week"\|"month","interval":1..12,"until":"YYYY-MM-DD"\|null,"remaining":1..100\|null}`), or stop it: `200` with the post, the rule in its `repeat`. It covers the other `scheduled` posts of the post's group. Any member; `409 not_scheduled`, `422 repeat_not_supported` for TikTok, `422 {"error":"invalid_repeat","field"}`, `422 too_many_repeats` over 200 posts with a rule per team. See "Repeating posts" |
| POST | `/v1/post-groups/{group_id}/takedown` | Takes every `published` post of a group down, one after another: `200` with the group and `results` (`post_id`, `platform`, `state` `taken_down`, `already_gone`, `failed` with a `message`, or `not_published`, left alone: a scheduled or queued post still goes out). A failure (TikTok has no delete call) does not stop the others, and calling it again only tries what is still up. One of the 10 publish, retry or take-down calls a minute. A token works too (`404` for a group outside a set token's set, `403 read_only_token` for a read-only one); the MCP tool is `takedown-post-group` |
| POST | `/v1/post-groups/{group_id}/approve`, `/v1/post-groups/{group_id}/reject` | The same for every `scheduled` post of a group that has an `approval`, all or none: `200 {"posts":[...]}` with the posts it changed, `409 not_awaiting_approval` when there is none |
| GET, POST | `/v1/lists` | Post lists (ordered groups such as a series), or create one (`{"name","social_set_id"}`; no set means team-wide, or the token's own set for a set token) |
| GET, PATCH, DELETE | `/v1/lists/{id}` | One list with its `items` (`post_id`, 1-based `position`, `post`); rename (`{"name"}`); delete (posts stay) |
| POST | `/v1/lists/{id}/posts` | Add a post (`{"post_id","position"}`; appends by default, an existing post moves) |
| DELETE | `/v1/lists/{id}/posts/{post_id}` | Take a post out; later posts move up |
| PUT | `/v1/lists/{id}/order` | Reorder (`{"post_ids"}`: every post of the list once, else `422 invalid_order`) |
| GET, POST | `/v1/media-library` | The team's media library, newest first: `{"items":[{"id","url","name","created_at","created_by"}],"next_cursor"}` (`?q=&cursor=&limit=`: `q` searches name and URL in any case, wildcards matched as typed; `limit` 1 to 100, default 50; `created_by` is `null` for a token). Add one (`{"url","name"}`, `201`): `url` a public `https://` URL up to 2048 bytes with no user or password, `name` trimmed up to 100 characters, by default the URL's last path segment decoded. `422 {"error":"invalid_media","field"}`, `409 {"error":"already_in_library","id"}` for a URL the team has, `422 library_full` past 1000 items |
| PATCH, DELETE | `/v1/media-library/{id}` | Rename (`{"name"}`, `200` with the item); remove (`204`). Posts keep their media URLs |
| POST | `/v1/uploads` | A file from the device for a post: the raw file as the body (any member, any token that may write). Typed by its bytes: JPEG, PNG, WebP, GIF or PDF up to 100 MB, MP4 or MOV up to `MEDIA_MAX_BYTES`. `201 {"id","url","content_type","size"}`; `422 media_type_not_supported`, `413 media_too_large`, `409 too_many_uploads` (200 uploads or 10 GiB kept per team, an upload in progress counting `MEDIA_MAX_BYTES`), `429 uploads_in_progress` (3 at once per team), `429 rate_limited` (60 a minute per team), `402 plan_limit` (billing on, no live plan). Put `url` in a post's `media` or `cover`: another team's (or a deleted) upload is `422 invalid_upload`, whatever the link's host or query. Kept a day, then while a post not yet published, or published with a failed target, names it |
| GET, POST | `/v1/feeds` | The team's RSS and Atom feeds, oldest first, as an array of `{"id","url","social_set_id","platforms":[..],"template","mode":"draft"\|"queue"\|"now","active","last_checked_at","last_error","items_posted","created_at"}`. Add one (`{"url","social_set_id","platforms","template","mode"}`, `201`; owner, admin or a full token): see [Feeds](#feeds). `422 {"error":"invalid_feed","field"}`, `422 {"error":"platform_not_supported","platform"}` for TikTok and YouTube, `422 {"error":"feed_unreadable","reason"}`, `422 too_many_feeds` past 20 |
| GET, PUT, DELETE | `/v1/feeds/{id}` | One feed; replace it (the same fields and `active`, `200`); remove it (`204`, its posts stay). Another team's id is `404` |
| POST | `/v1/feeds/{id}/check` | Check the feed now and answer it (`200`). At most once a minute per feed, every other check included: `429 checked_recently` |
| GET, POST | `/v1/snippets` | Saved captions, hashtag sets and inbox replies by name, ignoring case: `{"items":[{"id","kind","name","body","created_at","updated_at"}]}` (`?kind=caption`, `hashtags` or `reply`, a saved inbox reply). Save one (`{"kind","name","body"}`, `201`): `name` trimmed 1 to 80 characters, unique per team and kind ignoring case (`409 snippet_name_taken`), `body` 1 to 5000 characters. `422 {"error":"invalid_snippet","field"}`, `422 too_many_snippets` past 500 per team |
| PUT, DELETE | `/v1/snippets/{id}` | Replace kind, name and body, checked as on create (`200`); delete (`204`) |
| GET, POST | `/v1/inboxes` | The team's inboxes, the undeletable "All" first (every item of the team, `account_ids: []`), each with `account_ids` and `open_count` (its `open` items, follows left out; in the list only); create one (`{"name","account_ids":[..],"filters":{"kinds","keywords","unanswered"}}`, owner, admin): a custom inbox holds the items of its accounts, 1 to 100 of the team's Bluesky, Facebook and Instagram accounts (`422 invalid_accounts`), and an account may be in several. Its filters narrow it further. Past the plan's inboxes `402 plan_limit` with `limit: "inboxes"`; past 50 ("All" included) `409 too_many_inboxes` |
| PUT, DELETE | `/v1/inboxes/{id}` | Replace name, accounts and filters (same body); delete (owner, admin; not "All"). Disconnecting an account takes it out of every inbox |
| GET | `/v1/inboxes/{id}/items` | `?state=open\|done&kind=comment\|mention\|dm\|follow\|like\|repost\|activity&q=&tag=&assigned=me\|none\|<user id>&account_id=&cursor=&limit=`: items newest first with their `replies`, and `next_cursor`. Each item has `inbox_ids`, the custom inboxes that hold it (its account is in them and it passes their filters), for chips in "All". `account_id` keeps one account's items. `tag` keeps the items whose author's contact has that tag, `assigned` those assigned to you, to nobody or to that member. Each item has `contact_id`, `contact_tags` and `assigned_to` (`{"id","name"}` or null). `q` (1 to 100 characters after trimming) searches the text and the author's name and handle, any case, `%` and `_` literal. Activity items (`follow`: a new follower; `like` and `repost` of one of the account's posts, with `parent_remote_id` and `remote_url` pointing at that post) are stored `done`, never count as open and are listed only with their own `kind` or `kind=activity` (all three). A DM replying to an Instagram story has `story_reply: true` and `story_id`, `story_url` when Meta gave them; a DM that is a click on a quick reply button has its `button_payload`. `attachments` lists the photos, videos, stickers, GIFs, audio, files and links an item carries (`[{"kind","url","preview_url"?}]`, https links only, at most 10; a sticker or photo with no text has `text: ""`). Meta's links expire after a while, so a client falls back to the kind's name and `remote_url`. `group=conversations` lists the DMs of one account and contact as one row, the latest item, with `conversation` (`{"count","open_count","first_at","last_at"}`, counted over the items the inbox holds; `null` on a comment or mention): `state=open` keeps a conversation with any open item and `done` one with none, `q` and `tag` match any of its items, `assigned` its latest, and the cursor follows the latest item. A DM without a contact is a conversation of its own |
| GET, PATCH | `/v1/inbox-items/{id}` | One item; `{"state":"open"\|"done","assigned_to":"<user id>"\|null}`, either or both (any member): `assigned_to` is a member of the team (`422 invalid_assignee`) or null for nobody. A member removed from the team loses their assignments. With `"conversation":true` on a DM the change reaches every DM item of its conversation (ignored for other kinds) |
| GET | `/v1/inbox-items/{id}/conversation` | `?limit=&before=` (1 to 100, default 50): for a DM item, every DM item of the same account and contact with its `replies`, `{"items","has_more"}`: the newest page, oldest first within it; `before` (the first item's id) pages back. `422 not_a_conversation` for any other kind |
| POST | `/v1/inbox-items/{id}/reply` | `{"text","conversation"}` (any member): a public reply to a comment or mention, a DM for a DM. The item's state is left alone, unless `conversation` is true on a DM: then every open item of its conversation is `done` once the reply went out; In a public reply or a DM, an `@handle` that Bluesky resolves becomes a mention of its DID (at most 5; one that does not resolve stays plain text) and a `#tag` a tag, besides links. `422 {"error","message"}` when it cannot go (on Bluesky `reconnect_required`, `no_dm_access`, `dm_not_allowed`, and for a public reply, checked first so nothing is sent: `post_unavailable` (the post is gone or not shown to the account), `blocked_by_author`, `replies_disabled` (its threadgate); on Facebook and Instagram `meta_reconnect_required`, `meta_denied`, `meta_failed`, `outside_window`, `dm_unavailable`). A Facebook or Instagram DM item carries `reply_window` (`closes_at` 24 hours after the person's last message, `open`, and `human_agent_until` only with `META_HUMAN_AGENT`) |
| POST | `/v1/inbox-items/{id}/moderate` | `{"action"}` (`hide`, `unhide`, `delete`, `like`, `unlike`; every member, `delete` owners and admins): hides, shows, deletes or (Facebook only) likes the comment on Facebook or Instagram, answers the item, whose `moderation` says what was done and what `can` still be; `422 not_supported` for a like on Instagram or an item that is no Meta comment On a YouTube comment the actions are `hold` (held for review: hidden until approved), `publish` (shown again), `reject` and `ban` (hidden for good, and the author banned from the channel; owners and admins only; the item becomes `done`) and `delete_reply` with `reply_id` (deletes a public reply of the channel sent from SocialReins; owners and admins). Each costs 50 YouTube quota units, counted in `write_units_today`; `youtube_reconnect_required` and `youtube_failed` (with a `message`, e.g. comments turned off on the video) are the refusals. A YouTube reply (`/reply`, public only: YouTube has no DMs and no private replies) goes under the top-level comment, 10,000 characters at most, and YouTube has no like or heart for comments in its API |
| GET | `/v1/inbox-items/{id}/thread` | The posts above a comment, mention, like or repost in its Bluesky thread, read live as the account sees them: `{"posts":[{"uri","url","author","text","created_at","attachments"}],"truncated"}`, oldest first (a like or repost lists the liked post last, after what it answers); `truncated` when the chain ends at a post that is deleted or hidden. `422 not_a_thread` (a DM, a follow, another platform), `422 post_unavailable`, `422 reconnect_required`, `429` |
| GET, POST | `/v1/inbox-items/{id}/actions` | What a person does from a Bluesky item beyond replying. GET (any member): `{"viewer":{"liked","reposted","following","blocking","muted","thread_muted"},"viewer_error","hidden","log":[{"id","action","status","target","error","done_by","created_at"}]}`, the account's live state toward the item's post and author and what was done from here, newest first (`viewer` is `null` when Bluesky cannot be read). POST `{"action","confirm":true,...}`, `confirm` required (`422 confirm_required`): `like`, `unlike`, `repost`, `unrepost` (comments and mentions), `follow`, `unfollow`, `mute_author`, `unmute_author`, `mute_thread`, `unmute_thread` (any member), and for owners and admins `block_author`, `unblock_author`, `hide_reply`, `unhide_reply` (adds or takes the reply out of the `hiddenReplies` of the threadgate on our root post; `not_our_thread` otherwise), `delete_reply` (`reply_id`: one of the item's `replies[].id`, taken out of our repo) and `report` (`reason_type` `spam`, `violation`, `misleading`, `sexual`, `rude` or `other`, `reason` up to 2000 characters, `account: true` to report the author's account instead of the post; once per item and target, `409 already_reported`; goes to Bluesky's moderation service). On a DM (`kind: dm`): `accept_convo` (a message request), `mute_convo`, `unmute_convo`, `mark_read` (the item's message becomes the read mark), `react` and `unreact` (`emoji`: exactly one), and for owners and admins `leave_convo` (declines a request or leaves) and `delete_message` (for the account only); GET adds `convo` (`{status: accepted\|request, muted, group, members, unread}`, read live; a `group` reply goes to everyone in it). Message requests are read next to accepted conversations (`listConvoRequests`). Chat takes no images, only an embedded post, so a DM reply is text. A locked conversation answers `422 convo_locked`. Like, repost, follow and block look at the live state first, so repeating one adds nothing. Every attempt is logged (the reason's text is not). `403`, `404`, `422 {"error","message"}` (`not_applicable`, `own_account`, `invalid_reason`, Bluesky errors) or `429`; 60 a minute per user with thread reads |
| POST | `/v1/inbox-items/{id}/reply` | `{"text","conversation"}` (any member): a public reply to a comment or mention, a DM for a DM. The item's state is left alone, unless `conversation` is true on a DM: then every open item of its conversation is `done` once the reply went out; `422 {"error","message"}` when it cannot go (on Bluesky `reconnect_required`, `no_dm_access`, `dm_not_allowed`, and for a public reply, checked first so nothing is sent: `post_unavailable` (the post is gone or not shown to the account), `blocked_by_author`, `replies_disabled` (its threadgate); on Facebook and Instagram `meta_reconnect_required`, `meta_failed`, `outside_window`, `dm_unavailable`) |
| GET, PUT | `/v1/connections/{id}/x-inbox` | An X account's inbox settings (read: any member; set: owners and admins). Off until `{"enabled":true}`; `poll_minutes` 15 to 1440 (60), `budget_usd` $0.10 to $100 a month for this account ($2), `read_dms` (needs the DM permission, else `422 reconnect_for_dms`). Answers the state (`spent_usd` this month, `paused`, `message`, `last_error`, `can_read_dms`, `can_like`, `can_hide`, `reconnect`) and an `estimate` of what reads cost. X bills each post, user and DM event returned, so the poll asks only for what is new and stops at the budget |
| POST | `/v1/connections/{id}/x-inbox/resume` | Owners and admins: poll again after a pause (`credits`, `usage_capped`, `errors`); add credits or raise the cap with X first. A pause for `budget` ends on the 1st (UTC) or when the budget is raised |
| GET, PUT | `/v1/connections/{id}/youtube-inbox` | A YouTube channel's inbox settings (read: any member; set: owners and admins). Off until `{"enabled":true}` (the Google project's 10,000 quota units a day are shared by every team and by uploads); `poll_minutes` (10 to 1440, default 30), `daily_units` (20 to 2000, default 300: what reading may cost the channel a Pacific day, a read is 1 unit). The answer shows `units_today`, `write_units_today`, `paused` (`budget`, `quota`, `errors`) with `message`, `reconnect`, `reconnect_for_comments` and `shared_exhausted` (all channels together reached `YOUTUBE_INBOX_UNITS`). `422 invalid_poll`, `invalid_budget`, `reconnect_for_comments`; `409 reconnect_required` |
| POST | `/v1/connections/{id}/youtube-inbox/resume` | Owners and admins: read again after a pause |
| POST | `/v1/inbox-items/{id}/x-action` | `{"action":"like"\|"repost"\|"hide"\|"unhide"}` on an X comment or mention (hide: a reply to our post); `204`, or `422 {"error","message"}` (`x_reconnect_required` for an account connected before the inbox permissions) |
| GET | `/v1/contacts` | The people who wrote to the team's accounts, one per platform and author id, kept up by every stored inbox item: `?q=&tag=&platform=&cursor=&limit=` (1 to 100, default 50), newest `last_seen_at` first: `{"items":[{"id","platform","external_id","handle","display_name","avatar_url","tags","notes","opted_out","first_seen_at","last_seen_at","items_count"}],"next_cursor"}`. `q` searches the handle and display name, any case, `%` and `_` literal; `tag` is exact; `next_cursor` is opaque (it holds where the page ended, so a contact seen again between two pages repeats no row). Session only, like the inbox: no API tokens |
| GET, PATCH, DELETE | `/v1/contacts/{id}` | The contact plus `recent`, its 20 latest inbox items as the inbox lists them; `{"notes":string\|null,"tags":[..],"opted_out":bool}`, any of them (any member, but only an owner or admin sets `opted_out` back to `false`, else `403 forbidden`): `notes` at most 2000 characters, `tags` replaces the set (at most 20, each trimmed and lowercased, 1 to 30 characters, no commas, control, zero-width or text direction characters). `422 {"error":"invalid_contact","field"}`. `opted_out` (default `false`) keeps the contact out of every broadcast: a new DM whose whole text is `stop` or `unsubscribe` (trimmed, any case) sets it, one that says `start` clears it. Only broadcasts read it: automations and replies by hand are unaffected. It is kept for the person (team, platform and their id there), not the contact: deleting the contact keeps it, and a contact a later item makes is still opted out. Existing DMs were read once the same way when the feature came. `DELETE` (owner, admin; `204`) removes a person's data: the contact with its notes and tags; their inbox items stay with `contact_id` null, and a later item of theirs makes a new contact. Disconnecting an account deletes its inbox items and every contact left with none; deleting the team deletes them all |
| GET | `/v1/contact-tags` | `[{"tag","count"}]`: the tags in use, by count then name |
| GET, POST | `/v1/broadcasts` | Broadcasts: one DM to every reachable contact with a tag. The team's, newest first (the latest 200), each `{"id","name","text","tag","platform"\|null,"status":"draft"\|"sending"\|"paused"\|"sent"\|"cancelled","pause_reason","counts":{"total","sent","skipped","failed","pending"},"created_by","created_at","started_at","finished_at"}` (`pause_reason` is text only while `paused`; `counts` are the recipients of the start, all 0 for a draft; `pending` includes the ones going out right now); create a draft with `{"name","text","tag","platform"?}` (`201`): `name` 1 to 80 characters, `text` 1 to 1000 with each `{name}` (the contact's display name, "there" without a usable one) counted as 64, `tag` a contact tag, `platform` `bluesky`, `facebook` or `instagram`, or `null` for all three. `422 {"error":"invalid_broadcast","field"}`. Every member lists and reads; every other call here is for owners and admins (`403 forbidden`). Session only, like contacts: no API tokens. Another team's id is a `404` |
| GET, PUT, DELETE | `/v1/broadcasts/{id}` | One broadcast; replace a draft (`409 already_started` for anything else); delete a draft or a finished one with its recipients (`204`; `409 still_sending` while it sends or is paused, cancel it first) |
| POST | `/v1/broadcasts/{id}/preview` | Who it would go to if it started now, nothing stored or sent: `{"total","reachable","by_reason":{"opted_out","recently_messaged","outside_window","no_conversation","account_disconnected"},"by_platform":[{"platform","reachable"}],"sample":[up to 5 {"contact_id","display_name","platform"}]}`. `total` is the contacts with the tag on Bluesky, Facebook and Instagram (or the broadcast's one platform; contacts of other platforms are never counted), and equals `reachable` plus the five reasons. A contact is reachable when it did not opt out, no broadcast of the team went out to it in the last 24 hours (`recently_messaged`) and the account it last wrote to is connected and needs no reconnect (`account_disconnected` otherwise, also a Bluesky account without DM access). Facebook and Instagram: only a contact whose latest DM to that account is under 24 hours old (Meta's standard messaging window; a message tag is never used), else `outside_window`, and one who only ever commented is `no_conversation`. Bluesky: anyone who wrote a DM, comment or mention (a follower who wrote nothing is `no_conversation`) |
| POST | `/v1/broadcasts/{id}/send` | `{"confirm_total": n}` starts a draft when `n` is the reachable count of this very moment: those contacts become its recipients (one row each, in the same transaction as the count) and it is `sending`; `200` with the broadcast. `409 {"error":"audience_changed","preview":{..}}` with the new preview otherwise, `409 already_started` for anything but a draft, `409 another_sending` while another broadcast of the team sends, `422 no_recipients` with nobody reachable, `422 too_many_recipients` over 1000, `429 too_many_broadcasts` after 3 starts in 24 hours per team. A minute's round then sends to up to 20 recipients of each sending broadcast, each **at most once and never again**: a recipient is claimed before its send, and one whose send was started when the process died ends `failed` (`interrupted`) after 15 minutes; one only claimed goes back to `pending` (or is `skipped` when the broadcast was cancelled). Right before each send everything is checked again with the start's own rules: the tag (`untagged` when it was removed since), the opt-out, the 24 hours since another broadcast, the account and Meta's 24 hours. **Circuit breaker:** a send the platform refuses for more than the one person (a rate limit, a block, a policy or permission error, an outage, a dead token) fails that recipient, hands the other claimed ones back to `pending` and makes the broadcast `paused` with what the platform said in `pause_reason`; so do 5 failed sends in a row, whatever failed them. A paused broadcast never goes on by itself. The DM goes out from the account the contact last wrote to and shows in the inbox conversation like a reply sent by hand. At most 1000 broadcast messages per team in 24 hours: past it the rest stay `pending`, the broadcast stays `sending` and goes on as the cap frees. It is `sent` once nobody is pending |
| POST | `/v1/broadcasts/{id}/resume` | `paused` back to `sending` (owner, admin), `200` with the broadcast; the failed recipients are not tried again. `409 not_paused` otherwise, `409 another_sending` while another broadcast of the team sends. No new start: it does not count against the 3 a day |
| POST | `/v1/broadcasts/{id}/cancel` | `sending` or `paused` to `cancelled`: recipients not sent to yet are `skipped` (`cancelled`), the ones sent to stay. `409 not_sending` otherwise |
| GET | `/v1/broadcasts/{id}/recipients` | `?status=&cursor=`: `{"items":[{"id","contact_id","display_name","handle","platform","status","reason","updated_at"}],"next_cursor"}`, 100 a page in a fixed order (owner, admin). `status` is `pending`, `sending` (claimed by a round), `delivering` (the send was started), `sent`, `skipped` (`opted_out`, `recently_messaged`, `outside_window`, `no_conversation`, `account_disconnected`, `untagged`, `dm_not_allowed`, `too_long`, `contact_gone`, `cancelled`) or `failed` (`interrupted`, `internal`, or the platform's refusal, e.g. `rate_limited`, `meta_failed`, `bluesky_refused`). The contact's fields are `null` once it was deleted |
| GET | `/v1/subscribers` | Newsletter sign-ups from the team's public pages (owner, admin): `{"subscribers":[{"id","email","confirmed","subscribed_at","unsubscribed_at"}]}`, newest first, at most 5000. Only a `confirmed` one whose `unsubscribed_at` is `null` is ever mailed. `DELETE /v1/subscribers/{id}` removes one (`204`) |
| GET, POST | `/v1/newsletters` | Newsletters: one email to every confirmed subscriber who did not unsubscribe. Owners and admins only, reads included, session cookie only. The team's, newest first (the latest 200), each `{"id","subject","body","status":"draft"\|"sending"\|"paused"\|"sent"\|"cancelled","pause_reason","tested_at","counts":{"total","sent","failed","pending"},"created_by","created_at","started_at","finished_at"}` (`counts` are 0 for a draft; what is not `sent`, `failed` or `pending` was skipped: the person unsubscribed or was removed after the start, or the newsletter was cancelled first). `POST {"subject","body"}` makes a draft (`201`): `subject` 1 to 150 characters on one line, `body` 1 to 20000 characters of plain text with at most 20 links. The whole markup: a blank line ends a paragraph, a line break stays one, `**text**` is bold, a bare `https://` address becomes a link whose text is the address. Everything else is escaped, so HTML shows as typed and nothing but `https` is ever a link. `422 {"error":"invalid_newsletter","field"}` |
| GET, PUT, DELETE | `/v1/newsletters/{id}` | One newsletter; replace a draft (`409 already_started` for anything else; a changed subject or body clears `tested_at`); delete a draft or a finished one (`204`; `409 still_sending` while it sends or is paused, cancel it first) |
| POST | `/v1/newsletters/{id}/test` | Mails the draft to the caller's own account email and nobody else, exactly as a subscriber would get it (its unsubscribe links unsubscribe nobody), and sets `tested_at`; `200` with the newsletter. `422 sender_required` without the team's `newsletter_sender`, `409 already_started` for anything but a draft, `429 too_many_tests` after 10 in 24 hours per team, `502 mail_failed` |
| POST | `/v1/newsletters/{id}/send` | `{"confirm_total": n}` starts a draft when `n` is the number of confirmed, not unsubscribed subscribers of this very moment: they become its recipients (one row each, in the same transaction as the count) and it is `sending`; `200` with the newsletter. Checked in this order: `409 already_started`, `422 sender_required`, `422 test_required` (no test since the last edit), `422 no_recipients`, `422 too_many_recipients` (over 5000), `409 {"error":"audience_changed","total"}` with the current number, `429 too_many_newsletters` (a team starts one per 24 hours). A round every minute then mails up to 50 recipients of each sending newsletter, each at most once and never again: one whose mail was started when the process died ends `failed` (one only claimed is mailed by a later round), and before each mail the subscriber is read again (gone or unsubscribed: skipped). Any error of the mail provider (a rate limit, a refusal, an outage), or 5 failed sends in a row, makes it `paused` with a `pause_reason`. All teams together send 10000 newsletter mails per 24 hours; past that, recipients wait as `pending` |
| POST | `/v1/newsletters/{id}/resume` | `paused` back to `sending`, `200` with the newsletter; failed recipients are not tried again. `409 not_paused` otherwise. Nothing resumes by itself |
| POST | `/v1/newsletters/{id}/cancel` | `sending` or `paused` to `cancelled`: recipients not mailed yet are skipped, the ones mailed stay. `409 not_sending` otherwise |
| POST | `/v1/public/newsletter/unsubscribe` | Public, no sign-in, 120 a minute per IP. Sets the subscriber's `unsubscribed_at`; they are never mailed again until they sign up on a public page and confirm the new link. The token is each subscriber's own (random, made with their first mail, stored as a hash plus a copy sealed under `ENCRYPTION_KEY` so every mail carries the same link) and comes as `{"token"}` in the body, from the web page `{WEB_URL}/newsletter/unsubscribe?token=...` that the mail's footer links to and whose button posts it, or as `?token=` in the query with any body: the one-click POST of RFC 8058. `200 {"status":"ok"}` for any token, known or not. There is no `GET` (`405`): mail scanners open links. Every newsletter mail has the sender name as its From name (the address stays `MAIL_FROM`'s), a plain-text and an HTML part (the sender's logo, else their name, above a white card with the subject and the text), a footer with the sender name and postal address, "You get this because you signed up on a <team name> page" (the name with everything but letters, digits, spaces, `_`, `'` and `-` turned into spaces, so it cannot be a link) and the unsubscribe link, and the headers `List-Unsubscribe: <{BRAIN_URL}/v1/public/newsletter/unsubscribe?token=...>` and `List-Unsubscribe-Post: List-Unsubscribe=One-Click` |
| GET, POST | `/v1/automations` | The team's automations; each with `runs: {"sent","skipped","failed","running","waiting","total","count","last_at"}` (`count` equals `total`, `waiting` counts runs of a flow at a `wait`) and `steps` (null unless it is a flow, see Automation flows below); create one (`{"name","trigger","conditions","actions":[{"type":"send_dm"\|"reply_publicly","text"}],"active","fallback_public_reply"}`, owner, admin). Beside its sends, `actions` may hold up to 3 `{"type":"tag_contact","tag"}`, each tag once (every trigger takes it): it adds the tag to the contact of the item's author, sends nothing and takes no daily hit; its result in a run is `done`, or `skipped` with `already_tagged`, `tag_limit` (the contact holds 20) or `no_contact` (it was deleted), and a run's own status is that of its sends. Triggers: `comment_received` (conditions `social_set_id`\|`account_ids`, `post_remote_ids`, `keywords`, `first_per_author`; both actions), `mention_received` (scope, `keywords`; both actions), `dm_received` (scope, `keywords`, `first_message`: only the author's first DM ever stored on the account; `send_dm` in the conversation), `story_reply` (an Instagram DM replying to a story, which never runs `dm_received`; scope, `keywords`; `send_dm`), `button_clicked` (a click on a quick reply button on Facebook or Instagram, which never runs `dm_received` either; scope and `payloads`, 1 to 20 button payloads, required; `send_dm`) and `new_follower` (a Bluesky follow, once per follower per account ever: a refollow is stored but runs nothing; scope only; `send_dm`, skipped as `dm_not_allowed` when the follower takes no DMs). A condition or action the trigger does not take is `422 invalid_conditions` or `invalid_actions`; `fallback_public_reply` and `{page}` are for `comment_received` and `mention_received` only (`invalid_fallback`, `invalid_text`). Texts may hold `{name}` (the commenter's display name, else "there") and `{page}` (the address of the public page of the post the comment is under, counted as 100 characters); `{page}` needs a space, `(` or the start before it and a space, closing punctuation or the end after it, else `422 invalid_text`; an automation whose action holds `{page}` does not run (no run, no daily hit) on a comment whose post has no live page, and a fallback reply holding it is then `skipped` with `no_public_page`. Limited to accounts by name (`account_ids`): one on a network with no inbox (only Bluesky, Facebook and Instagram fire automations) is `422 unsupported_account`; when every account is Facebook or Instagram, a comment flow with more than one DM is `meta_one_private_reply` (one private reply per comment), a DM trigger's DM after 24 hours of waits, or a comment's private reply after 7 days, is `meta_reply_window`, and an Instagram DM over 1000 bytes ({name} counted as 64 four-byte letters) is `invalid_text`; enabling an automation checks these again. AI sends: `{"type":"ai_send_dm"\|"ai_reply_publicly","prompt"}` (as actions or flow steps, in place of a `send_dm` or `reply_publicly`; prompt 1 to 2000 characters, no `{page}`) send a reply an AI model writes from the prompt and the comment or DM. The team's own key (`/v1/team/ai-key`) goes first; without one, brain's `ANTHROPIC_API_KEY`, which with billing on costs 1 `ai` credit per reply. Its result carries `provider` and `model`; `skipped` with `no_ai_key` or `no_ai_credits`, or `failed` with `ai_failed`, `ai_empty`, `ai_too_long` or `ai_link` (a link or domain the prompt does not hold), and then nothing is sent |
| POST | `/v1/automations/test` | "Test this automation" (owner, admin, Automation plan, session only): `{"automation": <the body of POST /v1/automations, saved nowhere>, "sample": {"account_id","text","author_name","post_remote_id","button_payload","author_can_dm"}}` tries it on a sample from a stranger (so the guard against the team's own accounts does not apply) and answers `{"trigger","matched","mismatches","blocks","steps"}` without sending anything or storing an inbox item or a run. `mismatches` are the conditions that fail (`wrong_set`, `wrong_account`, `wrong_post`, `no_keyword`, `button_not_named`; a sample is always its author's first comment or message); `blocks` are rules that would stop it: `no_public_page` (a text holds `{page}` and the post has no live page: it would not run, `steps` is empty) and `meta_one_private_reply`, `meta_reply_window`, `x_unsupported_trigger` or `bluesky_unsupported_trigger` (the steps are still listed); `author_can_dm: false` (Bluesky accounts, default true) tries a person Bluesky would not let us DM: the DM (an AI one is still written and counted, as in a run) is `would_skip` `dm_not_allowed` and the fallback reply takes its place; each step has `status` `would_send` (with its final `text`, `{name}` and `{page}` filled in, AI text written by the real model with `provider` and `model`), `would_skip` or `would_fail` (with a `reason` as a run gives it, e.g. `no_dm_access`, `too_long`, `daily_cap`, `ai_link`), `would_tag`, `would_wait` or `not_reached`. An AI step counts against the AI allowance (or credit) like a run and is given back the same way. 100 tests a team a day, each AI step counting as one more (`429 rate_limited`); it runs to the end even if the request is dropped, so an AI spend is never left unrefunded; `422 invalid_sample` for an empty or over 2000 character text or an account that is not the team's or cannot run automations |
| GET, PUT, DELETE | `/v1/automations/{id}` | One automation with its `runs` counts; replace it; delete it (owner, admin). Changing a flow's steps or deleting it ends its waiting runs |
| POST | `/v1/automations/{id}/enable`, `/disable` | Switch it on (acts on items from then on) or off, which ends its waiting runs (owner, admin) |
| GET | `/v1/automations/{id}/runs` | Its runs newest first (`?cursor=`): `sent`, `failed` or `skipped` with a `reason` and per-action `results`, or `running`, or `waiting` for a flow at a `wait`; `step_index` (a flow's step last tried, from 0, null without steps) and `resume_at` (when a `waiting` run goes on, else null) |

`POST /v1/waitlist` answers `200 {"status":"ok","name_token":"..."}` for a new email and for one
already on the list, so it never reveals who signed up. The `name_token` (64 hex characters) is
stored hashed on a new row only; for a known email it is a random value that opens nothing.
`POST /v1/waitlist/name` with it sets the optional name within an hour of joining and answers
the same `200` for any well-formed token, so a stranger cannot change anyone's name or learn
who is on the list. A bad email gets `422 {"error":"invalid_email"}`. Each
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
10 requests a minute per IP. Only the token's SHA-256 hash is stored. `GET /v1/waitlist/count` is public and counts verified signups only. Signups made before names existed keep `verified_at` as it was (set only by opening the link), so none became verified by the migration. `GET /v1/admin/waitlist` needs a session cookie
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
hashes of link and session tokens are stored. `POST /auth/link` and `POST /auth/magic` share 5 requests a minute per IP. A link to an address with no account also draws from a daily
allowance, 20 per IP and 1000 overall (only links that are sent count); past it the answer is the same `202` and no mail goes out. A link to an existing account is
never counted, so a used-up allowance stops new sign-ups by email for the day and never a sign-in.

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

Team codes: every team has a `code`, 6 characters from `23456789abcdefghjkmnpqrstuvwxyz` (no
`0`, `o`, `1`, `l`, `i`), unique, given when the team is made and never changed, not even by a
rename. Teams that existed before the column get theirs from the migration. The web app uses it
as the team's address (`/t/<code>/home`). It is not a secret and gives no access. A cookie
request may name the team it acts in: the `X-Team: <code>` header, or `?team=<code>` on a page
navigation such as `/connections/<provider>/redirect`, in any case. The server checks the
caller's membership every time, and answers `404 {"error":"not_found"}` both for a team the
caller is not in and for a code no team has. A request that names no team acts in the user's
current team (`users.current_team_id`, the team opened last), so older clients keep working.
API tokens ignore both: a token always acts in its own team.

Team slugs: a team may also pick a `slug` (`null` until it does), and wherever a request names
a team by code (`X-Team`, `?team=`) it may use the slug instead, in any case. The code keeps
working and never changes. `PATCH /v1/team` with `{"slug": "acme"}` sets it and `{"slug": null}`
clears it (owner or admin). A slug is trimmed and lowercased, then must be 3 to 30 characters
of `a-z`, `0-9` and `-`, start and end with a letter or digit, hold no `--`, and not be one of
`new`, `admin`, `api`, `app`, `t`, `team`, `teams`, `login`, `signup`, `settings`, `billing`,
`www`, `socialreins`, `support`, `help`, `_`: otherwise `422 {"error":"invalid_slug","reason":...}`
with `reason` one of `reserved`, `length`, `characters` (checked in that order). A slug that
is another team's slug or any team's code is `409 {"error":"slug_taken"}`, and no new team is
given a code that is a slug. A team changes its slug (a set or a clear) at most 5 times a day,
then `429 {"error":"slug_changed_too_often"}`; refused changes and sending the slug it already
has do not count. A slug a team gives up (by changing or clearing it, or when the team is
deleted) stops naming the team at once: old links by it are `404`, so link by code where a link
must last. For 30 days after that no other team can take it (`409 slug_taken`), so an old link
never lands on another team; the team that had it may take it back at any time. Everything else is keyed by
the team's id: API tokens, posts and connections do not notice a slug change.

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
(`409 too_many_sets`). The default set (`is_default`, named "All sets" on a new team) holds every account; it can be renamed but not deleted
(`409 default_set`). Deleting a set needs a sign-in from the last 10 minutes;
it keeps its account links, hidden, so a restore within 30 days brings them back. An hourly
job in brain hard-deletes sets deleted over 30 days ago, one instance at a time (advisory lock).

Posts go to every account of their platform in their social set; Bluesky (300 graphemes, 4
images from public https URLs, a still image over 1 MB shrunk to fit, links become facets) and Threads (500
characters, an emoji counting its UTF-8 bytes, up to 20 JPEG or PNG images or MP4 or MOV videos as a carousel), LinkedIn (3000
characters, up to 20 JPEG, PNG or GIF images, one MP4 video or one PDF as a document, no threads), Facebook (63206
characters, up to 10 JPEG, PNG or GIF images or one MP4 or MOV video) and Instagram (2200
characters, 1 to 20 JPEG images or MP4 or MOV videos, `media_required` without one), YouTube
(one MP4 or MOV video, a title and a description, no threads), X (280 characters as X counts
them, up to 4 images or one GIF or one video, threads) and TikTok (2200 characters, one MP4 or MOV
video or 1 to 35 JPEG or WebP photos, `media_required` without one, no threads) publish today.
Saving with `scheduled_at` makes a post `scheduled`, else `draft`; only those two can change.

**Choosing accounts.** A post (and a post group) takes `account_ids`, the accounts to post to;
absent or `null` means every account of the platform in the set, an account added later
included. Each id must be the team's account of the post's platform in the set, and the list
may not be empty, else `422 invalid_accounts` (`field: "account_ids"`, `social_account_id` the
first bad one). A group gives each post the ids of its platform, and without `platforms` posts
to the platforms of the chosen accounts. Edits, repeats (the copy keeps the choice), the queue,
approvals and the MCP tools take and keep it. A chosen account that leaves the set or is
disconnected before the post goes out gets a `failed` target saying so, so the post names it.
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

**Alt text.** A post (and each thread part) may carry `media_alt`, a list of strings parallel to
`media`: one description per image, in order, at most 1000 characters each (the strictest limit of
the networks that take one), trimmed, no control characters. A shorter list leaves the last images
without; a longer one is `422 invalid_alt_text` (also a control character) and a long text `422
alt_text_too_long`, both on `media_alt` (a thread part's with its `part`). `media_alt` is saved aligned
with `media` (empty when no image has one) and comes back on the post. Left out on `PUT /v1/posts/{id}`,
the texts stored for the same links stay with them, so adding, removing or reordering images keeps each
text with its image; `[]` clears them (a thread part keeps them unless it sends a list). A group takes
`media_alt` like `media`, and an override with its own `media` has its own `media_alt` or none. Sent with
the images to Bluesky (`alt`), X (`POST /2/media/metadata` after the upload), Threads and Instagram
(`alt_text` on the image's container) and LinkedIn (`altText`); Facebook, TikTok and YouTube have none, and
a video's is ignored. A network that refuses the text still gets the post without it (a Threads or
Instagram container, or a LinkedIn post a `400` refused before anything was posted, is made again without
it; X's call comes after the upload and before the tweet), and the target's `alt_error` says so.

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
it frees up. A tick claims at most 50 posts, and a team no more than 3 at once (the rest wait `scheduled`, and each post that ends claims its team's next at once, so a burst takes minutes, not a tick each); 8 posts and 8 targets publish at once. The team limit is counted per brain process: run several instances and a team can have 3 on each. A past
`scheduled_at` means now.

A token created with `"read_only": true` (whole-team or set) only reads: every `POST`, `PUT`,
`PATCH` or `DELETE` it sends, and every `/mcp` write tool, is `403 {"error":"read_only_token"}`
before anything runs, except a create with `validate_only`, which creates nothing. A token is
revoked when the member who created it is removed from the team, leaves it, or becomes a plain
member: only an owner or admin may hold one. `GET /v1/posts/{id}` lists the post's `accounts` (id, provider, handle,
display name, target id and status), also before it publishes and has targets.

The media library and snippets are team assets, not set assets: any member reads and writes
them, and so does every token of the team, a set token too (it sees and changes the whole
team's library and snippets); a read-only token only reads them. Another team's id is `404`.
Adding a URL to the library stores the URL only, nothing is downloaded.

API token writes (`POST`, `PUT`, `PATCH`, `DELETE`) take an optional `Idempotency-Key` header,
1 to 255 printable ASCII characters (else `422 invalid_idempotency_key`). Keys belong to the
token's team. A retry with the same key, method, path and body gets the first answer again with
`Idempotent-Replayed: true`, or `409 idempotency_in_progress` while the first still runs; the
same key with anything else is `422 idempotency_key_reused`. 5xx and 429 answers are not kept,
so a retry runs again; every other answer is kept for 30 days. A first run unfinished after 10
minutes is taken as crashed and runs again. Sessions ignore the header.

Connections are the current team's social accounts. Members list them; owners and admins connect,
regroup and disconnect them. Tokens are sealed with `ENCRYPTION_KEY` and never returned. Every
account sits in the team's default set, which `PUT /v1/connections/{id}/sets` cannot drop
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
(id, username, name, picture) and saves a `threads` account in that team's default set; a
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
`youtube` account (`external_id` is the channel id) in the team's default set. Google always shows its
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
call, and saves a `tiktok` account (`external_id` is the open id) in the team's default set. The
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

Pinterest (when `PINTEREST_APP_ID` and `PINTEREST_APP_SECRET` are set) connects an account through
the same OAuth checks as Threads, with its own redirect URI
`{BRAIN_URL}/connect/pinterest/callback`, state and cookie (`sr_pinterest`, scoped to
`/connect/pinterest`). The callback swaps the code at `POST api.pinterest.com/v5/oauth/token`
(HTTP Basic with the app id and secret, `continuous_refresh=true`), needs the refresh token and
all five scopes (error code
`missing_scopes` when one is missing; `pinterest_failed` besides the Threads codes), reads
`GET /v5/user_account` (id, username, name, picture) and saves a `pinterest` account
(`external_id` is Pinterest's id) in the team's default set. Pinterest lists `boards:write` beside
the others for creating a pin, so brain asks for and requires it; brain never creates or changes a board. The
access token lasts 30 days, the refresh token 60 days from the last refresh: a token with under an
hour left is renewed before a call under a lock on the account row (the refresh token Pinterest
sends back is saved in the same transaction), and a daily job renews every account with 10 days or
less left, so an account nobody posts to does not lose its refresh token. A refused refresh, or a
`401`, flags the account for a reconnect.

**Pinterest settings.** A Pinterest post is a pin: the body is the description (at most 800
characters), the media 1 to 5 JPEG or PNG images (2 or more make a carousel; a WebP is turned
into a JPEG) or one MP4 or MOV video, never both (`media_required` without any; no threads, first
comment or cover). The post may carry `pinterest`: `{"board_id","title","link"}` (saved as
`posts.pinterest`, returned on the post): `board_id` is one of the account's boards
(`GET /v1/connections/{id}/pinterest/boards`), else the account's default board (`PUT
/v1/connections/{id}/pinterest`, kept across a reconnect); `title` at most 100 characters
(`pinterest_title_too_long`); `link` an http or https address of at most 2048 characters
(`invalid_pinterest_link`); a board id that is not digits is `invalid_pinterest_board`. A post
with no board named, for a set that holds a Pinterest account with no default board, is
`422 pinterest_board_required` naming that `social_account_id`. The pin's alt text is the first
non-blank alt text of its images, cut to Pinterest's 500 characters (a pin has one: a second
text, or the cut, is noted in the target's `alt_error`). A board id belongs to one account, so a post for several Pinterest accounts should name none and use each account's default board. Other platforms answer
`pinterest_not_supported`. A group takes `pinterest` for its Pinterest post only, with an override
(`overrides.pinterest.pinterest`). Feeds cannot post to Pinterest (an item has no image).

Images are not uploaded: Pinterest downloads each from brain's unguessable `/media/` links
(`image_url`, or `multiple_image_urls` for a carousel), in the `POST /v5/pins` call. A video is
registered (`POST /v5/media`), uploaded as a form to the storage address Pinterest names (a
streamed file read from brain's store, no bearer token sent there), asked about
(`GET /v5/media/{id}`) every three seconds for up to four minutes until it is processed, then
pinned (`media_source` `video_id`, with a cover taken from the first second); a video that is
still processing then fails the target with nothing posted. The saved id is the pin id and the
permalink `https://www.pinterest.com/pin/{id}/`. A take down is `DELETE /v5/pins/{id}` (a pin
already gone counts as down). A `429` fails the target as "nothing was posted" and so gets the
one automatic retry; a `403` naming a scope asks for a reconnect.

Pinterest has no comment or message API brain can poll, so there is no inbox or automation for
it. **Access tiers.** A new Pinterest app has Trial access, which is not live: every pin or board
it creates is only visible to the account that made it, and a Trial app may make 1,000 requests a
day and 300 writes (pin creates and deletes) a day in all, for every user of the app. Standard
access (an upgrade request with a screen recording of the OAuth flow and a pin made through the
app) makes pins public and limits writes to 100 a minute per user. Brain's calls are the same in
both; only Pinterest's rules differ.

X (when `X_CLIENT_ID` and `X_CLIENT_SECRET` are set) connects an account through the same OAuth
checks as Threads (state cookie `sr_x`, scoped to `/connect/x`), as a confidential Web App client
with PKCE. The app needs User authentication settings with read and write permission and the
redirect URI above. `GET /connections/x/redirect` makes a fresh code verifier and keeps it with
the state, sealed with `ENCRYPTION_KEY` like the tokens (the URL carries only its S256
challenge). The callback swaps the code and the verifier at `POST api.x.com/2/oauth2/token` with
the client id and secret in HTTP Basic auth, reads `GET /2/users/me` (id, username, name,
picture) and saves an `x` account (`external_id` is the user id, `handle` the username) in the
team's default set. Error codes `x_failed` and `missing_scopes` (the consent screen let the user
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
(`too_long`) that X would take, never the reverse. A text over 280 is held to an account's plan: the
user's setting (`PUT /v1/connections/{id}/plan`, kept across reconnects and token renewals) when there
is one, else the `subscription_type` X reported at connect or at the next renewal; an account with
neither counts as free. Media is uploaded first with
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

Automation flows: an automation holds either `actions` (all sent at once, as before) or `steps`,
never both (`422 actions_and_steps`; neither is `invalid_actions`). `steps` is an ordered list
of 1 to 10 of `{"type":"send_dm","text","buttons":[{"label","payload"}]}`,
`{"type":"reply_publicly","text"}`, `{"type":"wait","minutes"}` (1 to 10080, 7 days) and
`{"type":"tag_contact","tag"}` (tags the author's contact as the action does; whatever comes
of it, the flow goes on). A flow never starts or ends with a `wait` and holds at least one
send, a step must be one the trigger takes (`reply_publicly` and
`{page}` for `comment_received` and `mention_received` only), and `fallback_public_reply` is
for `actions` only (`422 invalid_steps`, `invalid_text`, `invalid_fallback`). `buttons` are
optional, at most 3, with a `label` of 1 to 20 characters (trimmed) and a `payload` of 1 to 64
of `a-z`, `0-9`, `_` and `-`. A flow answers with `actions: []`; an automation with actions
with `steps: null`.

A run goes through the steps in order. At a `wait` it is stored `waiting` with `step_index`
(the wait) and `resume_at`, and a tick every minute (`automations::tick`, its own advisory lock,
100 runs a round) takes it up from the next step once that time has come. The first send that
is skipped or fails (say `dm_not_allowed`, or `outside_window` when the wait outlasted Meta's
24 hours) ends the run with that status and reason, and the later steps never run; a run whose
every step went out is `sent`. `results` holds one entry per send tried, in order, and
`step_index` the last step tried. An item still runs an automation once, and each step is
claimed before it is sent and never retried: a run cut off mid-send fails as `interrupted`.
The daily cap of 500 counts each send step, and a step past it ends the run `skipped:
daily_cap`. Switching a flow off, changing its steps or deleting it ends its waiting runs
(`skipped: automation_changed`; a delete removes them with the automation); switching it on
again brings none back. A run also keeps the hash of the steps it started on
(`automation_runs.steps_hash`) and checks it, and that the flow is on, before every step and
before it parks, so a run caught mid-flight by a change ends the same way and never sends a
step of the new list. The tick holds its lock for the claim only, starts the runs it took for
at most 5 minutes and hands the rest back to `waiting`; a run taken up and never started is
put back by the stuck-run sweep, not failed. On Facebook and Instagram a comment takes one private reply, so a second
`send_dm` in a `comment_received` flow ends the run as `already_replied`.

Buttons go out as quick replies on Messenger and Instagram (`title` the label, `payload` the
payload). A click comes back through the webhook as a DM item with `button_payload`, and runs
the `button_clicked` automations whose `payloads` hold it (conditions: the scope and
`payloads`; `send_dm` only), never `dm_received` ones, like a story reply. Bluesky has no
buttons: the labels follow the text as numbered lines ("1. Label"), and an answer there is an
ordinary DM, so match it with a `dm_received` automation and `keywords`, not with a button.

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
profile is read once, an Instagram comment's link is the media permalink, and new items run
the team's automations. An Instagram message with `reply_to.story` is stored as a DM with
`story_reply` set (and the story's `id` and `url`); it runs `story_reply` automations, not
`dm_received` ones. Connecting (and reconnecting) a Page calls `POST {page}/subscribed_apps`
with `subscribed_fields=feed,messages` (`pages_manage_metadata`); a refusal never fails the
connect, it is logged and kept as `webhook_error` on the connection. Disconnecting the last
account of a Page (in any team) unsubscribes it, best effort. Replies: a comment gets a public
reply (`{comment}/comments` on Facebook, `{comment}/replies` on Instagram), a DM item a message
from the Page (`{page}/messages`, `recipient.id`, `messaging_type=RESPONSE`, within 24 hours of
the person's last message; Instagram goes through its linked Page). An automation's `send_dm`
on a comment is Meta's private reply (`recipient.comment_id`): within 7 days of the comment and
once per comment. Neither is ever retried. Reasons not sent: `outside_window`, `already_replied`,
`dm_unavailable` (these skip the DM and let the fallback public reply go), `rate_limited`.
A message with `message.quick_reply.payload` (a click on a quick reply button) is stored as a DM
with `button_payload` set (at most 200 characters); it runs `button_clicked` automations, not
`dm_received` ones.
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

## Feeds

A feed (`/v1/feeds`) is an RSS 2.0 or Atom URL whose new items become posts on their own. Every
member and a read-only token read feeds; only owners, admins and full tokens write them, since a
feed posts without anyone looking. A set token sees and writes the feeds of its own set.

- `url`: a public `https://` URL up to 2048 characters, no user or password. `platforms`: not
  empty, each one the set has an account for; TikTok and YouTube are refused (an item has no
  video). `template`: 1 to 2000 characters, default `{title}\n{link}`, with `{title}`, `{link}`
  and `{summary}` (the item's description as plain text, 300 characters at most). `mode`:
  `draft`, `queue` or `now`. At most 20 feeds per team.
- Adding a feed (or giving one a new `url`) reads it once. One that cannot be read or parsed is
  `422 feed_unreadable` with a short `reason` (`HTTP 404`, `timed out`, `over 1 MB`,
  `not an RSS or Atom feed`, ...). Every item it holds then is marked seen and never posted.
- The scheduler checks each active feed every 15 minutes (`feeds::tick`, one instance at a time).
  A round checks 8 feeds at once, the longest unchecked first, and starts no check after 5
  minutes: feeds it did not reach are first in the next round.
  The read is as careful as a media download: public addresses only (names are resolved by
  brain, so a name pointing at a private address is refused), at most 3 redirects, each checked
  the same way, 10 seconds and 1 MB at most, and `If-None-Match` or `If-Modified-Since` when the
  feed gave an `ETag` or `Last-Modified`.
- `{link}` is the item's link only when that is an absolute `http` or `https` URL; otherwise
  the feed's own site link when that is one, else nothing. A `javascript:`, `data:`, `ftp:` or
  relative link never reaches a post.
- A feed the parser breaks on is a failed check (`The feed could not be read`), like any other.
- An item is new when its guid (else its link) was not seen for this feed. A check posts at most
  5 new items, oldest first; the rest wait for the next check. Each becomes one post group of
  the feed's set and platforms: a `draft`, a post at the set's next free `queue` slot (a draft
  when the queue has no slot, which `last_error` says), or one scheduled `now`. The template is
  filled in per platform: over the platform's limit (X: 280) the summary is cut first, then the
  title, never the link. The item's image (an image enclosure or `media:content`, https only)
  is the posts' media; Instagram is left out of an item without one, and a platform that
  refuses the item is left out while the others post.
- A feed posts as a manager: where the team requires approval its posts are `approved`.
- The group and the mark that the item was seen are saved in one transaction, so no item posts
  twice. An item whose posts are all refused sets `last_error` and is tried again by the next
  check, 3 times in all, then given up.
- A check that cannot read or parse the feed sets `last_error`; 10 such checks in a row set
  `active` to `false`. `PUT` with `"active": true` switches it back on.

## Analytics

Brain reads how published posts did from the platforms that say so with the access an account
already has: Bluesky, Facebook Pages, Instagram and YouTube. Threads, LinkedIn, X and TikTok are
never called. One latest set of numbers is kept per target (`post_metrics`), no history.

- Every target of a post carries `metrics`: `null` before the first read and on the other
  platforms, else `{"likes","comments","shares","views","fetched_at"}`. A number the platform
  does not give is `null`: Bluesky has no views (shares are reposts and quotes together),
  YouTube no shares, Facebook no views (likes are every reaction), Instagram no shares and no
  views, and no likes when the account hides them. Of a thread only the root post is counted.
- The scheduler reads every 15 minutes (`metrics::tick`, one instance at a time): a target
  published in the last 30 days that was never read, or last tried over an hour ago while the
  post is under 48 hours old, over 12 hours ago after that. A round takes at most 500 targets,
  the longest unread first, grouped by account and asked in one call per 25 (Bluesky,
  `app.bsky.feed.getPosts`) or 50 (YouTube `videos.list?part=statistics`); Graph takes one post a call (v26 has no
  `/?ids=`). 8 accounts at once, and starts no call after 5 minutes.
- A read that fails keeps the numbers of the one before and waits twice as long per failed try,
  a day at most. A post the platform does not show keeps its last numbers and is asked for
  again a week later, 4 times; an account that needs a reconnect is skipped, and connecting it
  again clears the errors of its posts, so they are read in the next round. One post Graph
  refuses (a Reel has no reactions or shares, so it is asked for its likes and comments) never
  fails the others of its call. `fetched_at` is the last try, failed or not.
- `POST /v1/posts/{id}/metrics/refresh` reads one post now, waiting up to 20 seconds for its
  platforms, at most once a minute per post.
- `GET /v1/analytics` sums the `published` posts whose `published_at` falls on the days `from` to
  `to` (UTC, both included). A missing number counts as 0 and a post on several accounts counts
  once, with the numbers of all of them. `social_set_id` narrows it to one set (the default set
  is every set, as with posts); a set token only sees its own set. `by_platform` lists every
  platform with a post in the range, by name, those without metrics with `"supported": false`
  and zeros; `by_day` lists every day of the range, oldest first; `top_posts` holds up to 10
  posts, as `/v1/posts` gives them, by likes, comments and shares together, the most first,
  without those that have none.

## Approvals

A team with `require_approval` on (`PATCH /v1/team`, owner or admin, off by default) holds its
members' posts back until an owner or admin approves them. Every post carries `approval`
(`null`, `pending`, `approved` or `rejected`), `approval_note`, `approval_by` (the user's id)
and `approval_at`.

- While the switch is on, a create or an update that leaves the post `scheduled` (by
  `scheduled_at` or the queue) sets `approval`: `pending` for a member, `approved` for an owner
  or admin, with `approval_by` and `approval_at`. An API token acts as an admin, because only
  owners and admins make tokens and a token ends when its maker stops managing the team: its
  posts are `approved` with `approval_by` null. A draft has `approval: null`.
- Every edit is a new version: a member's edit of an `approved` or `rejected` post puts it back
  to `pending`, a manager's leaves it `approved`, and the note is cleared.
- The scheduler skips a `scheduled` post that is `pending` or `rejected`. Once approved it goes
  out on the next round, even when its time has passed. A member cannot publish one now either
  (`403 approval_required`). An owner, admin or token that publishes a `pending` post now
  approves it by doing so.
- A rejection is final until someone acts on it: a `rejected` post is never published, by the
  scheduler or by publish now (`403 approval_required` for anyone), whether or not the team
  still requires approval. Approve it, or edit it (an edit sets `approval` again as above, so
  with the switch off it becomes `null` and the post can go out).
- Switching `require_approval` off releases every `pending` post: they publish at their time
  (at once when it has passed), and new saves set `approval: null`. `rejected` posts stay held.
  A post scheduled before the switch went on has `approval: null` and is never held.

## Repeating posts

A `scheduled` post may carry a rule in `repeat` (`PUT /v1/posts/{id}/repeat`, cleared with
`DELETE`): `every` (`day`, `week` or `month`), `interval` (1 to 12), `until` (a UTC date, or
`null`) and `remaining` (how many more copies will be made after this post, 1 to 100, or `null`).
With both `null` it repeats until the rule is removed.

- When the post is taken to publish, by the scheduler or by publish now, brain schedules the
  next copy in the same transaction and clears the post's own rule, so a post yields one copy at
  most, whether it then publishes or fails. The rule lives on the one upcoming copy:
  `GET /v1/posts?repeat=true` lists a team's series.
- The copy is a new post with the same set, platform, body, media, thread, first comment, cover,
  YouTube settings, author and `approval`. It is in no list and has no public page. Copies of
  the posts of one group share one new `group_id`.
- Its time is the post's `scheduled_at` plus the interval: exact multiples of 24 hours or 7 days,
  or the same day of the month and time in UTC, on the month's last day when the month is
  shorter (January 31, February 28, March 31). A time already past is skipped, so a post held
  back for a week yields one copy, not seven. Publish now counts from the time the post was
  scheduled at, not from now.
- The copy carries the rule with `remaining` one lower, and the copy of a post with
  `remaining: 1` carries no rule: `remaining: N` makes N copies in all. No copy is made when the
  next time is after the end of `until`, or when the team is at its 2000 posts.
- A copy that cannot be made is logged and ends the series; the post itself still publishes.
- Setting or clearing a rule covers the other `scheduled` posts of the post's group and is an
  edit: while the team requires approval, a member's puts those posts back to `pending`.
- An edit (`PUT /v1/posts/{id}`) keeps the rule, unless it makes the post a draft or a TikTok
  post. TikTok posts never repeat: TikTok needs a person to confirm every post.
- At most 200 posts with a rule per team.

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
| `META_HUMAN_AGENT` | no | `true` once Meta granted the app the Human Agent feature (App Review). A person's inbox reply may then go out up to 7 days after the person's last message, with the `HUMAN_AGENT` tag; without it, 24 hours. Automations never use the tag |
| `META_WEBHOOK_VERIFY_TOKEN` | no | Any secret string, typed into the Meta app's webhook setup too. Turns on `GET /webhooks/meta`; `POST /webhooks/meta` is signed with `META_APP_SECRET` |
| `THREADS_APP_ID`, `THREADS_APP_SECRET` | no | Both to connect Threads accounts (redirect URI `{BRAIN_URL}/connect/threads/callback`). Publishing with connected accounts needs neither |
| `LINKEDIN_CLIENT_ID`, `LINKEDIN_CLIENT_SECRET` | no | Both to connect LinkedIn profiles (redirect URI `{BRAIN_URL}/connect/linkedin/callback`). Publishing with connected accounts needs neither |
| `X_CLIENT_ID`, `X_CLIENT_SECRET` | no | Both to connect X accounts (a Web App client, redirect URI `{BRAIN_URL}/connect/x/callback`). Publishing with a live token needs neither, renewing one does |
| `YOUTUBE_INBOX_UNITS` | no | YouTube quota units a day (0 to 10000, default 4000) that every channel together may spend reading comments in the inbox, out of Google's 10,000 a day for the whole project, so reading never uses up what uploads need. Resets at midnight Pacific time |
| `TIKTOK_CLIENT_KEY`, `TIKTOK_CLIENT_SECRET` | no | Connect TikTok accounts (redirect URI `{BRAIN_URL}/connect/tiktok/callback`) |
| `PINTEREST_APP_ID`, `PINTEREST_APP_SECRET` | no | Both to connect Pinterest accounts (redirect URI `{BRAIN_URL}/connect/pinterest/callback`). Publishing with a live token needs neither, renewing one does |
| `GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET` | no | Both or neither. Turn on Google sign-in (OAuth client of type Web application, redirect URI `{BRAIN_URL}/auth/google/callback`). They also turn on connecting YouTube channels (add the redirect URI `{BRAIN_URL}/connect/youtube/callback` to the same client) |
| `TURNSTILE_SITE_KEY`, `TURNSTILE_SECRET_KEY` | no | Both or neither. Turn on a Cloudflare Turnstile captcha: `POST /auth/link` and `/v1/contact` then need a `turnstile_token`. Off when unset |
| `ADMIN_EMAILS` | no | Comma-separated emails, any case, whose signed-in users may use the admin console, `/v1/admin/*` (and get `is_admin: true` from `/auth/me`). None when unset |
| `REVIEW_LOGIN_TOKEN` | no | Turns on `GET /review-login/{token}`, a sign-in to a demo account for Meta App Review. Unset (the default) the route is a `404`; shorter than 32 characters, brain refuses to start. Remove it after the review: see [App Review login](#app-review-login) |
| `MEDIA_DIR` | no | `./media` (`/var/lib/brain/media` in the image). Where post media is kept while a post publishes, when no bucket is set. Files go once the post settles, so an ephemeral disk is fine. A download cut off by a restart leaves a temp file, which the hourly sweep removes once it is an hour old. Assumes a single brain instance: with more than one replica, use the `S3_*` bucket |
| `S3_BUCKET`, `S3_ENDPOINT`, `S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY` | no | All or none. Keep post media in an S3-compatible bucket instead of `MEDIA_DIR`, path-style. Cloudflare R2: `S3_ENDPOINT=https://<account>.r2.cloudflarestorage.com`; AWS S3: `https://s3.<region>.amazonaws.com` with `S3_REGION`. Files are written as multipart uploads, which brain aborts when a download fails; give the bucket a lifecycle rule that aborts incomplete multipart uploads after a day (R2 does so after 7 days by default) for the ones a restart cuts off |
| `S3_REGION` | no | `auto` (what R2 wants) |
| `MEDIA_MAX_BYTES` | no | `1073741824` (1 GiB), which is also the most. The largest video a post may carry, in bytes. Videos are streamed to and from storage, so this is disk or bucket space, not memory. It caps videos only, whatever its value: images and PDFs stay at 100 MB. A video over it is refused when the post is saved (`422 media_too_large` with `limit` and `actual`) where its server says its size, else when it publishes |
| `CONTACT_TO` | no | Turn on `POST /v1/contact`, which mails each message to this address (Reply-To: the sender). Off when unset |

## App Review login

Meta App Review needs a reviewer to sign in without receiving email. To allow it:

1. Set `REVIEW_LOGIN_TOKEN` to a long random value (`openssl rand -hex 32`; brain refuses to start with fewer than 32 characters) and redeploy.
2. Give the reviewer `{BRAIN_URL}/review-login/<token>`. Opening it signs the browser in to the
   demo user `review@socialreins.com` ("App Review Demo", the owner of its own team with the
   default social set, nothing connected) and lands on `{WEB_URL}/app`. The reviewer connects
   their own test accounts from there.
3. Once the review is over, remove `REVIEW_LOGIN_TOKEN` and redeploy: the route is a `404` again.
   On every start without the variable brain ends the demo user's sessions, deletes its passkeys,
   and revokes every API token and deletes every pending invitation of each team the demo user
   is in, whoever made them. The demo user, its teams and their other members stay until you
   remove them; brain logs a warning when such a team has other members, so check its Team page.
   The cleanup runs once, at start: a link opened on the old instance while the new one was
   starting outlives it, so restart brain once more after that deploy is live.

A wrong token and an unset one both give the same bare `404`, and the token is compared in
constant time and never logged. The route shares the per-IP sign-in rate limit. The demo user
signs in even with `REGISTRATION_DISABLED=true`, is never an admin, and brain refuses (with a
warning in the log) if `review@socialreins.com` is listed in `ADMIN_EMAILS`. Anyone holding the
link has full use of the demo account, so treat it like a password. In a team it owns, the demo
user cannot be demoted, removed or leave (`403 forbidden`), so nobody signed in through the link
can hand the demo team to another account and out of that cleanup. Keep the demo user out of
your own teams: the cleanup revokes the tokens and invitations of every team it is in.

## Media

A post's `media` are URLs: public `https://` links, Google Drive share links
(`/file/d/<id>/view`, `open?id=`, `uc?id=`) or Dropbox share links to one file (`/s/<id>/<name>`,
`/scl/fi/<id>/<name>`); the file must be shared with anyone with the link.
**Uploads.** A file uploaded with `POST /v1/uploads` is streamed into the same storage under
`uploads/<team>/<id>`, typed by its first bytes (never by its name or `Content-Type`), and
answered with brain's unguessable link to it (`/media/<sealed>.<ext>`, served with its real type
and `nosniff`, for the composer's preview). A post holds that link like any other: saving checks
that it is an upload of the post's team, probing reads the stored file, and publishing copies it
inside the storage to the post's own files, so it is never downloaded. An upload is kept a day,
then for as long as a draft, scheduled or queued post of its team names it; the hourly sweep
deletes the rest, file and row. Publishing an upload deleted since fails that target (`HTTP 404`).

Nothing is fetched when a post is saved. When it publishes, brain downloads each URL once into
its storage (`MEDIA_DIR` or the `S3_*` bucket): names must resolve to public addresses, at most 3
redirects, each to a public https URL, 4 at a time, streamed to storage in 5 MB parts (a download
holds about 15 MB however large the file) and typed by its first bytes. A video may be up to 1 GiB
(`MEDIA_MAX_BYTES` lowers it; Bluesky takes 100 MB and X 512 MB), any other file 100 MB; a file
over its cap is refused by its `Content-Length`, else the moment its bytes pass it, and nothing of
it stays stored. Each download has 60 seconds plus the bytes that came so far at 2 MB a second (about 3.5 minutes
for 300 MB; the size its server claims earns nothing, so a slow one ends early), all of a post's 12 minutes together, and a server silent for 30 seconds fails at
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
