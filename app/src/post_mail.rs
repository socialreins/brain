//! Mail about posts that did not go out (decision 0008): a post that settled as failed or partly
//! failed, or that went back to draft for being over an hour late. Whatever decides that queues a
//! notice (`queue`) in its own transaction; the publish round sends what is queued (`send_due`)
//! in the background, never inside a transaction. Each notice mails the post's author and the
//! team's owners once, at most `PER_DAY` notices a team a day; the rest are only logged.
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement, Value};
use uuid::Uuid;

use crate::{rate_limit, state::AppState};

/// Notices a team is mailed a day.
pub const PER_DAY: i64 = 10;

/// Why a post is mailed about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Every account failed.
    Failed,
    /// Out on some accounts, failed on others.
    Partial,
    /// Over an hour late, back to draft.
    Late,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Failed => "failed",
            Kind::Partial => "partial",
            Kind::Late => "late",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        [Kind::Failed, Kind::Partial, Kind::Late]
            .into_iter()
            .find(|k| k.as_str() == s)
    }
}

fn sql(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// Queues a notice about `post`, in the caller's transaction. One per post and kind: a post that
/// fails again after a retry is not mailed twice.
pub async fn queue(db: &impl ConnectionTrait, post: Uuid, kind: Kind) -> Result<(), DbErr> {
    let stmt = sql(
        "INSERT INTO post_notices (post_id, team_id, kind) \
         SELECT id, team_id, $2 FROM posts WHERE id = $1 ON CONFLICT DO NOTHING",
        vec![post.into(), kind.as_str().into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// Claims the notices not sent yet (each by one instance, `sent_at` first) and mails them, one
/// at a time. Errors are logged: a notice is never sent twice, so a lost one stays lost.
pub async fn send_due(app: &AppState) {
    let claim = sql(
        "UPDATE post_notices SET sent_at = now() WHERE (post_id, kind) IN ( \
           SELECT post_id, kind FROM post_notices WHERE sent_at IS NULL \
           ORDER BY created_at LIMIT 50 FOR UPDATE SKIP LOCKED) \
         RETURNING post_id, team_id, kind",
        vec![],
    );
    let rows = match app.db.query_all_raw(claim).await {
        Ok(rows) => rows,
        Err(e) => return tracing::warn!(error = %e, "post notices not claimed"),
    };
    for row in rows {
        let got = (|| {
            let post: Uuid = row.try_get("", "post_id")?;
            let team: Uuid = row.try_get("", "team_id")?;
            let kind: String = row.try_get("", "kind")?;
            Ok::<_, DbErr>((post, team, kind))
        })();
        let Ok((post, team, kind)) = got else {
            continue;
        };
        let Some(kind) = Kind::parse(&kind) else {
            continue;
        };
        if let Err(e) = send_one(app, post, team, kind).await {
            tracing::warn!(error = %e, %post, "post notice not sent");
        }
    }
}

async fn send_one(app: &AppState, post: Uuid, team: Uuid, kind: Kind) -> Result<(), DbErr> {
    let cap = [(format!("post-mail:team:{team}"), PER_DAY)];
    if rate_limit::daily_reserve(&app.db, &cap).await?.is_none() {
        tracing::info!(%post, %team, "post notice over the team's daily cap: not mailed");
        return Ok(());
    }
    let found = app
        .db
        .query_one_raw(sql(
            "SELECT p.platform, p.body, p.error, p.scheduled_at, t.code FROM posts p \
             JOIN teams t ON t.id = p.team_id WHERE p.id = $1",
            vec![post.into()],
        ))
        .await?;
    let Some(p) = found else {
        return Ok(());
    };
    let platform: String = p.try_get("", "platform")?;
    let body: String = p.try_get("", "body")?;
    let error: Option<String> = p.try_get("", "error")?;
    let code: String = p.try_get("", "code")?;
    // The author and the owners, each once; a member who left the team is not mailed.
    let to = app
        .db
        .query_all_raw(sql(
            "SELECT DISTINCT u.email FROM team_user m JOIN users u ON u.id = m.user_id \
             WHERE m.team_id = $1 AND (m.role = 'owner' OR m.user_id = \
               (SELECT created_by FROM posts WHERE id = $2)) ORDER BY u.email",
            vec![team.into(), post.into()],
        ))
        .await?;
    let link = format!("{}/t/{code}/posts/{post}", app.web_url);
    let (subject, text) = compose(kind, &platform, &body, error.as_deref(), &link);
    for r in to {
        let email: String = r.try_get("", "email")?;
        if let Err(e) = app
            .mailer
            .send(&email, &subject, text.clone(), &app.web_url)
            .await
        {
            tracing::warn!(error = %e, %post, "post notice mail not sent");
        }
    }
    Ok(())
}

/// The subject and text of a notice. `error` names each failed account and why (`publish::settle`),
/// or the late reason.
fn compose(
    kind: Kind,
    platform: &str,
    body: &str,
    error: Option<&str>,
    link: &str,
) -> (String, String) {
    let network = crate::expiry::label(platform);
    let snippet: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let snippet = match snippet.char_indices().nth(120) {
        Some((i, _)) => format!("{}...", &snippet[..i]),
        None => snippet,
    };
    let quoted = if snippet.is_empty() {
        String::new()
    } else {
        format!("\"{snippet}\"\n\n")
    };
    let why = error.unwrap_or("No reason was given.");
    // `settle` joins the accounts with "; ": one line each.
    let lines = why.split("; ").collect::<Vec<_>>().join("\n");
    let (subject, lead) = match kind {
        Kind::Failed => (
            format!("Your {network} post failed"),
            format!("Your {network} post did not go out on any account."),
        ),
        Kind::Partial => (
            format!("Your {network} post partly failed"),
            format!("Your {network} post went out, but not on every account."),
        ),
        Kind::Late => (
            format!("Your {network} post was not posted"),
            format!(
                "Your {network} post was not posted: it was more than an hour late, so it went \
                 back to your drafts."
            ),
        ),
    };
    let text = format!("{lead}\n\n{quoted}{lines}\n\nOpen the post:\n\n{link}\n");
    (subject, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notice_names_the_network_each_account_and_the_link() {
        let (subject, text) = compose(
            Kind::Partial,
            "bluesky",
            "Hello\n  world",
            Some("@a.test: Bluesky refused it (InvalidRequest).; @b.test: Timed out"),
            "https://app.test/t/abc234/posts/1",
        );
        assert_eq!(subject, "Your Bluesky post partly failed");
        assert!(text.contains("\"Hello world\""), "{text}");
        assert!(text.contains("@a.test: Bluesky refused it (InvalidRequest).\n@b.test: Timed out"));
        assert!(text.ends_with("https://app.test/t/abc234/posts/1\n"));
        let (subject, text) = compose(Kind::Late, "x", &"a".repeat(300), None, "l");
        assert_eq!(subject, "Your X post was not posted");
        assert!(text.contains(&format!("\"{}...\"", "a".repeat(120))));
        assert!(!text.contains("failed"));
        assert_eq!(Kind::parse("late"), Some(Kind::Late));
    }
}
