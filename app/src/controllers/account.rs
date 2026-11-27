//! Self-serve data export and account deletion for the signed-in user (session cookie only,
//! never an API token). Decision 0019.
//!
//! `POST /v1/account/export` streams one JSON file of the user's own data, one page of rows at a
//! time (`PAGE`), so memory stays at one page whatever the size; a database error ends the body
//! early, which the client sees as a failed download. One export an hour per user.
//! `DELETE /v1/account` deletes the user and the teams they alone belong to in one transaction
//! (see `erase`). Both only touch the caller's own rows.
use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use bytes::Bytes;
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement, TransactionTrait, Value,
};
use serde::Deserialize;
use socialreins_core::email;
use tokio::sync::mpsc;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    auth::{internal, named_cookie, Body as JsonBody, CurrentUser, FromWeb, SESSION_COOKIE},
    review_login::is_demo_email,
    team_slugs::RETIRE,
    waitlist::{error, Error},
};
use crate::{models::users, state::AppState};

/// Rows read at a time: the most an export holds in memory (plus what it has queued).
const PAGE: i64 = 200;

/// One table, or a slice of one, in the export. `cols` is an allowlist: a column added later is
/// not exported until someone lists it here or in `skip` (a test fails until one does).
pub struct Part {
    pub key: &'static str,
    pub table: &'static str,
    pub cols: &'static str,
    /// Columns left out on purpose: secrets, ids of other systems, internal bookkeeping.
    pub skip: &'static str,
    /// `WHERE` of the slice; `$1` is the team (or, for the user's own parts, the user).
    pub filter: &'static str,
    /// An expression unique among the slice's rows; pages continue after the last one. A column
    /// (`id`) is compared as a uuid so the primary key index serves the page; the composite
    /// keys of the small link tables are text.
    pub id: &'static str,
    pub uuid: bool,
    /// One row, written as an object (or `null`), not a list.
    pub one: bool,
}

const fn part(
    key: &'static str,
    table: &'static str,
    cols: &'static str,
    skip: &'static str,
    filter: &'static str,
) -> Part {
    Part {
        key,
        table,
        cols,
        skip,
        filter,
        id: "id",
        uuid: true,
        one: false,
    }
}

const fn one(mut p: Part) -> Part {
    p.one = true;
    p
}

const fn keyed(mut p: Part, id: &'static str) -> Part {
    p.id = id;
    p.uuid = false;
    p
}

const TEAM: &str = "team_id = $1";

/// The person's own rows (`$1` is their id).
pub const OWN: &[Part] = &[
    one(part(
        "profile",
        "users",
        "id, email, name, username, created_at, updated_at",
        "current_team_id, google_sub",
        "id = $1",
    )),
    part(
        "passkeys",
        "passkeys",
        "id, name, created_at, last_used_at",
        "user_id, credential_id, credential",
        "user_id = $1",
    ),
    keyed(
        part(
            "sessions",
            "sessions",
            "created_at, expires_at",
            "token_hash, user_id",
            "user_id = $1",
        ),
        "token_hash",
    ),
    keyed(
        part(
            "memberships",
            "team_user",
            "team_id, role, created_at",
            "user_id",
            "user_id = $1",
        ),
        "team_id::text",
    ),
];

/// What a plain member gets of a team: its name and the posts they wrote (`$1` team, `$2` user).
pub const MEMBER: &[Part] = &[
    one(part(
        "team",
        "teams",
        "id, name, code, slug, created_at",
        "updated_at, public_pages, newsletter, require_approval, newsletter_sender_name, \
         newsletter_sender_address, newsletter_sender_logo, automations_paused",
        "id = $1",
    )),
    part(
        "posts",
        "posts",
        POST_COLS,
        POST_SKIP,
        "team_id = $1 AND created_by = $2",
    ),
];

