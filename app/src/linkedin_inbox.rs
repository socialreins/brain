//! LinkedIn comments in the inbox, and answering them, for automations. Everything here is behind
//! a capability that stays off until LinkedIn has approved the Community Management API for the
//! app and an account was connected with its Page scopes (`capability`):
//! - `LINKEDIN_COMMUNITY_API=true` (`LinkedIn::community_api`) says the approval came;
//! - the account is a Page (`meta.organization`, `urn:li:organization:...`): a member's comments
//!   cannot be read by an app at all, only the Page's (`r_member_social_feed` is closed);
//! - the token carries `r_organization_social_feed` and `w_organization_social_feed`
//!   (`meta.scopes`, what LinkedIn said it granted).
//!
//! Without all three the account shows `needs_approval`, `personal_profile` or `needs_scope`, no
//! LinkedIn call is made, and an automation limited to it is refused when saved. Nothing connects a
//! Page yet (that needs the same approval), so no account is ready today.
//!
//! LinkedIn pushes nothing on the Development tier, so a ready Page is polled: the comments of its
//! newest posts (`POSTS`, published through SocialReins in the last `POST_DAYS` days), one call
//! each, every `POLL_MINUTES`. The Development tier allows 100 calls a day for a member and 500
//! for the app, so a Page reads at most `READ_CALLS` a day and writes (replies, likes) up to
//! `DAILY_CALLS`, counted in `meta.linkedin_inbox`. A first read stores what it finds as already
//! automated. A failed round waits for the next interval and is never retried.
use chrono::DateTime;
use sea_orm::{ConnectionTrait, DbBackend, DbErr, EntityTrait, Statement, Value};
use uuid::Uuid;

use crate::{
    automations,
    connectors::{
        bluesky::{Author, InboxError, PostError},
        linkedin::{post_url, Comment},
    },
    inbox::{self, Fresh},
    models::{inbox_items, social_accounts},
    state::AppState,
};

/// Scopes a Page connection must hold, both from the Community Management API.
pub const PAGE_SCOPES: [&str; 2] = ["r_organization_social_feed", "w_organization_social_feed"];
pub const POLL_MINUTES: i64 = 180;
const POSTS: i64 = 5;
const POST_DAYS: i64 = 14;
/// Calls a Page may spend a day reading, and reading plus writing (LinkedIn's per-member limit on
/// the Development tier is 100).
pub const READ_CALLS: i64 = 80;
pub const DAILY_CALLS: i64 = 100;

/// What a LinkedIn account can do with comments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    /// LinkedIn has not approved the Community Management API for this app (or the server does
    /// not say it did).
    NeedsApproval,
    /// A personal profile: LinkedIn lets no app read comments on a member's posts.
    PersonalProfile,
    /// A Page connected without the comment scopes: connect it again.
    NeedsScope,
    Ready,
}

impl Capability {
    pub fn code(self) -> &'static str {
        match self {
            Self::NeedsApproval => "linkedin_needs_approval",
            Self::PersonalProfile => "linkedin_personal_profile",
            Self::NeedsScope => "linkedin_needs_scope",
            Self::Ready => "ready",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::NeedsApproval => {
                "Comments and automations on LinkedIn need LinkedIn's approval of the \
                 Community Management API for this app. Until it is granted, LinkedIn accounts \
                 publish only."
            }
            Self::PersonalProfile => {
                "LinkedIn lets apps read comments on a Page's posts, not on a personal \
                 profile's. Connect the Page to use automations."
            }
            Self::NeedsScope => {
                "This Page was connected without the comment permissions. Connect it again and \
                 accept them."
            }
            Self::Ready => "",
        }
    }
}

/// The Page an account is, when it is one.
pub fn organization(meta: &serde_json::Value) -> Option<&str> {
    meta["organization"]
        .as_str()
        .filter(|o| o.starts_with("urn:li:organization:") && o.len() > 20)
}

pub fn capability(app: &AppState, account: &social_accounts::Model) -> Capability {
    capability_of(app.linkedin.community_api, &account.meta)
}

