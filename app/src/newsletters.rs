//! Newsletters: one email to every confirmed subscriber of a team who did not unsubscribe.
//! A start (`start`) snapshots the audience into `newsletter_recipients`; `tick` (every minute,
//! own advisory lock) then claims up to 50 pending recipients of each sending newsletter in one
//! short transaction (`claim`, with each mail's hit of the cap over all teams), commits, and
//! only then sends. A recipient moves `pending` to `sending` (claimed) to `mailing` (committed
//! right before the mail call) to an end state, and never back once it was `mailing`, so nobody
//! is mailed a newsletter twice: one cut off mid-mail ends `failed: interrupted`, one only
//! claimed goes back to `pending` (`recover`). Never mail a recipient without claiming it, and never
//! retry one. Any error of the mail provider (a rate limit, a refusal, an outage) pauses the
//! newsletter at once, as do 5 failed sends in a row: `paused` waits for a person (`resume`),
//! it never goes on by itself. The mail comes from our own mail domain, so everything here
//! errs on the side of not sending.
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use sea_orm::{
    ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement, TransactionTrait, Value,
};
use socialreins_core::{team::inert, token};
use uuid::Uuid;

use crate::{
    mail::{escape, List},
    rate_limit::{daily_refund, reserve_in},
    state::AppState,
};

pub const MAX_SUBJECT: usize = 150;
pub const MAX_BODY: usize = 20_000;
/// Most links a body may hold.
pub const MAX_LINKS: usize = 20;
/// Most recipients of one newsletter.
pub const MAX_RECIPIENTS: u64 = 5000;
/// Newsletters a team starts in 24 hours.
pub const STARTS_PER_DAY: i64 = 1;
/// Test mails a team sends in 24 hours.
pub const TESTS_PER_DAY: i64 = 10;
/// Newsletter mails all teams together send in 24 hours; past it, recipients wait as `pending`.
pub const MAILS_PER_DAY: i64 = 10_000;
pub const MAIL_BUCKET: &str = "newsletter:mail";
/// Recipients claimed per sending newsletter per round.
const BATCH: i64 = 50;
/// This many sends of a newsletter failing in a row pause it.
const FAILS_IN_A_ROW: i64 = 5;
/// A recipient `sending` this long was cut off; far above `ROUND` plus the mailer's timeout.
const STUCK_MINUTES: i32 = 15;
const NEWSLETTER_LOCK: i64 = 0x004e_4557_534c_5452;
/// How long a round may start sends; recipients it did not get to go back to `pending`.
pub const ROUND: Duration = Duration::from_secs(45);

fn start_bucket(team: Uuid) -> String {
    format!("newsletter:start:team:{team}")
}

pub fn test_bucket(team: Uuid) -> String {
    format!("newsletter:test:team:{team}")
}

fn sql(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// The subject and body as stored, or the field `422 invalid_newsletter` names. The subject is
/// one line of 1 to 150 characters; the body 1 to 20000 characters of text (line breaks and
/// tabs the only control characters) with at most 20 links.
pub fn checked(subject: &str, body: &str) -> Result<(String, String), &'static str> {
    let subject = subject.trim();
    if !(1..=MAX_SUBJECT).contains(&subject.chars().count())
        || subject.chars().any(char::is_control)
    {
        return Err("subject");
    }
    let body = body.replace("\r\n", "\n").replace('\r', "\n");
    let body = body.trim();
    let bad = |c: char| c.is_control() && c != '\n' && c != '\t';
    if !(1..=MAX_BODY).contains(&body.chars().count())
        || body.chars().any(bad)
        || render(body).1 > MAX_LINKS
    {
        return Err("body");
    }
    Ok((subject.to_owned(), body.to_owned()))
}

/// The body as HTML paragraphs, and how many links it holds. The whole markup: a blank line
/// ends a paragraph, a line break stays one, `**text**` within a paragraph is bold, and a bare
/// `https://` address is a link whose text is that address. Every other character is escaped,
/// so no HTML of the writer's passes through.
pub fn render(body: &str) -> (String, usize) {
    let mut html = String::new();
    let mut links = 0;
    let mut paragraph = Vec::new();
    for line in body.lines().map(str::trim).chain([""]) {
        if !line.is_empty() {
            paragraph.push(line);
        } else if !paragraph.is_empty() {
            let text = paragraph.join("\n");
            let parts: Vec<&str> = text.split("**").collect();
            html.push_str("<p style=\"margin:0 0 16px\">");
            for (i, part) in parts.iter().enumerate() {
                // A marker without its pair stays as typed.
                let bold = i % 2 == 1 && i + 1 < parts.len();
                html.push_str(match (bold, i % 2) {
                    (true, _) => "<strong>",
                    (false, 1) => "**",
                    _ => "",
                });
                links += linked(part, &mut html);
                html.push_str(if bold { "</strong>" } else { "" });
            }
            html.push_str("</p>");
            paragraph.clear();
        }
    }
    (html.replace('\n', "<br>"), links)
}