const POST_COLS: &str = "id, number, social_set_id, platform, body, media, status, scheduled_at, \
    published_at, error, created_by, created_at, updated_at, group_id, thread, first_comment, \
    cover, tiktok, youtube, approval, approval_note, approval_by, approval_at, repeat, \
    account_ids, media_alt, pinterest";
const POST_SKIP: &str = "team_id, quota_reserved, fanned_out";

/// What an owner or admin gets of a team (`$1` is the team). Subscribers, recipients and runs
/// are other people's data or internal logs and are named in the README of the export instead.
pub const TEAM_PARTS: &[Part] = &[
    one(part("team", "teams",
        "id, name, code, slug, created_at, updated_at, public_pages, newsletter, require_approval, \
         newsletter_sender_name, newsletter_sender_address, newsletter_sender_logo, \
         automations_paused", "", "id = $1")),
    keyed(one(part("billing", "subscriptions",
        "plan_id, status, trial_ends_at, current_period_end, cancel_at_period_end, created_at",
        "team_id, provider, provider_customer_id, provider_subscription_id, updated_at, \
         provider_event_at", TEAM)), "team_id::text"),
    part("credit_ledger", "credit_ledger", "id, kind, amount, reason, created_at",
        "team_id, ref", TEAM),
    part("api_tokens", "api_tokens",
        "id, name, social_set_id, read_only, created_by, last_used_at, expires_at, revoked_at, \
         created_at", "team_id, token_hash, prefix_hint", TEAM),
    keyed(one(part("webhook", "team_webhooks", "url, created_at, updated_at", "team_id, secret",
        TEAM)), "team_id::text"),
    part("social_sets", "social_sets",
        "id, name, is_default, created_at, updated_at, deleted_at", "team_id", TEAM),
    part("social_accounts", "social_accounts",
        "id, provider, external_id, handle, display_name, avatar_url, dm_access, created_by, \
         created_at, updated_at",
        "team_id, access_token, token_expires_at, meta, notifications_seen_at, messages_seen_at, \
         inbox_error_at, notifications_cursor, notifications_pending_at, messages_resume_at, \
         inbox_retry_at, meta_synced_at, threads_synced_at, threads_full_at, inbox_read_at", TEAM),
    keyed(part("account_sets", "social_account_social_set", "social_account_id, social_set_id", "",
        "social_account_id IN (SELECT id FROM social_accounts WHERE team_id = $1)"),
        "social_account_id::text || social_set_id::text"),
    part("posts", "posts", POST_COLS, POST_SKIP, TEAM),
    part("post_targets", "post_targets",
        "id, post_id, social_account_id, status, remote_id, remote_url, error, published_at, \
         created_at, updated_at, parts, comment_status, comment_id, comment_error, cover_error, \
         alt_error", "remote_cid, retry_at, retried",
        "post_id IN (SELECT id FROM posts WHERE team_id = $1)"),
    part("lists", "post_lists",
        "id, number, social_set_id, name, remote_ids, created_at, updated_at, deleted_at",
        "team_id", TEAM),
    keyed(part("list_items", "post_list_items", "list_id, post_id, position", "",
        "list_id IN (SELECT id FROM post_lists WHERE team_id = $1)"),
        "list_id::text || post_id::text"),
    part("public_pages", "post_pages",
        "id, subject, code, details, enabled, created_at, updated_at, taken_down_at", "team_id",
        TEAM),
    part("automations", "automations",
        "id, number, name, active, activated_at, trigger, conditions, actions, steps, \
         fallback_public_reply, cooldown_minutes, daily_cap, created_by, created_at, updated_at",
        "team_id", TEAM),
    part("contacts", "contacts",
        "id, number, platform, external_id, handle, display_name, avatar_url, notes, \
         first_seen_at, last_seen_at, last_broadcast_at", "team_id", TEAM),
    keyed(part("contact_tags", "contact_tags", "contact_id, tag", "",
        "contact_id IN (SELECT id FROM contacts WHERE team_id = $1)"), "contact_id::text || tag"),
    part("inboxes", "inboxes", "id, number, name, is_all, filters, created_at, updated_at",
        "team_id, filters_before_accounts", TEAM),
    part("inbox_items", "inbox_items",
        "id, number, social_account_id, platform, kind, author_handle, author_name, text, \
         received_at, state, replied_at, created_at, permalink, contact_id, assigned_to",
        "team_id, remote_id, remote_cid, parent_remote_id, root_remote_id, root_remote_cid, \
         convo_id, post_id, author_id, author_avatar, story_reply, story_id, story_url, \
         button_payload, automated_at, attachments, moderation", TEAM),
    part("inbox_replies", "inbox_replies",
        "id, inbox_item_id, kind, text, sent_by, automation_id, created_at", "remote_id",
        "inbox_item_id IN (SELECT id FROM inbox_items WHERE team_id = $1)"),
    part("broadcasts", "broadcasts",
        "id, number, name, text, tag, platform, status, pause_reason, created_by, created_at, \
         started_at, resumed_at, finished_at", "team_id", TEAM),
    part("newsletters", "newsletters",
        "id, number, subject, body, status, pause_reason, tested_at, sender_name, sender_address, \
         sender_logo, created_by, created_at, started_at, resumed_at, finished_at", "team_id",
        TEAM),
    part("media_library", "media_library", "id, url, name, created_at", "team_id, created_by",
        TEAM),
    part("snippets", "snippets", "id, kind, name, body, created_at, updated_at", "team_id", TEAM),
    part("feeds", "feeds", "id, social_set_id, url, platforms, template, mode, active, created_at",
        "team_id, etag, last_modified, last_checked_at, last_error, failures, items_posted, \
         created_by", TEAM),
    keyed(part("queues", "queue_schedules", "social_set_id, time_zone, slots, updated_at",
        "team_id", TEAM), "social_set_id::text"),
];

