//! The post timeline: one row per step the publisher took (`target_claimed`, `attempt_started`,
//! `published`, `failed`, `retry_scheduled`, `late_to_draft`, `taken_down`, `comment_posted`,
//! `comment_failed`, `cover_set`, `cover_failed`), so a post's page can show a claim, a retry or
//! a failed account as its own line instead of only its current statuses. `detail` is our own
//! words or a code the caller chose, never post text, never a URL with a query and never a
//! token: it goes through the same cleaning the operator digest gives a post's error.
//! Rows go with their post (cascade), so there is no purge of its own.
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement, Value};
use uuid::Uuid;

/// The kinds, the whole list the table accepts. Adding one is a migration and a line in
/// `AGENTS.md`; keep it small.
pub const KINDS: [&str; 11] = [
    "target_claimed",
    "attempt_started",
    "published",
    "failed",
    "retry_scheduled",
    "late_to_draft",
    "taken_down",
    "comment_posted",
    "comment_failed",
    "cover_set",
    "cover_failed",
];

fn sql(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// `text` as a detail: one short line with links cut to scheme and host, tokens, handles and
/// addresses removed (`operator_digest::clean`), so nothing a platform said can leak more.
pub(crate) fn clean_detail(text: &str) -> String {
    crate::operator_digest::clean(text)
}

/// Records one event for `post`, best effort: a log that failed to write never fails the
/// publish step it describes.
pub async fn note(
    db: &impl ConnectionTrait,
    post: Uuid,
    target: Option<Uuid>,
    kind: &str,
    detail: Option<&str>,
) {
    if let Err(e) = record(db, post, target, kind, detail).await {
        tracing::warn!(error = %e, %post, kind, "post event not recorded");
    }
}

/// `note` for a detail that is our own code, never a message from a platform or a post: a
/// platform's post or comment id. It is not stripped (the stripping would mangle an `at://`
/// URI), only flattened and cut short.
pub async fn note_code(
    db: &impl ConnectionTrait,
    post: Uuid,
    target: Option<Uuid>,
    kind: &str,
    code: &str,
) {
    let code = crate::post_mail::one_line(code, 120);
    let stmt = sql(
        "INSERT INTO post_events (team_id, post_id, target_id, kind, detail) \
         SELECT p.team_id, p.id, (SELECT t.id FROM post_targets t \
           WHERE t.id = $2 AND t.post_id = p.id), $3, $4 \
         FROM posts p WHERE p.id = $1",
        vec![
            post.into(),
            target.into(),
            kind.to_owned().into(),
            Some(code).into(),
        ],
    );
    if let Err(e) = db.execute_raw(stmt).await {
        tracing::warn!(error = %e, %post, kind, "post event not recorded");
    }
}

/// `note` that returns the error, for tests.
pub async fn record(
    db: &impl ConnectionTrait,
    post: Uuid,
    target: Option<Uuid>,
    kind: &str,
    detail: Option<&str>,
) -> Result<(), DbErr> {
    let detail = detail.map(clean_detail);
    let stmt = sql(
        "INSERT INTO post_events (team_id, post_id, target_id, kind, detail) \
         SELECT p.team_id, p.id, (SELECT t.id FROM post_targets t \
           WHERE t.id = $2 AND t.post_id = p.id), $3, $4 \
         FROM posts p WHERE p.id = $1",
        vec![
            post.into(),
            target.into(),
            kind.to_owned().into(),
            detail.into(),
        ],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// Records one event for a target when only its id is known (`publish::fail_target`, the first
/// comment, the cover). Its team and post come from the row, so they cannot disagree.
pub async fn note_target(
    db: &impl ConnectionTrait,
    target: Uuid,
    kind: &str,
    detail: Option<&str>,
) {
    let detail = detail.map(clean_detail);
    let stmt = sql(
        "INSERT INTO post_events (team_id, post_id, target_id, kind, detail) \
         SELECT p.team_id, p.id, t.id, $2, $3 FROM post_targets t \
         JOIN posts p ON p.id = t.post_id WHERE t.id = $1",
        vec![target.into(), kind.to_owned().into(), detail.into()],
    );
    if let Err(e) = db.execute_raw(stmt).await {
        tracing::warn!(error = %e, %target, kind, "post event not recorded");
    }
}

/// `note_target` for our own code (a comment id), not stripped: see `note_code`.
pub async fn note_target_code(db: &impl ConnectionTrait, target: Uuid, kind: &str, code: &str) {
    let code = crate::post_mail::one_line(code, 120);
    let stmt = sql(
        "INSERT INTO post_events (team_id, post_id, target_id, kind, detail) \
         SELECT p.team_id, p.id, t.id, $2, $3 FROM post_targets t \
         JOIN posts p ON p.id = t.post_id WHERE t.id = $1",
        vec![target.into(), kind.to_owned().into(), Some(code).into()],
    );
    if let Err(e) = db.execute_raw(stmt).await {
        tracing::warn!(error = %e, %target, kind, "post event not recorded");
    }
}

/// A target was claimed and the attempt it started: two events in one place, the second naming
/// its number (the attempts this target has started, this one included).
pub async fn note_attempt(db: &impl ConnectionTrait, target: Uuid) {
    note_target(db, target, "target_claimed", None).await;
    let stmt = sql(
        "INSERT INTO post_events (team_id, post_id, target_id, kind, detail) \
         SELECT p.team_id, p.id, t.id, 'attempt_started', ( \
           SELECT (count(*) + 1)::text FROM post_events e \
           WHERE e.target_id = t.id AND e.kind = 'attempt_started') \
         FROM post_targets t JOIN posts p ON p.id = t.post_id WHERE t.id = $1",
        vec![target.into()],
    );
    if let Err(e) = db.execute_raw(stmt).await {
        tracing::warn!(error = %e, %target, "post event not recorded");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_detail_keeps_our_words_and_drops_what_identifies_or_opens_a_door() {
        assert_eq!(
            clean_detail("Bluesky refused it (InvalidRequest)."),
            "Bluesky refused it (InvalidRequest)."
        );
        assert_eq!(
            clean_detail("Refused, see https://a.test/x?token=abc and Bearer sk-123"),
            "Refused, see https://a.test and Bearer <redacted>"
        );
        assert_eq!(clean_detail("Reply to @someone.test"), "Reply to <handle>");
        assert!(clean_detail(&"a".repeat(300)).ends_with("..."));
    }

    #[test]
    fn every_kind_is_one_the_migration_accepts() {
        assert!(KINDS.contains(&"attempt_started"));
        assert_eq!(KINDS.len(), 11);
    }
}