/// Appends `text` escaped, each bare `https://` address in it as a link to itself; how many.
/// An address ends at white space, a quote or an angle bracket, and gives up the punctuation
/// that ends a sentence.
fn linked(text: &str, html: &mut String) -> usize {
    const SCHEME: &str = "https://";
    let mut links = 0;
    let mut rest = text;
    while let Some(at) = rest.find(SCHEME) {
        let (before, tail) = rest.split_at(at);
        let end = tail
            .find(|c: char| c.is_whitespace() || "<>\"'`".contains(c))
            .unwrap_or(tail.len());
        let url = tail[..end].trim_end_matches(|c: char| ".,;:!?)]}".contains(c));
        html.push_str(&escape(before));
        let host = url[SCHEME.len()..].chars().next();
        if host.is_some_and(char::is_alphanumeric) {
            let url = escape(url);
            html.push_str(&format!("<a href=\"{url}\">{url}</a>"));
            links += 1;
        } else {
            html.push_str(&escape(url));
        }
        rest = &tail[url.len()..];
    }
    html.push_str(&escape(rest));
    links
}

/// Who a newsletter is from and for.
pub struct Envelope<'a> {
    pub subject: &'a str,
    pub body: &'a str,
    pub sender_name: &'a str,
    /// The sender's postal address, which the law wants in every mail.
    pub sender_address: &'a str,
    /// The sender's logo, a public `https` image link, shown above the mail instead of the name.
    pub sender_logo: Option<&'a str>,
    pub team: &'a str,
}

/// The mail's plain text and its HTML: the body, then a footer with the sender, their postal
/// address, why the reader gets this, and a link to `page`, the web page whose button
/// unsubscribes (a link in a mail must never do it by itself: mail scanners open links).
/// The HTML is laid out as `mail::html` is: the sender's logo (else their name) above a white
/// card with the subject and the body, the footer below it. Tables and inline styles, because
/// mail clients ignore most CSS.
pub fn compose(e: &Envelope<'_>, page: &str) -> (String, String) {
    // The team's name is its own text: `inert`, so it cannot pass as a link in our mail.
    let why = format!(
        "You get this because you signed up on a {} page.",
        inert(e.team).trim()
    );
    let text = format!(
        "{}\n\n-- \n{}\n{}\n\n{why}\nUnsubscribe: {}\n",
        e.body, e.sender_name, e.sender_address, page
    );
    let name = escape(e.sender_name);
    let header = match e.sender_logo {
        Some(logo) => format!(
            "<img src=\"{}\" height=\"40\" alt=\"{name}\" style=\"display:block;border:0;\
             height:40px;width:auto;max-width:240px\">",
            escape(logo)
        ),
        None => name.clone(),
    };
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta name=\"color-scheme\" content=\"light\">\
         <meta name=\"supported-color-schemes\" content=\"light\"><title>{title}</title></head>\
         <body style=\"margin:0;padding:0;background:#faf8f6\">\
         <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" \
         style=\"background:#faf8f6\"><tr><td align=\"center\" style=\"padding:32px 16px\">\
         <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" \
         style=\"max-width:560px;text-align:left;font-family:-apple-system,BlinkMacSystemFont,\
         'Segoe UI',Roboto,Helvetica,Arial,sans-serif;color:#121212\">\
         <tr><td style=\"padding:0 4px 16px;font-size:20px;font-weight:800;\
         letter-spacing:-0.02em\">{header}</td></tr>\
         <tr><td style=\"background:#ffffff;border:1px solid #e7e5e4;border-radius:16px;\
         padding:32px 28px;font-size:16px;line-height:1.55\">\
         <h1 style=\"margin:0 0 20px;font-size:24px;line-height:1.25;font-weight:800;\
         letter-spacing:-0.02em\">{title}</h1>{body}</td></tr>\
         <tr><td style=\"padding:16px 4px 0;font-size:13px;line-height:1.5;color:#57534e\">\
         {name}<br>{address}<br><br>{why}<br>\
         <a href=\"{page}\" style=\"color:#57534e\">Unsubscribe</a></td></tr>\
         </table></td></tr></table></body></html>",
        title = escape(e.subject),
        body = render(e.body).0,
        address = escape(e.sender_address).replace('\n', "<br>"),
        why = escape(&why),
        page = escape(page),
    );
    (text, html)
}