/// Tables with a `team_id` that the export leaves out, so a test notices a new one that nobody
/// decided about.
pub const NOT_EXPORTED: &[&str] = &[
    "automation_misses",
    "automation_runs",
    "contact_opt_outs",
    "idempotency_keys",
    "inbox_actions",
    "media_uploads",
    "meta_picks",
    "oauth_states",
    "post_metrics",
    "post_notices",
    "subscribers",
    "team_ai_keys",
    "team_invitations",
    "team_slug_history",
    "team_user",
    "trial_uses",
    "webhook_deliveries",
];

const README: &str = "Your SocialReins data, as of the time in `exported_at`.\n\
\n\
profile, passkeys (names only), sessions (times only) and memberships are about you.\n\
\n\
`teams` has one entry for each team you belong to. For a team where you are an owner or admin it \
holds the team's data: settings, billing summary (plan and status, no card or Stripe data), \
credit ledger, API tokens (names only), webhook address, sets, connected accounts (no tokens or \
passwords), posts with their targets and status, lists, public pages, automations, contacts and \
their notes, inbox items and our replies (the text people sent to the team's accounts), \
broadcasts, newsletters, media library, saved replies, feeds and posting queues. Contacts and \
inbox text are the team's data, not only yours: they are in the export of an owner or admin and \
nobody else. For a team where you are a plain member you get the team's name and the posts you \
wrote.\n\
\n\
Not included: tokens, passwords, passkey secrets, encryption keys and anything from Stripe; \
other members' details; newsletter subscribers and recipients; automation run logs; inbox \
actions; delivery logs; usage counters. Ask info@socialreins.com if you need one of those.\n\
\n\
Dates are UTC (ISO 8601). Posts already published on a network stay there; this file is a \
copy, not a link to them.\n";

type Chunk = Result<Bytes, std::io::Error>;

/// Why an export stopped: the reader left, or the database failed.
enum Stop {
    Closed,
    Db(DbErr),
}

impl From<DbErr> for Stop {
    fn from(e: DbErr) -> Self {
        Self::Db(e)
    }
}

