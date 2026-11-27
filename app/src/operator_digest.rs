//! The operator digest: one mail a day, at 08:00 Asia/Dhaka (no daylight saving), to the
//! addresses in `ADMIN_EMAILS`, about every team's failed posts of the last 24 hours (decision
//! 0008 mails only a post's author and owners). It counts posts that failed, partly failed, went
//! back to draft for being late or stopped repeating (`post_notices`, by network), lists the top
//! error reasons (stored text, accounts, links, tokens and addresses stripped, cut short) and the
//! teams affected (slug and post numbers only: no post text, no people), and adds posts stuck
//! publishing, accounts needing a reconnect and the age of the newest database backup
//! (`backups/latest.json`, written by the `db-backup` service). Nothing is sent when all of it is
//! zero, except an "all clear" line on Mondays, so silence means broken. One send a day: the
//! day's row in `rate_limit_hits` is the record, so a restart or a deploy does not mail twice.
//! shortcut: a late start (a deploy over 08:00) sends at the next round with a window from the
//! missed 08:00; more than `ROWS` notices a day only count (reasons and teams use the newest).
use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc, Weekday};
use chrono_tz::Asia::Dhaka;
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement, Value};

use crate::{expiry::label, post_mail, rate_limit, state::AppState};

/// The hour (Dhaka) the digest goes out.
const HOUR: u32 = 8;
/// Publishing this long is stuck (a recovery cuts a target off after `publish::STUCK_MINUTES`).
const STUCK_MINUTES: i64 = 15;
/// A backup older than this is flagged: a run is due every 6 hours.
const STALE_HOURS: i64 = 7;
/// Where the backup job leaves its newest dump's time (`ops/backup/backup.sh`).
const BACKUP_KEY: &str = "backups/latest.json";
/// Notices read for reasons and teams.
const ROWS: i64 = 1000;
/// Reasons listed, teams listed and post numbers listed for each team.
const TOP: usize = 5;
const TEAMS: usize = 10;

/// What a round did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Before 08:00 in Dhaka.
    NotDue,
    /// Today's digest was already handled (sent, nothing to say, or skipped).
    Done,
    /// `ADMIN_EMAILS` is empty.
    NoAdmins,
    /// All zero, and not a Monday.
    Quiet,
    /// Mailed to this many addresses.
    Sent(usize),
}

enum Backup {
    /// How old the newest dump is.
    Age(Duration),
    Unreadable,
}

struct Facts {
    /// `(kind, platform)` to posts.
    counts: BTreeMap<(String, String), i64>,
    /// The top reasons and how many failed accounts gave each.
    reasons: Vec<(String, i64)>,
    /// Team slug (or code) to its posts' numbers, newest first.
    teams: BTreeMap<String, Vec<i64>>,
    stuck: i64,
    reconnect: i64,
    x_paused: i64,
    /// Refunds and disputes of credit packs in the window, the credits taken back and the credits
    /// that were already spent (`credit_takebacks`).
    takebacks: (i64, i64, i64),
    backup: Backup,
}

impl Facts {
    fn kind(&self, kind: &str) -> i64 {
        self.counts
            .iter()
            .filter(|((k, _), _)| k == kind)
            .map(|(_, n)| n)
            .sum()
    }

    fn backup_bad(&self) -> bool {
        match self.backup {
            Backup::Age(age) => age > Duration::hours(STALE_HOURS),
            Backup::Unreadable => true,
        }
    }

    fn problems(&self) -> bool {
        !self.counts.is_empty()
            || self.stuck > 0
            || self.reconnect > 0
            || self.x_paused > 0
            || self.takebacks.0 > 0
            || self.backup_bad()
    }
}