/// Sends one newsletter mail to `to`, whose unsubscribe links carry `secret`. Errors are the
/// mailer's: the failure kind and status only.
pub async fn mail(app: &AppState, to: &str, secret: &str, e: &Envelope<'_>) -> Result<(), String> {
    let page = format!("{}/newsletter/unsubscribe?token={secret}", app.web_url);
    let one_click = format!(
        "{}/v1/public/newsletter/unsubscribe?token={secret}",
        app.brain_url
    );
    let (text, html) = compose(e, &page);
    let list = List {
        from_name: e.sender_name,
        unsubscribe: &one_click,
    };
    app.mailer.send_list(to, list, e.subject, text, html).await
}

/// What `start` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Start {
    Started,
    NotFound,
    /// It is no draft.
    AlreadyStarted,
    /// The team has no sender details.
    SenderRequired,
    /// No test mail went out since the last edit.
    TestRequired,
    NoRecipients,
    /// Over `MAX_RECIPIENTS`.
    TooManyRecipients,
    /// The audience is this many, not the confirmed number.
    AudienceChanged(u64),
    /// `STARTS_PER_DAY` reached.
    TooManyToday,
}

/// Starts a draft: copies the team's sender details onto it, snapshots every confirmed
/// subscriber who did not unsubscribe into `newsletter_recipients` and sets it `sending`, all
/// in one transaction, and only when the snapshot holds exactly `confirm` people. Anything but
/// `Started` leaves no trace.
pub async fn start(
    db: &DatabaseConnection,
    team: Uuid,
    id: Uuid,
    confirm: u64,
) -> Result<Start, DbErr> {
    let txn = db.begin().await?;
    let found = sql(
        "SELECT n.status, n.tested_at IS NOT NULL AS tested, t.newsletter_sender_name AS name, \
         t.newsletter_sender_address AS address, t.newsletter_sender_logo AS logo \
         FROM newsletters n \
         JOIN teams t ON t.id = n.team_id WHERE n.id = $1 AND n.team_id = $2 FOR UPDATE OF n",
        vec![id.into(), team.into()],
    );
    let Some(row) = txn.query_one_raw(found).await? else {
        return Ok(Start::NotFound);
    };
    if row.try_get::<String>("", "status")? != "draft" {
        return Ok(Start::AlreadyStarted);
    }
    let sender: (Option<String>, Option<String>) =
        (row.try_get("", "name")?, row.try_get("", "address")?);
    let (Some(name), Some(address)) = sender else {
        return Ok(Start::SenderRequired);
    };
    let logo: Option<String> = row.try_get("", "logo")?;
    if !row.try_get::<bool>("", "tested")? {
        return Ok(Start::TestRequired);
    }
    // The snapshot is the count: one statement reads the audience and stores it.
    let snapshot = sql(
        "INSERT INTO newsletter_recipients (id, newsletter_id, subscriber_id) \
         SELECT gen_random_uuid(), $2, s.id FROM subscribers s WHERE s.team_id = $1 \
         AND s.verified_at IS NOT NULL AND s.unsubscribed_at IS NULL LIMIT $3",
        vec![team.into(), id.into(), (MAX_RECIPIENTS as i64 + 1).into()],
    );
    let n = txn.execute_raw(snapshot).await?.rows_affected();
    if n == 0 {
        return Ok(Start::NoRecipients);
    }
    if n > MAX_RECIPIENTS {
        return Ok(Start::TooManyRecipients);
    }
    if n != confirm {
        return Ok(Start::AudienceChanged(n));
    }
    let cap = [(start_bucket(team), STARTS_PER_DAY)];
    if reserve_in(&txn, &cap).await?.is_none() {
        return Ok(Start::TooManyToday);
    }
    let go = sql(
        "UPDATE newsletters SET status = 'sending', started_at = now(), sender_name = $2, \
         sender_address = $3, sender_logo = $4 WHERE id = $1",
        vec![id.into(), name.into(), address.into(), logo.into()],
    );
    txn.execute_raw(go).await?;
    txn.commit().await?;
    Ok(Start::Started)
}