struct Sink(mpsc::Sender<Chunk>);

impl Sink {
    async fn put(&self, text: String) -> Result<(), Stop> {
        self.0
            .send(Ok(Bytes::from(text)))
            .await
            .map_err(|_| Stop::Closed)
    }
}

fn stmt(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// The SQL that reads one page of `p`: rows after the id bound as the last parameter.
fn page_sql(p: &Part, filter: &str, last: usize) -> String {
    format!(
        "SELECT k, to_jsonb(r) - 'k' AS j FROM \
         (SELECT {id}::text AS k, {cols} FROM {table} \
          WHERE ({filter}) AND {id} > ${last}::text{cast} ORDER BY {id} LIMIT {PAGE}) r \
         ORDER BY k",
        id = p.id,
        cast = if p.uuid { "::uuid" } else { "" },
        cols = p.cols,
        table = p.table,
    )
}

/// Writes `"key":<rows>` for `p` with `binds` as `$1...`, one page at a time.
async fn write_part(
    db: &DatabaseConnection,
    out: &Sink,
    p: &Part,
    binds: &[Value],
) -> Result<(), Stop> {
    let sql = page_sql(p, p.filter, binds.len() + 1);
    let head = format!("{}:", serde_json::to_string(p.key).unwrap_or_default());
    out.put(if p.one { head } else { format!("{head}[") })
        .await?;
    let mut last = if p.uuid {
        Uuid::nil().to_string()
    } else {
        String::new()
    };
    let mut first = true;
    loop {
        let mut values = binds.to_vec();
        values.push(last.clone().into());
        let rows = db.query_all_raw(stmt(&sql, values)).await?;
        let mut text = String::new();
        for row in &rows {
            let json: serde_json::Value = row.try_get("", "j")?;
            if !first && !p.one {
                text.push(',');
            }
            first = false;
            text.push_str(&json.to_string());
            if p.one {
                break;
            }
        }
        out.put(text).await?;
        match rows.last() {
            Some(row) if !p.one && rows.len() as i64 == PAGE => {
                last = row.try_get("", "k")?;
            }
            _ => break,
        }
    }
    if p.one && first {
        out.put("null".into()).await?;
    }
    if !p.one {
        out.put("]".into()).await?;
    }
    Ok(())
}

async fn write_parts(
    db: &DatabaseConnection,
    out: &Sink,
    parts: &[Part],
    binds: &[Value],
) -> Result<(), Stop> {
    for (n, p) in parts.iter().enumerate() {
        if n > 0 {
            out.put(",".into()).await?;
        }
        write_part(db, out, p, binds).await?;
    }
    Ok(())
}

/// The whole export of `user`, written to `out`.
async fn export_to(db: &DatabaseConnection, out: &Sink, user: Uuid) -> Result<(), Stop> {
    let readme = serde_json::to_string(README).unwrap_or_default();
    out.put(format!(
        "{{\"readme\":{readme},\"exported_at\":\"{}\",",
        chrono::Utc::now().to_rfc3339()
    ))
    .await?;
    write_parts(db, out, OWN, &[user.into()]).await?;
    out.put(",\"teams\":[".into()).await?;
    let sql = "SELECT team_id, role FROM team_user WHERE user_id = $1 \
               ORDER BY created_at, team_id";
    let rows = db.query_all_raw(stmt(sql, vec![user.into()])).await?;
    for (n, row) in rows.iter().enumerate() {
        let team: Uuid = row.try_get("", "team_id")?;
        let role: String = row.try_get("", "role")?;
        out.put(format!(
            "{}{{\"role\":{},",
            if n > 0 { "," } else { "" },
            json_str(&role)
        ))
        .await?;
        match role.as_str() {
            "owner" | "admin" => write_parts(db, out, TEAM_PARTS, &[team.into()]).await?,
            "member" => write_parts(db, out, MEMBER, &[team.into(), user.into()]).await?,
            _ => {}
        }
        out.put("}".into()).await?;
    }
    // A body cut short by a failure ends without it, whatever a proxy does with the broken stream.
    out.put("],\"complete\":true}".into()).await?;
    Ok(())
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// Takes this user's one export of the hour, `None` when it is used. Locked per user, so two
/// requests at once cannot both pass.
async fn reserve(db: &DatabaseConnection, user: Uuid) -> Result<Option<i64>, DbErr> {
    let bucket = format!("export:{user}");
    let txn = db.begin().await?;
    let lock = "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))";
    txn.execute_raw(stmt(lock, vec![bucket.clone().into()]))
        .await?;
    let used = "SELECT 1 AS n FROM rate_limit_hits WHERE bucket = $1 \
                AND created_at > now() - interval '1 hour'";
    if txn
        .query_one_raw(stmt(used, vec![bucket.clone().into()]))
        .await?
        .is_some()
    {
        return Ok(None);
    }
    let sql = "INSERT INTO rate_limit_hits (bucket) VALUES ($1) RETURNING id";
    let id = match txn.query_one_raw(stmt(sql, vec![bucket.into()])).await? {
        Some(row) => row.try_get("", "id")?,
        None => return Ok(None),
    };
    txn.commit().await?;
    Ok(Some(id))
}

/// Your data as one JSON file: your profile and, for each team you belong to, what your role
/// may read. Streamed; one export an hour (`429 rate_limited` after that).
#[utoipa::path(post, path = "/account/export", tag = "account",
    responses((status = 200, content_type = "application/json", description = "The export file"),
        (status = 401, body = Error), (status = 403, body = Error), (status = 429, body = Error)))]