/// `capability` from the server's approval flag and an account's `meta`.
pub fn capability_of(approved: bool, meta: &serde_json::Value) -> Capability {
    if !approved {
        return Capability::NeedsApproval;
    }
    if organization(meta).is_none() {
        return Capability::PersonalProfile;
    }
    let granted = |s: &str| {
        meta["scopes"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(s)))
    };
    match PAGE_SCOPES.iter().all(|s| granted(s)) {
        true => Capability::Ready,
        false => Capability::NeedsScope,
    }
}

fn sql(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// Counts one call against the Page's day (the database's), unless `cap` calls were spent.
async fn reserve(db: &impl ConnectionTrait, id: Uuid, cap: i64) -> Result<bool, DbErr> {
    let hit = db
        .query_all_raw(sql(
            "UPDATE social_accounts SET meta = meta || jsonb_build_object('linkedin_inbox', \
               COALESCE(meta->'linkedin_inbox', '{}'::jsonb) || jsonb_build_object('day', \
               current_date::text, 'calls', CASE WHEN meta->'linkedin_inbox'->>'day' = \
               current_date::text THEN COALESCE((meta->'linkedin_inbox'->>'calls')::int, 0) + 1 \
               ELSE 1 END)) \
             WHERE id = $1 AND (COALESCE(meta->'linkedin_inbox'->>'day', '') <> current_date::text \
               OR COALESCE((meta->'linkedin_inbox'->>'calls')::int, 0) < $2) RETURNING id",
            vec![id.into(), cap.into()],
        ))
        .await?;
    Ok(!hit.is_empty())
}

fn refused(e: PostError) -> InboxError {
    match e {
        PostError::Reconnect | PostError::Denied => InboxError::LinkedInReconnect,
        PostError::Failed(why) => InboxError::LinkedIn(why),
    }
}

/// What a write needs: a ready Page, its token and a call left today. The Page's urn and token.
async fn writable(
    app: &AppState,
    account: &social_accounts::Model,
) -> Result<(String, String), InboxError> {
    match capability(app, account) {
        Capability::Ready => {}
        c => return Err(InboxError::Invalid(c.code())),
    }
    let (Some(org), Some(token)) = (organization(&account.meta), inbox::secret(app, account))
    else {
        return Err(InboxError::LinkedInReconnect);
    };
    match reserve(&app.db, account.id, DAILY_CALLS).await {
        Ok(true) => Ok((org.to_owned(), token)),
        Ok(false) => Err(InboxError::Invalid("linkedin_call_budget")),
        Err(e) => {
            tracing::error!(error = %e, "linkedin call budget failed");
            Err(InboxError::LinkedIn(
                "This could not be sent. Try again.".into(),
            ))
        }
    }
}

/// Answers a comment as the Page, once (`w_organization_social_feed`). The id of what went out.
pub async fn send(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    text: &str,
) -> Result<String, InboxError> {
    let Some(post) = item
        .root_remote_id
        .as_deref()
        .filter(|_| item.kind == "comment")
    else {
        return Err(InboxError::Invalid("not_repliable"));
    };
    let (org, token) = writable(app, account).await?;
    let parent = (org.as_str(), post, item.remote_id.as_str());
    app.linkedin
        .reply(&token, parent, text)
        .await
        .map_err(refused)
}

/// Likes a comment as the Page, once.
pub async fn like(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
) -> Result<(), InboxError> {
    if item.kind != "comment" {
        return Err(InboxError::Invalid("not_supported"));
    }
    let (org, token) = writable(app, account).await?;
    app.linkedin
        .like_comment(&token, &org, &item.remote_id)
        .await
        .map_err(refused)
}

fn fresh_of(post: &str, c: &Comment) -> Option<Fresh> {
    let who = c.actor.rsplit(':').next().filter(|w| !w.is_empty())?;
    Some(Fresh {
        kind: "comment",
        remote_id: c.urn.clone(),
        remote_cid: None,
        parent: Some(c.parent.clone().unwrap_or_else(|| post.to_owned())),
        root: (Some(post.to_owned()), None),
        convo_id: None,
        author: Author {
            did: who.to_owned(),
            handle: None,
            display_name: None,
            avatar: None,
        },
        text: c.text.clone(),
        at: DateTime::from_timestamp_millis(c.at_ms)?.fixed_offset(),
        permalink: Some(post_url(post)),
        story: None,
        button_payload: None,
        attachments: Vec::new(),
    })
}

/// Reads the comments of one Page's newest posts.
async fn poll(app: &AppState, id: Uuid) -> Result<(), DbErr> {
    let Some(account) = social_accounts::Entity::find_by_id(id).one(&app.db).await? else {
        return Ok(());
    };
    let (Capability::Ready, Some(own), Some(token)) = (
        capability(app, &account),
        organization(&account.meta).map(str::to_owned),
        inbox::secret(app, &account),
    ) else {
        return Ok(());
    };
    // Items found before the first round ended are history: no automation answers them.
    let first = account.meta["linkedin_inbox"]["first_done"].as_bool() != Some(true);
    let posts: Vec<String> = app
        .db
        .query_all_raw(sql(
            "SELECT remote_id FROM post_targets WHERE social_account_id = $1 \
             AND status = 'published' AND remote_id LIKE 'urn:li:%' \
             AND published_at > now() - make_interval(days => $2) \
             ORDER BY published_at DESC LIMIT $3",
            vec![id.into(), (POST_DAYS as i32).into(), POSTS.into()],
        ))
        .await?
        .iter()
        .filter_map(|r| r.try_get("", "remote_id").ok())
        .collect();
    let mut fresh = Vec::new();
    for post in &posts {
        if !reserve(&app.db, id, READ_CALLS).await? {
            return Ok(());
        }
        let comments = match app.linkedin.comments(&token, post).await {
            Ok(c) => c,
            Err(e) => {
                if matches!(e, PostError::Reconnect) {
                    crate::expiry::flag_reconnect(&app.db, id).await;
                }
                tracing::warn!(account = %id, "linkedin comments read failed");
                return Ok(());
            }
        };
        for c in comments.iter().filter(|c| c.actor != own) {
            let Some(f) = fresh_of(post, c) else { continue };
            if let Some(item) = inbox::store_with(&app.db, &account, f, (true, first)).await? {
                fresh.push(item);
            }
        }
    }
    if first {
        app.db
            .execute_raw(sql(
                "UPDATE social_accounts SET meta = meta || jsonb_build_object('linkedin_inbox', \
                 COALESCE(meta->'linkedin_inbox', '{}'::jsonb) || '{\"first_done\": true}'::jsonb) \
                 WHERE id = $1",
                vec![id.into()],
            ))
            .await?;
    }
    for item in fresh {
        if let Err(e) = automations::run(app, &account, &item).await {
            tracing::error!(error = %e, "linkedin automation failed");
        }
    }
    Ok(())
}

/// Reads the Pages that are due. Does nothing until the capability is on.
pub async fn tick(app: &AppState) -> Result<(), DbErr> {
    if !app.linkedin.community_api {
        return Ok(());
    }
    let claimed = app
        .db
        .query_all_raw(sql(
            "UPDATE social_accounts SET meta = meta || jsonb_build_object('linkedin_inbox', \
               COALESCE(meta->'linkedin_inbox', '{}'::jsonb) || jsonb_build_object('next_at', \
               to_char(now() + make_interval(mins => $1), 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'))) \
             WHERE id IN (SELECT id FROM social_accounts WHERE provider = 'linkedin' \
               AND meta->>'organization' LIKE 'urn:li:organization:%' \
               AND COALESCE(meta->>'reconnect', 'false') <> 'true' \
               AND COALESCE((meta->'linkedin_inbox'->>'next_at')::timestamptz, 'epoch') <= now() \
               ORDER BY id LIMIT 20 FOR UPDATE SKIP LOCKED) RETURNING id",
            vec![(POLL_MINUTES as i32).into()],
        ))
        .await?;
    let reads: Vec<_> = claimed
        .iter()
        .filter_map(|r| r.try_get::<Uuid>("", "id").ok())
        .map(|id| {
            let app = app.clone();
            tokio::spawn(async move {
                if let Err(e) = poll(&app, id).await {
                    tracing::error!(error = %e, account = %id, "linkedin inbox poll failed");
                }
            })
        })
        .collect();
    for read in reads {
        let _ = read.await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_page_urn_counts() {
        let page = |o: &str| serde_json::json!({ "organization": o });
        assert!(organization(&page("urn:li:organization:123")).is_some());
        assert!(organization(&page("urn:li:person:123")).is_none());
        assert!(organization(&page("urn:li:organization:")).is_none());
        assert!(organization(&serde_json::json!({})).is_none());
    }
}