/// Cancels a `sending` or `paused` newsletter: its pending recipients are `skipped: cancelled`.
/// `None` when the team has no such newsletter, `false` when it is neither. A recipient already
/// claimed ends on its own: `attempt` looks at the newsletter again before it sends.
pub async fn cancel(db: &DatabaseConnection, team: Uuid, id: Uuid) -> Result<Option<bool>, DbErr> {
    let txn = db.begin().await?;
    let key: Vec<Value> = vec![id.into(), team.into()];
    let stop = sql(
        "UPDATE newsletters SET status = 'cancelled', pause_reason = NULL, finished_at = now() \
         WHERE id = $1 AND team_id = $2 AND status IN ('sending', 'paused')",
        key.clone(),
    );
    if txn.execute_raw(stop).await?.rows_affected() != 1 {
        let exists = sql(
            "SELECT 1 FROM newsletters WHERE id = $1 AND team_id = $2",
            key,
        );
        return Ok(txn.query_one_raw(exists).await?.map(|_| false));
    }
    let skip = sql(
        "UPDATE newsletter_recipients SET status = 'skipped', reason = 'cancelled', \
         updated_at = now() WHERE newsletter_id = $1 AND status = 'pending'",
        vec![id.into()],
    );
    txn.execute_raw(skip).await?;
    txn.commit().await?;
    Ok(Some(true))
}

/// Puts a `paused` newsletter back to `sending`; its run of failures counts from now
/// (`resumed_at`). `None` when the team has no such newsletter, `false` when it is not paused.
/// It is no new start: the one-a-day rule does not count it.
pub async fn resume(db: &DatabaseConnection, team: Uuid, id: Uuid) -> Result<Option<bool>, DbErr> {
    let key: Vec<Value> = vec![id.into(), team.into()];
    let go = sql(
        "UPDATE newsletters SET status = 'sending', pause_reason = NULL, resumed_at = now() \
         WHERE id = $1 AND team_id = $2 AND status = 'paused'",
        key.clone(),
    );
    if db.execute_raw(go).await?.rows_affected() == 1 {
        return Ok(Some(true));
    }
    let exists = sql(
        "SELECT 1 FROM newsletters WHERE id = $1 AND team_id = $2",
        key,
    );
    Ok(db.query_one_raw(exists).await?.map(|_| false))
}