fn sql(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// One round of the scheduler loop.
pub async fn tick(app: &AppState) -> Result<Outcome, DbErr> {
    run(app, Utc::now()).await
}

/// The digest due at `now`, if it is due and not sent yet today (Dhaka).
pub async fn run(app: &AppState, now: DateTime<Utc>) -> Result<Outcome, DbErr> {
    let local = now.with_timezone(&Dhaka);
    if local.hour() < HOUR {
        return Ok(Outcome::NotDue);
    }
    let day = local.date_naive();
    let caps = [(format!("operator-digest:{day}"), 1)];
    // The record comes first: a second instance or a restart finds it taken. Given back when
    // nothing could be sent, so the next round tries again.
    let Some(hit) = rate_limit::daily_reserve(&app.db, &caps).await? else {
        return Ok(Outcome::Done);
    };
    if app.admin_emails.is_empty() {
        tracing::warn!("ADMIN_EMAILS is not set: no operator digest");
        return Ok(Outcome::NoAdmins);
    }
    let from = Dhaka
        .with_ymd_and_hms(day.year(), day.month(), day.day(), HOUR, 0, 0)
        .single()
        .map_or(now, |t| t.with_timezone(&Utc))
        - Duration::hours(24);
    let facts = match collect(app, from, now).await {
        Ok(facts) => facts,
        Err(e) => {
            rate_limit::daily_refund(&app.db, &hit).await;
            return Err(e);
        }
    };
    let monday = local.weekday() == Weekday::Mon;
    if !facts.problems() && !monday {
        return Ok(Outcome::Quiet);
    }
    let (subject, text) = compose(&facts, &day.to_string());
    let (mut sent, mut failed, mut down) = (0, 0, true);
    for to in &app.admin_emails {
        match app
            .mailer
            .send(to, &subject, text.clone(), &app.web_url)
            .await
        {
            Ok(()) => sent += 1,
            Err(e) => {
                failed += 1;
                down &= crate::mail::transient(&e);
                tracing::warn!(error = %e, "operator digest not sent");
            }
        }
    }
    // Nobody got it and the provider is down: the next round tries again.
    if sent == 0 && failed > 0 && down {
        rate_limit::daily_refund(&app.db, &hit).await;
    }
    Ok(Outcome::Sent(sent))
}

async fn collect(app: &AppState, from: DateTime<Utc>, now: DateTime<Utc>) -> Result<Facts, DbErr> {
    let db = &app.db;
    let mut counts = BTreeMap::new();
    let rows = db
        .query_all_raw(sql(
            "SELECT n.kind, p.platform, count(*) AS n FROM post_notices n \
             JOIN posts p ON p.id = n.post_id WHERE n.created_at >= $1 AND n.created_at < $2 \
             GROUP BY n.kind, p.platform",
            vec![from.into(), now.into()],
        ))
        .await?;
    for r in rows {
        counts.insert(
            (r.try_get("", "kind")?, r.try_get("", "platform")?),
            r.try_get("", "n")?,
        );
    }
    let rows = db
        .query_all_raw(sql(
            "SELECT n.kind, p.error, p.number, COALESCE(t.slug, t.code) AS team \
             FROM post_notices n JOIN posts p ON p.id = n.post_id JOIN teams t ON t.id = n.team_id \
             WHERE n.created_at >= $1 AND n.created_at < $2 ORDER BY n.created_at DESC LIMIT $3",
            vec![from.into(), now.into(), ROWS.into()],
        ))
        .await?;
    let mut reasons: BTreeMap<String, i64> = BTreeMap::new();
    let mut teams: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for r in rows {
        let kind: String = r.try_get("", "kind")?;
        let error: Option<String> = r.try_get("", "error")?;
        let numbers = teams.entry(r.try_get("", "team")?).or_default();
        let number = r.try_get("", "number")?;
        // A post that failed and went late has two notices.
        if !numbers.contains(&number) {
            numbers.push(number);
        }
        // A late post's text is its due time, a stopped repeat's is not about publishing.
        if matches!(kind.as_str(), "failed" | "partial") {
            for reason in reasons_of(error.as_deref().unwrap_or("No reason was stored.")) {
                *reasons.entry(reason).or_default() += 1;
            }
        }
    }
    let mut reasons: Vec<_> = reasons.into_iter().collect();
    reasons.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    reasons.truncate(TOP);
    let count = |stmt: Statement| async move {
        let row = db.query_one_raw(stmt).await?;
        row.map_or(Ok(0), |r| r.try_get::<i64>("", "n"))
    };
    let stuck = count(sql(
        "SELECT count(DISTINCT post_id) AS n FROM post_targets WHERE status = 'publishing' \
         AND updated_at < $1",
        vec![(now - Duration::minutes(STUCK_MINUTES)).into()],
    ))
    .await?;
    // `expiry::status`: refused, or a token that ends for good and has ended.
    let reconnect = count(sql(
        "SELECT count(*) AS n FROM social_accounts WHERE meta->>'reconnect' = 'true' \
         OR (token_expires_at <= $1 AND provider NOT IN ('x', 'tiktok', 'youtube', 'pinterest'))",
        vec![now.into()],
    ))
    .await?;
    let x_paused = count(sql(
        "SELECT count(*) AS n FROM x_inbox WHERE paused_reason = 'errors'",
        vec![],
    ))
    .await?;
    let row = db
        .query_one_raw(sql(
            "SELECT count(*) AS n, COALESCE(sum(taken), 0)::bigint AS taken, \
             COALESCE(sum(owed - taken), 0)::bigint AS short FROM credit_takebacks \
             WHERE created_at >= $1 AND created_at < $2",
            vec![from.into(), now.into()],
        ))
        .await?;
    let takebacks = match row {
        Some(r) => (
            r.try_get("", "n")?,
            r.try_get("", "taken")?,
            r.try_get("", "short")?,
        ),
        None => (0, 0, 0),
    };
    Ok(Facts {
        counts,
        reasons,
        teams,
        stuck,
        reconnect,
        x_paused,
        takebacks,
        backup: backup(app, now).await,
    })
}

async fn backup(app: &AppState, now: DateTime<Utc>) -> Backup {
    let read = async {
        let bytes = app.media.read(BACKUP_KEY).await.ok()?;
        let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        DateTime::parse_from_rfc3339(json["time"].as_str()?).ok()
    };
    match read.await {
        Some(time) => Backup::Age(now - time.with_timezone(&Utc)),
        None => Backup::Unreadable,
    }
}

/// The reasons in a post's `error` (`publish::settle` joins its failed accounts with "; ", each
/// as "account: reason"), without the account, one cleaned line each.
fn reasons_of(error: &str) -> Vec<String> {
    error
        .split("; ")
        .map(|part| clean(part.split_once(": ").map_or(part, |(_, why)| why)))
        .filter(|r| !r.is_empty())
        .collect()
}

/// `text` as one short line with what could identify a person or open a door removed: a link
/// keeps its scheme and host only (no user info, path, query or fragment, wherever it sits), and
/// a word goes when it holds an `@` (address, handle), looks like a token (16 or more
/// letters, digits and `-_.+/~%|` with a digit in them), is a name with two dots (`a.b.c`), or
/// follows `=`, `Bearer` or `Basic`.
/// shortcut: a provider error that quotes a post or an account's display name is only cut
/// short; move to fixed reason codes if one does.
fn clean(text: &str) -> String {
    let run = |c: char| c.is_alphanumeric() || "_-.+/~%|@".contains(c);
    let chars: Vec<char> = text.chars().collect();
    // `value`: the next word is a secret (after `=`, `Bearer` or `Basic`), and all of it up to
    // the next space; `used` says it was met.
    let (mut out, mut i, mut value, mut used) = (String::new(), 0, false, false);
    while i < chars.len() {
        if !run(chars[i]) {
            let c = chars[i];
            if c == '=' {
                value = true;
            } else if c.is_whitespace() && used {
                (value, used) = (false, false);
            }
            out.push(c);
            i += 1;
            continue;
        }
        let end = (i..chars.len())
            .find(|&j| !run(chars[j]))
            .unwrap_or(chars.len());
        let word: String = chars[i..end].iter().collect();
        let lower = word.to_ascii_lowercase();
        if matches!(lower.as_str(), "http" | "https") && chars[end..].starts_with(&[':', '/', '/'])
        {
            let stop = |c: char| c.is_whitespace() || ")]>\"'<,;".contains(c);
            let k = (end + 3..chars.len())
                .find(|&j| stop(chars[j]))
                .unwrap_or(chars.len());
            let rest: String = chars[end + 3..k].iter().collect();
            let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
            out.push_str(&format!(
                "{word}://{}",
                authority.rsplit('@').next().unwrap_or_default()
            ));
            (i, value, used) = (k, false, false);
            continue;
        }
        let token = word.len() >= 16 && word.chars().any(|c| c.is_ascii_digit());
        let named = word.split('.').filter(|p| !p.is_empty()).count() >= 3;
        out.push_str(if word.contains('@') {
            "<handle>"
        } else if value || token || named {
            "<redacted>"
        } else {
            &word
        });
        if matches!(lower.as_str(), "bearer" | "basic") {
            (value, used) = (true, false);
        } else if value {
            used = true;
        }
        i = end;
    }
    post_mail::one_line(&out, 120)
}

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn by_network(facts: &Facts, kind: &str) -> String {
    let parts: Vec<String> = facts
        .counts
        .iter()
        .filter(|((k, _), _)| k == kind)
        .map(|((_, platform), n)| format!("{} {n}", label(platform)))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

fn age(age: Duration) -> String {
    let minutes = age.num_minutes().max(0);
    format!("{} h {} min", minutes / 60, minutes % 60)
}

/// The subject and text of the digest for the Dhaka date `day`.
fn compose(facts: &Facts, day: &str) -> (String, String) {
    let (failed, partial) = (facts.kind("failed"), facts.kind("partial"));
    let (late, repeat) = (facts.kind("late"), facts.kind("repeat_ended"));
    let backup = match facts.backup {
        Backup::Age(a) if !facts.backup_bad() => format!("Last database backup: {} old.", age(a)),
        Backup::Age(a) => format!(
            "Last database backup: {} old. STALE: a run is due every 6 hours (docs/restore-runbook.md, section 8).",
            age(a)
        ),
        Backup::Unreadable => format!(
            "Last database backup: could not be read ({BACKUP_KEY} is missing or unreadable). \
             Check the db-backup service."
        ),
    };
    if !facts.problems() {
        return (
            format!("SocialReins digest {day}: all clear"),
            format!(
                "All clear for the last 24 hours: no failed, part failed or late posts, none \
                 stuck publishing, no account to reconnect.\n\n{backup}\n\nThis line comes \
                 every Monday, so no mail on other days means nothing to report, and no \
                 mail on a Monday means the digest is broken.\n"
            ),
        );
    }
    let mut lines = vec![
        format!("The last 24 hours, every team (08:00 Asia/Dhaka, {day})."),
        format!("Failed: {failed}{}", by_network(facts, "failed")),
        format!("Part failed: {partial}{}", by_network(facts, "partial")),
        format!("Late, back to draft: {late}{}", by_network(facts, "late")),
    ];
    if repeat > 0 {
        lines.push(format!(
            "Repeat stopped: {repeat}{}",
            by_network(facts, "repeat_ended")
        ));
    }
    if !facts.reasons.is_empty() {
        lines.push("Top reasons:".into());
        lines.extend(facts.reasons.iter().map(|(r, n)| format!("- {n} x {r}")));
    }
    if !facts.teams.is_empty() {
        lines.push("Teams affected (post numbers):".into());
        for (team, numbers) in facts.teams.iter().take(TEAMS) {
            let shown: Vec<String> = numbers.iter().take(TOP).map(|n| format!("#{n}")).collect();
            let more = numbers.len().saturating_sub(TOP);
            let more = if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            };
            lines.push(format!("- {team}: {}{more}", shown.join(", ")));
        }
        let hidden = facts.teams.len().saturating_sub(TEAMS);
        if hidden > 0 {
            lines.push(format!("- and {hidden} more teams"));
        }
    }
    lines.push(format!(
        "Publishing for over {STUCK_MINUTES} minutes: {}",
        facts.stuck
    ));
    lines.push(format!("Accounts to reconnect: {}", facts.reconnect));
    lines.push(format!("X inboxes paused after errors: {}", facts.x_paused));
    let (n, taken, short) = facts.takebacks;
    lines.push(format!(
        "Credit pack refunds or disputes: {n} ({taken} credits taken back, {short} already spent)"
    ));
    lines.push(backup);
    let total = failed + partial + late + repeat;
    let mut flags = Vec::new();
    if total > 0 {
        flags.push(plural(total, "post", "posts") + " not out");
    }
    if facts.takebacks.0 > 0 {
        flags.push(plural(facts.takebacks.0, "take-back", "take-backs"));
    }
    if facts.stuck > 0 {
        flags.push(format!("{} stuck", facts.stuck));
    }
    if facts.reconnect > 0 {
        flags.push(format!("{} to reconnect", facts.reconnect));
    }
    if facts.backup_bad() {
        flags.push("backup".into());
    }
    (
        format!("SocialReins digest {day}: {}", flags.join(", ")),
        lines.join("\n\n") + "\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_lose_accounts_links_tokens_and_addresses() {
        let error = "@ann.bsky.social: Bluesky refused it (InvalidRequest).; \
                     Ann B: GET https://graph.test/v1/me/feed?access_token=EAAB1234567890abcdefghij&x=1 failed";
        let reasons = reasons_of(error);
        assert_eq!(reasons[0], "Bluesky refused it (InvalidRequest).");
        assert_eq!(reasons[1], "GET https://graph.test failed");
        assert_eq!(
            clean(
                "Bearer abcdef token=xyz mail me@a.test @ann AbCdEf1234567890AbCdEf1234567890 ok"
            ),
            "Bearer <redacted> token=<redacted> mail <handle> <handle> <redacted> ok"
        );
        assert_eq!(
            clean(
                "https://u:pw@a.test/p?k=v#f (https://b.test/x?t=1) \
                 {\"access_token\":\"EAAB123456789012345\"} ann.bsky.social ..."
            ),
            "https://a.test (https://b.test) {\"access_token\":\"<redacted>\"} <redacted> ..."
        );
        // A link in brackets, as reqwest writes it, loses its path too.
        assert_eq!(
            clean(
                "error sending request for url (https://graph.test/v19.0/1234/feed?access_token=x)"
            ),
            "error sending request for url (https://graph.test)"
        );
        assert!(clean(&"x ".repeat(200)).ends_with("..."));
    }

    #[test]
    fn the_text_names_networks_reasons_and_teams_but_no_post_text() {
        let mut facts = Facts {
            counts: BTreeMap::new(),
            reasons: vec![("Timed out".into(), 3)],
            teams: BTreeMap::from([("acme".into(), vec![7, 3])]),
            stuck: 1,
            reconnect: 2,
            x_paused: 0,
            takebacks: (0, 0, 0),
            backup: Backup::Age(Duration::minutes(200)),
        };
        facts.counts.insert(("failed".into(), "x".into()), 2);
        facts.counts.insert(("failed".into(), "bluesky".into()), 1);
        facts.counts.insert(("late".into(), "facebook".into()), 1);
        let (subject, text) = compose(&facts, "2026-11-27");
        assert_eq!(
            subject,
            "SocialReins digest 2026-11-27: 4 posts not out, 1 stuck, 2 to reconnect"
        );
        assert!(text.contains("Failed: 3 (Bluesky 1, X 2)"), "{text}");
        assert!(
            text.contains("Late, back to draft: 1 (Facebook 1)"),
            "{text}"
        );
        assert!(text.contains("- 3 x Timed out"), "{text}");
        assert!(text.contains("- acme: #7, #3"), "{text}");
        assert!(text.contains("Publishing for over 15 minutes: 1"), "{text}");
        assert!(
            text.contains("Last database backup: 3 h 20 min old."),
            "{text}"
        );
        assert!(!text.contains("STALE"));
        facts.backup = Backup::Age(Duration::hours(8));
        assert!(compose(&facts, "d").1.contains("STALE"));
        facts.backup = Backup::Unreadable;
        assert!(compose(&facts, "d").1.contains("could not be read"));
        // A take-back alone is worth a mail, and says how much was already spent.
        let quiet = Facts {
            counts: BTreeMap::new(),
            reasons: vec![],
            teams: BTreeMap::new(),
            stuck: 0,
            reconnect: 0,
            x_paused: 0,
            takebacks: (2, 130, 70),
            backup: Backup::Age(Duration::minutes(10)),
        };
        let (subject, text) = compose(&quiet, "2026-11-27");
        assert_eq!(subject, "SocialReins digest 2026-11-27: 2 take-backs");
        assert!(
            text.contains(
                "Credit pack refunds or disputes: 2 (130 credits taken back, 70 already spent)"
            ),
            "{text}"
        );
    }
}