async fn export(
    _: FromWeb,
    State(app): State<AppState>,
    CurrentUser(user, _): CurrentUser,
) -> Response {
    let hit = match reserve(&app.db, user.id).await {
        Ok(Some(id)) => id,
        Ok(None) => return error(StatusCode::TOO_MANY_REQUESTS, "rate_limited").into_response(),
        Err(e) => return internal(e, "export reserve"),
    };
    // A few pages queued ahead of a slow reader, no more.
    let (tx, mut rx) = mpsc::channel::<Chunk>(4);
    let db = app.db.clone();
    let id = user.id;
    tokio::spawn(async move {
        let sink = Sink(tx);
        let done = export_to(&db, &sink, id).await;
        // A reader that leaves keeps its slot used (else dropping the connection would make
        // exports free); only our own failure gives it back.
        if let Err(Stop::Db(e)) = done {
            crate::rate_limit::daily_refund(&db, &[hit]).await;
            {
                tracing::error!(error = %e, "account export failed");
                let _ = sink
                    .0
                    .send(Err(std::io::Error::other("export failed")))
                    .await;
            }
        }
    });
    let stream = futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx));
    let name = format!(
        "socialreins-export-{}.json",
        chrono::Utc::now().format("%Y-%m-%d")
    );
    (
        [
            (header::CONTENT_TYPE, "application/json".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

#[derive(Deserialize, ToSchema)]
struct Confirm {
    /// The account's own email address, typed to confirm.
    email: String,
}

enum Erased {
    Done,
    /// Already deleted by an earlier request: nothing to do, nothing to send.
    Gone,
    /// Teams the person owns alone that other people belong to.
    Shared(Vec<String>),
    /// A subscription Stripe would not cancel.
    Stripe,
}

/// Deletes `user` in one transaction: the teams only they belong to (with a Stripe cancel first,
/// inside the transaction, so a failure leaves everything as it was), then the person's own
/// rows and what is keyed by their email. Locks the user and every team of theirs first, so a
/// member joining or leaving cannot change the answer while it runs. Posts already out on a
/// network are not touched; stored files of a deleted team go with the media sweep (up to 7 days).
async fn erase(app: &AppState, user: &users::Model) -> Result<Erased, DbErr> {
    let txn = app.db.begin().await?;
    let id = || vec![Value::from(user.id)];
    // Teams first, then the user: the order `DELETE /v1/team` takes them in.
    let lock =
        "SELECT id FROM teams WHERE id IN (SELECT team_id FROM team_user WHERE user_id = $1) \
                ORDER BY id FOR UPDATE";
    txn.execute_raw(stmt(lock, id())).await?;
    let locked = "SELECT id FROM users WHERE id = $1 FOR UPDATE";
    if txn.query_one_raw(stmt(locked, id())).await?.is_none() {
        // A second request for the same deletion, after the first one went through.
        return Ok(Erased::Gone);
    }
    let sql = "SELECT t.id, t.name, t.slug, tu.role, \
        (SELECT count(*) FROM team_user o WHERE o.team_id = t.id AND o.user_id <> $1) AS others, \
        (SELECT count(*) FROM team_user o WHERE o.team_id = t.id AND o.user_id <> $1 \
           AND o.role = 'owner') AS owners \
        FROM teams t JOIN team_user tu ON tu.team_id = t.id AND tu.user_id = $1 ORDER BY t.id";
    let teams = txn.query_all_raw(stmt(sql, id())).await?;
    let (mut solo, mut stuck, mut left) = (Vec::new(), Vec::new(), 0_i32);
    for row in &teams {
        let team: Uuid = row.try_get("", "id")?;
        let name: String = row.try_get("", "name")?;
        let slug: Option<String> = row.try_get("", "slug")?;
        let role: String = row.try_get("", "role")?;
        let others: i64 = row.try_get("", "others")?;
        let owners: i64 = row.try_get("", "owners")?;
        if others == 0 {
            solo.push((team, slug));
        } else if role == "owner" && owners == 0 {
            stuck.push(name);
        } else {
            left += 1;
        }
    }
    if !stuck.is_empty() {
        return Ok(Erased::Shared(stuck));
    }
    let mut subs = Vec::new();
    for (team, _) in &solo {
        let sql = "SELECT provider_subscription_id AS sub FROM subscriptions WHERE team_id = $1 \
                   AND provider = 'stripe' AND provider_subscription_id IS NOT NULL \
                   AND status <> 'canceled'";
        let Some(row) = txn.query_one_raw(stmt(sql, vec![(*team).into()])).await? else {
            continue;
        };
        subs.push(row.try_get::<String>("", "sub")?);
    }
    for (team, slug) in &solo {
        if let Some(slug) = slug {
            txn.execute_raw(stmt(RETIRE, vec![slug.clone().into(), (*team).into()]))
                .await?;
        }
        txn.execute_raw(stmt(
            "DELETE FROM teams WHERE id = $1",
            vec![(*team).into()],
        ))
        .await?;
    }
    // The trial record stays so a trial cannot be had twice, but as a hash, not the address.
    if let Some(key) = email::person_key(&user.email) {
        let sql = "UPDATE trial_uses SET team_id = NULL, \
                   person_key = encode(sha256(convert_to(person_key, 'UTF8')), 'hex') \
                   WHERE person_key = $1";
        txn.execute_raw(stmt(sql, vec![key.into()])).await?;
    }
    let mine = || vec![Value::from(user.email.clone())];
    for sql in [
        "DELETE FROM magic_links WHERE email = $1",
        "DELETE FROM waitlist_signups WHERE email = $1",
        "DELETE FROM team_invitations WHERE email = $1",
        "DELETE FROM subscribers WHERE email = $1",
    ] {
        txn.execute_raw(stmt(sql, mine())).await?;
    }
    // Tokens the person made in teams that go on without them; sessions, passkeys and
    // memberships go with the user row.
    txn.execute_raw(stmt("DELETE FROM api_tokens WHERE created_by = $1", id()))
        .await?;
    txn.execute_raw(stmt("DELETE FROM users WHERE id = $1", id()))
        .await?;
    // Last, so what can fail in the database fails before the one step that cannot be undone. A
    // second team's refusal after a first's cancel still leaves that one cancelled: a retry
    // finds it ended (`resource_missing`) and goes on.
    let mut cancelled = 0_i32;
    for sub in &subs {
        match app.stripe.cancel(sub).await {
            Ok(()) => cancelled += 1,
            // Stripe has already ended it, which is what we wanted.
            Err(e) if e.1 == "resource_missing" => {}
            Err(e) => {
                tracing::error!(status = ?e.0, code = %e.1, "account delete: stripe cancel failed");
                return Ok(Erased::Stripe);
            }
        }
    }
    let audit =
        "INSERT INTO account_deletions (teams_deleted, teams_left, subscriptions_cancelled) \
                 VALUES ($1, $2, $3)";
    let counts = vec![(solo.len() as i32).into(), left.into(), cancelled.into()];
    txn.execute_raw(stmt(audit, counts)).await?;
    txn.commit().await?;
    Ok(Erased::Done)
}

/// Deletes the signed-in user's account and everything only they hold, for good. Needs a
/// sign-in from the last 10 minutes (`403 reauth_required`) and the account's email in the body
/// (`422 email_mismatch`). A team the person alone owns but others belong to must be handed over
/// or deleted first (`409 owns_shared_team` with the team names); a team only they belong to is
/// deleted with them, its Stripe plan cancelled last, before the commit (`502 stripe_failed`, nothing deleted).
/// Sessions end, a mail confirms it. The App Review demo user cannot (`403 review_account`).
#[utoipa::path(delete, path = "/account", tag = "account", request_body = Confirm,
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 409, body = Error), (status = 422, body = Error), (status = 502, body = Error)))]
async fn destroy(
    _: FromWeb,
    State(app): State<AppState>,
    current: CurrentUser,
    JsonBody(body): JsonBody<Confirm>,
) -> Response {
    let user = current.0.clone();
    if is_demo_email(&user.email) {
        return error(StatusCode::FORBIDDEN, "review_account").into_response();
    }
    if !current.recent() {
        return error(StatusCode::FORBIDDEN, "reauth_required").into_response();
    }
    if body.email.trim().to_lowercase() != user.email {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "email_mismatch").into_response();
    }
    match erase(&app, &user).await {
        Ok(Erased::Gone) => {
            let clear = named_cookie(&app, SESSION_COOKIE, "/", "", 0);
            return (StatusCode::NO_CONTENT, [(header::SET_COOKIE, clear)]).into_response();
        }
        Ok(Erased::Done) => {}
        Ok(Erased::Shared(teams)) => {
            let body = serde_json::json!({"error": "owns_shared_team", "teams": teams});
            return (StatusCode::CONFLICT, Json(body)).into_response();
        }
        Ok(Erased::Stripe) => {
            return error(StatusCode::BAD_GATEWAY, "stripe_failed").into_response()
        }
        Err(e) => return internal(e, "account delete"),
    }
    let sending = crate::mail::Sending::begin();
    let app2 = app.clone();
    tokio::spawn(async move {
        let _sending = sending;
        let text = "Your SocialReins account was deleted at your request, with the teams only you \
            belonged to, their connected accounts, posts and settings, and your sign-in methods. \
            Posts already published on social networks are not touched.\n\nCopies in our encrypted \
            backups are gone within 30 days. A paid plan, if you had one, was cancelled; Stripe \
            keeps its own payment records.\n\nIf you did not ask for this, write to \
            info@socialreins.com now.\n";
        let sent = app2
            .mailer
            .send(
                &user.email,
                "Your SocialReins account was deleted",
                text.into(),
                &app2.web_url,
            )
            .await;
        if let Err(e) = sent {
            tracing::warn!(error = %e, "account deletion mail not sent");
        }
    });
    let clear = named_cookie(&app, SESSION_COOKIE, "/", "", 0);
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, clear)]).into_response()
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(export))
        .routes(routes!(destroy))
}