/// Pauses a `sending` newsletter; `why` is for the person who must look at it.
async fn pause(db: &impl ConnectionTrait, newsletter: Uuid, why: &str) -> Result<(), DbErr> {
    let stmt = sql(
        "UPDATE newsletters SET status = 'paused', pause_reason = $2 \
         WHERE id = $1 AND status = 'sending'",
        vec![newsletter.into(), why.into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// Pauses every sending newsletter (or only `newsletter`) whose last `FAILS_IN_A_ROW` sends
/// since it started or was resumed all failed, whatever the reason; whether any was paused.
async fn pause_failing(db: &impl ConnectionTrait, newsletter: Option<Uuid>) -> Result<bool, DbErr> {
    let stmt = sql(
        "UPDATE newsletters n SET status = 'paused', pause_reason = $2 \
         WHERE n.status = 'sending' AND ($3::uuid IS NULL OR n.id = $3) \
         AND (SELECT count(*) FROM (SELECT r.status FROM newsletter_recipients r \
              WHERE r.newsletter_id = n.id AND r.status IN ('sent', 'failed') \
              AND r.updated_at >= COALESCE(n.resumed_at, n.started_at) \
              ORDER BY r.updated_at DESC, r.id LIMIT $1) l WHERE l.status = 'failed') = $1",
        vec![
            FAILS_IN_A_ROW.into(),
            format!("{FAILS_IN_A_ROW} sends in a row failed.").into(),
            newsletter.into(),
        ],
    );
    Ok(db.execute_raw(stmt).await?.rows_affected() > 0)
}

/// A claimed recipient: `sending`, with its hit of the cap over all teams taken.
struct Claimed {
    id: Uuid,
    newsletter: Uuid,
    subscriber: Option<Uuid>,
    subject: String,
    body: String,
    sender_name: String,
    sender_address: String,
    sender_logo: Option<String>,
    team: String,
    hits: Vec<i64>,
}

/// Hands the claimed (`sending`) recipients matching the `WHERE` appended to it back: nothing
/// was sent to them, so they are `pending` again, or `skipped: cancelled` when their newsletter
/// was cancelled meanwhile (a cancelled one never shows anybody pending).
const HAND_BACK: &str = "UPDATE newsletter_recipients r SET updated_at = now(), \
    status = CASE WHEN n.status = 'cancelled' THEN 'skipped' ELSE 'pending' END, \
    reason = CASE WHEN n.status = 'cancelled' THEN 'cancelled' END \
    FROM newsletters n WHERE n.id = r.newsletter_id AND r.status = 'sending'";

/// What a process that died left behind. A recipient whose mail was started (`mailing`) fails
/// as `interrupted` and is never mailed again; one only claimed (`sending`) got nothing, so it
/// is handed back.
async fn recover(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    let stale = "updated_at < now() - make_interval(mins => $1)";
    let cut = format!(
        "UPDATE newsletter_recipients SET status = 'failed', reason = 'interrupted', \
         updated_at = now() WHERE status = 'mailing' AND {stale}"
    );
    db.execute_raw(sql(&cut, vec![STUCK_MINUTES.into()]))
        .await?;
    let back = format!("{HAND_BACK} AND r.{stale}");
    db.execute_raw(sql(&back, vec![STUCK_MINUTES.into()]))
        .await
        .map(|_| ())
}

/// Moves up to `BATCH` pending recipients of every sending newsletter to `sending`, each with
/// its hit of `MAILS_PER_DAY`. Call it in a transaction that commits before any send:
/// `SKIP LOCKED` keeps two callers off the same recipient. At the cap the rest stay `pending`,
/// for a later round.
async fn claim(txn: &impl ConnectionTrait) -> Result<Vec<Claimed>, DbErr> {
    // A started newsletter always has its sender (`newsletters_sender_check`).
    let sending =
        "SELECT n.id, n.subject, n.body, n.sender_name, n.sender_address, n.sender_logo, \
                   t.name AS team FROM newsletters n JOIN teams t ON t.id = n.team_id \
                   WHERE n.status = 'sending' ORDER BY n.started_at";
    let cap = [(MAIL_BUCKET.to_owned(), MAILS_PER_DAY)];
    let mut out = Vec::new();
    for n in txn.query_all_raw(sql(sending, vec![])).await? {
        let newsletter: Uuid = n.try_get("", "id")?;
        let pending = sql(
            "SELECT id, subscriber_id FROM newsletter_recipients WHERE newsletter_id = $1 \
             AND status = 'pending' ORDER BY id LIMIT $2 FOR UPDATE SKIP LOCKED",
            vec![newsletter.into(), BATCH.into()],
        );
        for r in txn.query_all_raw(pending).await? {
            let Some(hits) = reserve_in(txn, &cap).await? else {
                return Ok(out);
            };
            let id: Uuid = r.try_get("", "id")?;
            let take = sql(
                "UPDATE newsletter_recipients SET status = 'sending', updated_at = now() \
                 WHERE id = $1",
                vec![id.into()],
            );
            txn.execute_raw(take).await?;
            out.push(Claimed {
                id,
                newsletter,
                subscriber: r.try_get("", "subscriber_id")?,
                subject: n.try_get("", "subject")?,
                body: n.try_get("", "body")?,
                sender_name: n.try_get("", "sender_name")?,
                sender_address: n.try_get("", "sender_address")?,
                sender_logo: n.try_get("", "sender_logo")?,
                team: n.try_get("", "team")?,
                hits,
            });
        }
    }
    Ok(out)
}

/// What became of a claimed recipient.
enum Tried {
    /// Its end state, and why when it was not sent.
    End(&'static str, Option<String>),
    /// The newsletter was paused meanwhile: nothing was sent, it is `pending` again.
    Back,
    /// The claim is no longer ours (`recover` took it for dead): nothing was sent, and the row
    /// is somebody else's now.
    Lost,
    /// The send failed in a way that would meet the next recipient too: the recipient is
    /// `failed` with the first text, and the newsletter pauses with the second.
    Trip(String, String),
}

fn skipped(reason: &str) -> Tried {
    Tried::End("skipped", Some(reason.to_owned()))
}

/// The subscriber's email and the token of their unsubscribe links, made now if they had none;
/// `Err` says why they get no mail. One statement, so two sends cannot make two tokens.
async fn addressee(
    app: &AppState,
    subscriber: Uuid,
) -> Result<Result<(String, Option<String>), &'static str>, DbErr> {
    let fresh = token::new();
    let stmt = sql(
        "UPDATE subscribers SET unsubscribe_token = COALESCE(unsubscribe_token, $2), \
         unsubscribe_token_hash = COALESCE(unsubscribe_token_hash, $3) WHERE id = $1 \
         RETURNING email, unsubscribe_token, verified_at IS NOT NULL AS confirmed, \
         unsubscribed_at IS NOT NULL AS unsubscribed",
        vec![
            subscriber.into(),
            app.cipher.encrypt(fresh.as_bytes()).into(),
            token::hash(&fresh).into(),
        ],
    );
    let Some(row) = app.db.query_one_raw(stmt).await? else {
        return Ok(Err("subscriber_gone"));
    };
    if row.try_get::<bool>("", "unsubscribed")? {
        return Ok(Err("unsubscribed"));
    }
    if !row.try_get::<bool>("", "confirmed")? {
        return Ok(Err("unconfirmed"));
    }
    let sealed: Vec<u8> = row.try_get("", "unsubscribe_token")?;
    let secret = app.cipher.decrypt(&sealed);
    let secret = secret.and_then(|bytes| String::from_utf8(bytes).ok());
    Ok(Ok((row.try_get("", "email")?, secret)))
}

/// Mails one claimed recipient, once. First the newsletter must still be sending and the
/// subscriber still be there, confirmed and not unsubscribed.
async fn attempt(app: &AppState, c: &Claimed) -> Result<Tried, DbErr> {
    let live = sql(
        "SELECT status FROM newsletters WHERE id = $1",
        vec![c.newsletter.into()],
    );
    let status: Option<String> = match app.db.query_one_raw(live).await? {
        Some(r) => r.try_get("", "status")?,
        None => None,
    };
    match status.as_deref() {
        Some("sending") => {}
        Some("paused") => return Ok(Tried::Back),
        _ => return Ok(skipped("cancelled")),
    }
    let Some(subscriber) = c.subscriber else {
        return Ok(skipped("subscriber_gone"));
    };
    let (email, secret) = match addressee(app, subscriber).await? {
        Ok(found) => found,
        Err(reason) => return Ok(skipped(reason)),
    };
    // Sealed under another `ENCRYPTION_KEY`: no mail goes out without a working link.
    let Some(secret) = secret else {
        let why = "An unsubscribe link could not be read (did ENCRYPTION_KEY change?).";
        return Ok(Tried::Trip("no_unsubscribe_link".into(), why.into()));
    };
    let envelope = Envelope {
        subject: &c.subject,
        body: &c.body,
        sender_name: &c.sender_name,
        sender_address: &c.sender_address,
        sender_logo: c.sender_logo.as_deref(),
        team: &c.team,
    };
    // Committed before the mail starts: from here on the recipient counts as mailed, whatever
    // becomes of this process. Only one holder of a claim gets through.
    let mark = sql(
        "UPDATE newsletter_recipients SET status = 'mailing', updated_at = now() \
         WHERE id = $1 AND status = 'sending'",
        vec![c.id.into()],
    );
    if app.db.execute_raw(mark).await?.rows_affected() != 1 {
        return Ok(Tried::Lost);
    }
    Ok(match mail(app, &email, &secret, &envelope).await {
        Ok(()) => Tried::End("sent", None),
        // It may have gone out (a timeout): never again. Whatever the provider said would meet
        // the next recipient too, and each further try costs the mail domain: stop here.
        Err(e) => {
            let why = if e.contains("429") {
                format!("The mail provider is rate limiting us ({e}).")
            } else {
                format!("The mail provider did not take a mail ({e}).")
            };
            Tried::Trip(e, why)
        }
    })
}

/// `attempt`, then the recipient's end state; `true` when the newsletter was paused over it. A
/// skip gives its hit of the cap back.
async fn deliver(app: &AppState, c: &Claimed) -> Result<bool, DbErr> {
    let db = &app.db;
    let (status, reason, trip) = match attempt(app, c).await {
        Ok(Tried::Back) => return release(db, c).await.map(|()| false),
        Ok(Tried::Lost) => {
            daily_refund(db, &c.hits).await;
            return Ok(false);
        }
        Ok(Tried::End(status, reason)) => (status, reason, None),
        Ok(Tried::Trip(reason, why)) => ("failed", Some(reason), Some(why)),
        // Before the send: every query of `attempt` comes before it.
        Err(e) => {
            tracing::error!(error = %e, recipient = %c.id, "newsletter recipient check failed");
            ("failed", Some("internal".to_owned()), None)
        }
    };
    // First of all: the others must stop even if this recipient's own row cannot be written.
    if let Some(why) = &trip {
        pause(db, c.newsletter, why).await?;
    }
    let end = sql(
        "UPDATE newsletter_recipients SET status = $2, reason = $3, updated_at = now() \
         WHERE id = $1 AND status IN ('sending', 'mailing')",
        vec![c.id.into(), status.into(), reason.into()],
    );
    db.execute_raw(end).await?;
    if status == "skipped" {
        daily_refund(db, &c.hits).await;
    }
    Ok(trip.is_some() || (status == "failed" && pause_failing(db, Some(c.newsletter)).await?))
}

/// Hands back a recipient nothing was sent to (the round ran out of time, or the newsletter was
/// paused or cancelled): see `HAND_BACK`.
async fn release(db: &DatabaseConnection, c: &Claimed) -> Result<(), DbErr> {
    let back = format!("{HAND_BACK} AND r.id = $1");
    db.execute_raw(sql(&back, vec![c.id.into()])).await?;
    daily_refund(db, &c.hits).await;
    Ok(())
}

/// Mails the claimed recipients: newsletters side by side, one's recipients one after another,
/// until one of them pauses it; the rest are then handed back.
async fn work(app: &AppState, claimed: Vec<Claimed>, budget: Duration) {
    let started = Instant::now();
    let mut by_newsletter: HashMap<Uuid, Vec<Claimed>> = HashMap::new();
    for c in claimed {
        by_newsletter.entry(c.newsletter).or_default().push(c);
    }
    let runs: Vec<_> = by_newsletter
        .into_values()
        .map(|list| {
            let app = app.clone();
            tokio::spawn(async move {
                let mut paused = false;
                for c in &list {
                    let done = if paused || started.elapsed() >= budget {
                        release(&app.db, c).await
                    } else {
                        deliver(&app, c).await.map(|p| paused = p)
                    };
                    if let Err(e) = done {
                        tracing::error!(error = %e, recipient = %c.id, "newsletter recipient not settled");
                    }
                }
            })
        })
        .collect();
    for run in runs {
        let _ = run.await;
    }
}

/// A sending newsletter with nobody left to mail is `sent`; so is a paused one, which has
/// nothing to resume.
async fn finish(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    let stmt = "UPDATE newsletters n SET status = 'sent', pause_reason = NULL, \
                finished_at = now() WHERE n.status IN ('sending', 'paused') AND NOT EXISTS \
                (SELECT 1 FROM newsletter_recipients r WHERE r.newsletter_id = n.id \
                AND r.status IN ('pending', 'sending', 'mailing'))";
    db.execute_raw(sql(stmt, vec![])).await.map(|_| ())
}

/// One round of newsletters. Skips while another instance is claiming. The lock and its
/// transaction last for the recovery and the claim only, never for a send.
pub async fn tick(app: &AppState) -> Result<(), DbErr> {
    let lock = app.db.begin().await?;
    let row = lock
        .query_one_raw(sql(
            "SELECT pg_try_advisory_xact_lock($1) AS ok",
            vec![NEWSLETTER_LOCK.into()],
        ))
        .await?;
    let locked: bool = match row {
        Some(r) => r.try_get("", "ok")?,
        None => false,
    };
    if !locked {
        return Ok(());
    }
    recover(&lock).await?;
    // Recovered recipients are failures too: a newsletter that keeps being cut off stops.
    pause_failing(&lock, None).await?;
    let claimed = claim(&lock).await?;
    lock.commit().await?;
    work(app, claimed, ROUND).await;
    finish(&app.db).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_markup_is_paragraphs_bold_and_https_links_and_nothing_else() {
        let body = "Hi <b>you</b> & \"all\"\nsecond line\n\n \n**Big** news at \
                    https://x.test/a?b=1&c=2. Not http://x.test or javascript:alert(1)\n\n\
                    **open <script>alert(1)</script>";
        let (html, links) = render(body);
        assert_eq!(
            html,
            "<p style=\"margin:0 0 16px\">Hi &lt;b&gt;you&lt;/b&gt; &amp; &quot;all&quot;<br>\
             second line</p><p style=\"margin:0 0 16px\"><strong>Big</strong> news at \
             <a href=\"https://x.test/a?b=1&amp;c=2\">https://x.test/a?b=1&amp;c=2</a>. \
             Not http://x.test or javascript:alert(1)</p>\
             <p style=\"margin:0 0 16px\">**open &lt;script&gt;alert(1)&lt;/script&gt;</p>"
        );
        assert_eq!(links, 1);
        // An address cannot leave its attribute, and a bare scheme is no link.
        let (html, links) = render("https://x.test/\"onclick=\"a <https://y.test> https:// **b");
        assert_eq!(links, 2);
        assert!(html.contains("<a href=\"https://x.test/\">https://x.test/</a>&quot;onclick="));
        assert!(html.contains("&lt;<a href=\"https://y.test\">https://y.test</a>&gt; https:// "));
        assert_eq!(html.matches('<').count(), 6, "{html}");
    }

    #[test]
    fn subjects_and_bodies_are_checked() {
        let ok = checked(" Hello ", " a\r\nb\r\n").unwrap();
        assert_eq!(ok, ("Hello".to_owned(), "a\nb".to_owned()));
        assert!(checked(&"s".repeat(150), &"b".repeat(20_000)).is_ok());
        let links = |n: usize| "https://x.test ".repeat(n);
        assert!(checked("s", &links(20)).is_ok());
        for (subject, body, field) in [
            (" ", "b", "subject"),
            ("a\nb", "b", "subject"),
            ("a\u{7}", "b", "subject"),
            (&*"s".repeat(151), "b", "subject"),
            ("s", " \n ", "body"),
            ("s", "a\u{0}b", "body"),
            ("s", &*"b".repeat(20_001), "body"),
            ("s", &*links(21), "body"),
        ] {
            assert_eq!(checked(subject, body).err(), Some(field));
        }
    }

    #[test]
    fn the_mail_ends_with_the_sender_the_address_and_the_unsubscribe_page() {
        let envelope = Envelope {
            subject: "News <1>",
            body: "Hello",
            sender_name: "Ann & Co",
            sender_address: "1 Main St\nSpringfield",
            sender_logo: None,
            team: "Ann's evil.example/x",
        };
        let page = "https://web.test/newsletter/unsubscribe?token=t";
        let (text, html) = compose(&envelope, page);
        assert!(text.starts_with("Hello\n\n-- \nAnn & Co\n1 Main St\nSpringfield\n\n"));
        let why = "You get this because you signed up on a Ann's evil example x page.";
        assert!(text.contains(&format!("\n\n{why}\nUnsubscribe: ")));
        assert!(text.ends_with("Unsubscribe: https://web.test/newsletter/unsubscribe?token=t\n"));
        assert!(html.contains("<title>News &lt;1&gt;</title>"));
        assert!(html.contains("Ann &amp; Co<br>1 Main St<br>Springfield<br>"));
        assert!(html.contains(&format!("<br><br>{why}<br>")));
        let unsubscribe = "<a href=\"https://web.test/newsletter/unsubscribe?token=t\"";
        assert!(html.contains(unsubscribe));
        // No logo: the sender's name is the header, and the subject heads the card.
        assert!(html.contains("letter-spacing:-0.02em\">Ann &amp; Co</td>"));
        assert!(!html.contains("<img"));
        assert!(html.contains(">News &lt;1&gt;</h1><p style=\"margin:0 0 16px\">Hello</p>"));

        // With a logo the header is the image, and nothing of the footer or the text changes.
        let logo = "https://cdn.test/a.png?x=1&y=\"><script>alert(1)</script>";
        let with = Envelope {
            sender_logo: Some(logo),
            ..envelope
        };
        let (same, html) = compose(&with, page);
        assert_eq!(same, text);
        // The link cannot leave its attribute: no quote or angle bracket of its own survives.
        let src = "<img src=\"https://cdn.test/a.png?x=1&amp;y=&quot;&gt;&lt;script&gt;\
                   alert(1)&lt;/script&gt;\" height=\"40\" alt=\"Ann &amp; Co\" style=";
        assert!(html.contains(src), "{html}");
        assert!(!html.contains("<script") && html.matches("<img").count() == 1);
        assert!(!html.contains("letter-spacing:-0.02em\">Ann &amp; Co</td>"));
        assert!(html.contains("Ann &amp; Co<br>1 Main St<br>Springfield<br>"));
        assert!(html.contains(&format!("<br><br>{why}<br>")) && html.contains(unsubscribe));
    }
}
