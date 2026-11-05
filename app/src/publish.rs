//! Publishing: claim due posts, fan each out to its set's accounts, publish every target once,
//! settle the post; and take published posts down. A thread goes out root first, then each
//! part as a reply to the one before, each saved on its target the moment the platform takes
//! it. Every step is a conditional update, so two instances (or a tick and "publish now") never
//! send a post twice. A publish is never retried: a retry could post twice. Each target status change queues its webhook event in the same
//! statement (`webhooks::with_event`). Media is downloaded into brain's storage before any
//! target is claimed (`media.rs`) and deleted once the post settles.
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
    time::Duration,
};

use tokio::sync::Semaphore;

use async_trait::async_trait;
use sea_orm::{
    sea_query::Query, ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, DbErr,
    EntityTrait, QueryFilter, QuerySelect, Statement, TransactionTrait, Value,
};
use uuid::Uuid;

pub use crate::connectors::bluesky::PostError as PublishError;
use crate::{
    connectors::{
        bluesky::{self, Bluesky},
        linkedin::{self, LinkedIn},
        meta::{self, Facebook, Instagram},
        threads::{self, Threads},
        tiktok::{self, TikTok},
        x::{self, X},
        youtube::{self, YouTube},
    },
    controllers::posts::ThreadPart,
    media::{Fail, Stored},
    models::{
        post_targets, posts, social_account_social_set as membership, social_accounts, social_sets,
    },
    rate_limit::reserve_in,
    state::AppState,
    webhooks::with_event,
};

/// A published post: the platform's id, where a reply must name it (Bluesky) its cid, and its
/// link when the platform gave one (Threads); else `Publisher::url` makes it.
#[derive(Clone)]
pub struct Remote {
    pub id: String,
    pub cid: String,
    pub url: Option<String>,
}

/// One stored image or video: its bytes (empty for a publisher that fetches by URL), type, and
/// the unguessable brain URL that serves it while the post publishes (`media::link`).
pub struct Attachment {
    pub bytes: bytes::Bytes,
    pub content_type: String,
    pub url: String,
}

/// One publisher per platform.
#[async_trait]
pub trait Publisher: Send + Sync {
    /// Publishes, as a reply when `reply` holds `(parent, root)`, and returns the post.
    async fn publish(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        reply: Option<(&Remote, &Remote)>,
    ) -> Result<Remote, PublishError>;

    /// The platform's name in messages, e.g. "Delete the post in the Bluesky app".
    fn name(&self) -> &'static str;

    /// What a target says when the platform refused the stored credentials.
    fn reconnect(&self) -> &'static str;

    /// The platform downloads media from `Attachment::url` itself, so no bytes are read.
    fn by_url(&self) -> bool {
        false
    }

    /// Longest one post (or thread part) may take to publish.
    fn timeout(&self) -> Duration {
        TARGET_TIMEOUT
    }

    async fn delete(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        remote_id: &str,
    ) -> Result<(), PublishError>;

    /// The public link to a published post.
    fn url(&self, account: &social_accounts::Model, remote_id: &str) -> Option<String>;

    /// Comments `text` on the published post as the account and returns the comment's id. Only
    /// Facebook, Instagram, LinkedIn and YouTube do (`post::validate_first_comment`). Taking the
    /// post down removes its comments, so there is no delete.
    async fn comment(
        &self,
        _: &social_accounts::Model,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<String, PublishError> {
        Err(PublishError::Failed(
            "This platform cannot post a first comment.".into(),
        ))
    }

    /// How long a first comment or a cover waits after the post is saved as published.
    fn comment_wait(&self) -> Duration {
        Duration::ZERO
    }

    /// The cover image types the platform takes (Instagram, Facebook, YouTube); none for the
    /// rest, which `post::validate_cover` already refuses.
    fn cover_types(&self) -> &'static [&'static str] {
        &[]
    }

    /// The cover goes in the publish call (`publish_with_cover`, by its brain URL, Instagram)
    /// rather than onto the live video afterwards (`set_cover`, from its bytes).
    fn cover_in_publish(&self) -> bool {
        false
    }

    /// `publish` of one video with its cover. Only where `cover_in_publish`.
    async fn publish_with_cover(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        _: &Attachment,
    ) -> Result<Remote, PublishError> {
        self.publish(account, secret, body, media, None).await
    }

    /// `publish` with the settings the user chose for the post. Only TikTok has any.
    async fn publish_with_tiktok(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        _: &tiktok::Options,
    ) -> Result<Remote, PublishError> {
        self.publish(account, secret, body, media, None).await
    }

    /// Sets the cover of the live video `remote_id`. Only where a cover is not
    /// `cover_in_publish`: Facebook and YouTube. A failure is a warning, the post stays up.
    async fn set_cover(
        &self,
        _: &social_accounts::Model,
        _: &str,
        _: &str,
        _: &Attachment,
    ) -> Result<(), PublishError> {
        Err(PublishError::Failed(
            "This platform cannot set a cover image.".into(),
        ))
    }
}

fn creds<'a>(account: &'a social_accounts::Model, secret: &'a str) -> bluesky::Creds<'a> {
    bluesky::Creds {
        pds: account.meta["pds"].as_str().unwrap_or_default(),
        did: &account.external_id,
        password: secret,
    }
}

#[async_trait]
impl Publisher for Bluesky {
    async fn publish(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        reply: Option<(&Remote, &Remote)>,
    ) -> Result<Remote, PublishError> {
        let media: Vec<(&[u8], &str)> = media
            .iter()
            .map(|a| (&a.bytes[..], a.content_type.as_str()))
            .collect();
        let reply = reply.map(|(p, r)| {
            (
                (p.id.as_str(), p.cid.as_str()),
                (r.id.as_str(), r.cid.as_str()),
            )
        });
        let (id, cid) = self
            .publish_post(&creds(account, secret), body, &media, reply)
            .await?;
        Ok(Remote { id, cid, url: None })
    }

    fn name(&self) -> &'static str {
        "Bluesky"
    }

    fn reconnect(&self) -> &'static str {
        RECONNECT
    }

    async fn delete(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        remote_id: &str,
    ) -> Result<(), PublishError> {
        self.delete_post(&creds(account, secret), remote_id).await
    }

    fn url(&self, _: &social_accounts::Model, remote_id: &str) -> Option<String> {
        bluesky::post_url(remote_id)
    }
}

#[async_trait]
impl Publisher for Threads {
    async fn publish(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        reply: Option<(&Remote, &Remote)>,
    ) -> Result<Remote, PublishError> {
        let what = "a JPEG or PNG image or an MP4 or MOV video";
        let items = by_type(media, &threads::MEDIA_TYPES, what)?;
        let parent = reply.map(|(p, _)| p.id.as_str());
        let (id, url) = self
            .publish_post(&account.external_id, secret, body, &items, parent)
            .await?;
        Ok(Remote {
            id,
            cid: String::new(),
            url,
        })
    }

    async fn delete(
        &self,
        _: &social_accounts::Model,
        secret: &str,
        remote_id: &str,
    ) -> Result<(), PublishError> {
        self.delete_post(secret, remote_id).await
    }

    /// A Threads permalink needs a lookup, done at publish (`Remote::url`).
    fn url(&self, _: &social_accounts::Model, _: &str) -> Option<String> {
        None
    }

    fn name(&self) -> &'static str {
        "Threads"
    }

    fn reconnect(&self) -> &'static str {
        THREADS_RECONNECT
    }

    fn by_url(&self) -> bool {
        true
    }

    fn timeout(&self) -> Duration {
        threads::PUBLISH_TIMEOUT
    }
}

/// Cover types Facebook and YouTube take (Instagram's is JPEG only).
const COVER_TYPES: [&str; 2] = ["image/jpeg", "image/png"];

/// Each file as an item by URL, when its type is one of `types`; else which one is `what` not.
fn by_type<'a>(
    media: &'a [Attachment],
    types: &[&str],
    what: &str,
) -> Result<Vec<threads::Item<'a>>, PublishError> {
    media
        .iter()
        .enumerate()
        .map(|(i, a)| {
            if !types.contains(&a.content_type.as_str()) {
                let n = i + 1;
                return Err(PublishError::Failed(format!("Image {n} is not {what}.")));
            }
            let video = a.content_type.starts_with("video/");
            Ok(threads::Item { url: &a.url, video })
        })
        .collect()
}

#[async_trait]
impl Publisher for LinkedIn {
    async fn publish(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        _: Option<(&Remote, &Remote)>,
    ) -> Result<Remote, PublishError> {
        // A token past its 60 days is refused anyway: say so without calling.
        if account
            .token_expires_at
            .is_some_and(|t| t <= chrono::Utc::now())
        {
            return Err(PublishError::Reconnect);
        }
        let mut files = Vec::new();
        for (i, a) in media.iter().enumerate() {
            let t = a.content_type.as_str();
            let known = linkedin::IMAGE_TYPES.contains(&t)
                || t == linkedin::VIDEO_TYPE
                || t == linkedin::PDF_TYPE;
            if !known {
                let n = i + 1;
                return Err(PublishError::Failed(format!(
                    "Image {n} is not a JPEG, PNG or GIF image, an MP4 video or a PDF."
                )));
            }
            files.push(linkedin::File {
                bytes: &a.bytes,
                content_type: t,
            });
        }
        let id = self
            .publish_post(&account.external_id, secret, body, &files)
            .await?;
        Ok(Remote {
            id,
            cid: String::new(),
            url: None,
        })
    }

    async fn delete(
        &self,
        _: &social_accounts::Model,
        secret: &str,
        remote_id: &str,
    ) -> Result<(), PublishError> {
        self.delete_post(secret, remote_id).await
    }

    fn url(&self, _: &social_accounts::Model, remote_id: &str) -> Option<String> {
        Some(linkedin::post_url(remote_id))
    }

    async fn comment(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        remote_id: &str,
        text: &str,
    ) -> Result<String, PublishError> {
        self.comment_post(&account.external_id, secret, remote_id, text)
            .await
    }

    fn name(&self) -> &'static str {
        "LinkedIn"
    }

    fn reconnect(&self) -> &'static str {
        LINKEDIN_RECONNECT
    }

    fn timeout(&self) -> Duration {
        linkedin::PUBLISH_TIMEOUT
    }
}

#[async_trait]
impl Publisher for X {
    /// Text with up to four images, or one GIF, or one video; a thread part is a reply to the
    /// part before it (`reply`'s parent). `secret` is already renewed (`credential`). The
    /// tweet id is returned the moment X has it, and the caller saves it first.
    async fn publish(
        &self,
        _: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        reply: Option<(&Remote, &Remote)>,
    ) -> Result<Remote, PublishError> {
        let mut files = Vec::new();
        for (i, a) in media.iter().enumerate() {
            let t = a.content_type.as_str();
            let known =
                x::IMAGE_TYPES.contains(&t) || t == x::GIF_TYPE || x::VIDEO_TYPES.contains(&t);
            if !known {
                let n = i + 1;
                return Err(PublishError::Failed(format!(
                    "Image {n} is not a JPEG, PNG, WebP or GIF image or an MP4 or MOV video."
                )));
            }
            files.push(x::File {
                bytes: &a.bytes,
                content_type: t,
            });
        }
        let parent = reply.map(|(p, _)| p.id.as_str());
        let id = self.publish_post(secret, body, &files, parent).await?;
        Ok(Remote {
            id,
            cid: String::new(),
            url: None,
        })
    }

    async fn delete(
        &self,
        _: &social_accounts::Model,
        secret: &str,
        remote_id: &str,
    ) -> Result<(), PublishError> {
        self.delete_post(secret, remote_id).await
    }

    fn url(&self, account: &social_accounts::Model, remote_id: &str) -> Option<String> {
        Some(x::status_url(account.handle.as_deref(), remote_id))
    }

    fn name(&self) -> &'static str {
        "X"
    }

    fn reconnect(&self) -> &'static str {
        X_RECONNECT
    }

    fn timeout(&self) -> Duration {
        x::PUBLISH_TIMEOUT
    }
}

#[async_trait]
impl Publisher for YouTube {
    /// One MP4 or MOV video, titled by the first line of the body. `secret` is already renewed
    /// (`credential`). The video id is returned the moment YouTube has the bytes, and the caller
    /// saves it first.
    async fn publish(
        &self,
        _: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        _: Option<(&Remote, &Remote)>,
    ) -> Result<Remote, PublishError> {
        let [video] = media else {
            return Err(PublishError::Failed(
                "A YouTube post needs exactly one MP4 or MOV video.".into(),
            ));
        };
        if !youtube::VIDEO_TYPES.contains(&video.content_type.as_str()) {
            return Err(PublishError::Failed(
                "The file is not an MP4 or MOV video.".into(),
            ));
        }
        let file = youtube::File {
            bytes: video.bytes.clone(),
            content_type: &video.content_type,
        };
        let id = self
            .upload(secret, socialreins_core::post::youtube_text(body), &file)
            .await?;
        Ok(Remote {
            id,
            cid: String::new(),
            url: None,
        })
    }

    async fn delete(
        &self,
        _: &social_accounts::Model,
        secret: &str,
        remote_id: &str,
    ) -> Result<(), PublishError> {
        self.delete_video(secret, remote_id).await
    }

    fn url(&self, _: &social_accounts::Model, remote_id: &str) -> Option<String> {
        Some(youtube::watch_url(remote_id))
    }

    async fn comment(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        remote_id: &str,
        text: &str,
    ) -> Result<String, PublishError> {
        YouTube::comment(self, secret, &account.external_id, remote_id, text).await
    }

    fn comment_wait(&self) -> Duration {
        self.comment_wait
    }

    fn cover_types(&self) -> &'static [&'static str] {
        &COVER_TYPES
    }

    async fn set_cover(
        &self,
        _: &social_accounts::Model,
        secret: &str,
        remote_id: &str,
        cover: &Attachment,
    ) -> Result<(), PublishError> {
        let file = youtube::File {
            bytes: cover.bytes.clone(),
            content_type: &cover.content_type,
        };
        self.set_thumbnail(secret, remote_id, &file).await
    }

    fn name(&self) -> &'static str {
        "YouTube"
    }

    fn reconnect(&self) -> &'static str {
        YOUTUBE_RECONNECT
    }

    fn timeout(&self) -> Duration {
        youtube::PUBLISH_TIMEOUT
    }
}

impl TikTok {
    /// One MP4 or MOV video, or JPEG or WebP photos, that TikTok downloads from brain's links.
    /// `secret` is already renewed (`credential`). The caption is the body.
    async fn publish_media(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        options: Option<&tiktok::Options>,
    ) -> Result<Remote, PublishError> {
        let videos = media
            .iter()
            .filter(|a| a.content_type.starts_with("video/"));
        let items = match (videos.count(), media) {
            (1, [video]) if tiktok::VIDEO_TYPES.contains(&video.content_type.as_str()) => {
                tiktok::Media::Video(&video.url)
            }
            (0, [_, ..])
                if media
                    .iter()
                    .all(|a| tiktok::PHOTO_TYPES.contains(&a.content_type.as_str())) =>
            {
                tiktok::Media::Photos(media.iter().map(|a| a.url.as_str()).collect())
            }
            _ => {
                return Err(PublishError::Failed(
                    "A TikTok post needs one MP4 or MOV video, or JPEG or WebP photos.".into(),
                ))
            }
        };
        let posted = self.post(secret, body, &items, options).await?;
        let url = posted
            .video_id
            .as_deref()
            .zip(account.handle.as_deref())
            .and_then(|(id, user)| tiktok::video_url(user, id));
        Ok(Remote {
            id: posted.publish_id,
            cid: String::new(),
            url,
        })
    }
}

#[async_trait]
impl Publisher for TikTok {
    async fn publish(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        _: Option<(&Remote, &Remote)>,
    ) -> Result<Remote, PublishError> {
        self.publish_media(account, secret, body, media, None).await
    }

    async fn publish_with_tiktok(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        options: &tiktok::Options,
    ) -> Result<Remote, PublishError> {
        self.publish_media(account, secret, body, media, Some(options))
            .await
    }

    /// TikTok gives apps no way to delete a post.
    async fn delete(
        &self,
        _: &social_accounts::Model,
        _: &str,
        _: &str,
    ) -> Result<(), PublishError> {
        Err(PublishError::Failed(
            "TikTok does not let other apps delete posts.".into(),
        ))
    }

    fn url(&self, _: &social_accounts::Model, _: &str) -> Option<String> {
        None
    }

    fn name(&self) -> &'static str {
        "TikTok"
    }

    fn reconnect(&self) -> &'static str {
        TIKTOK_RECONNECT
    }

    fn by_url(&self) -> bool {
        true
    }

    fn timeout(&self) -> Duration {
        tiktok::PUBLISH_TIMEOUT
    }
}

#[async_trait]
impl Publisher for Facebook {
    async fn publish(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        _: Option<(&Remote, &Remote)>,
    ) -> Result<Remote, PublishError> {
        let what = "a JPEG, PNG or GIF image or an MP4 or MOV video";
        let items = by_type(media, &meta::FACEBOOK_TYPES, what)?;
        let id = self
            .0
            .publish_facebook(&account.external_id, secret, body, &items)
            .await?;
        Ok(Remote {
            id,
            cid: String::new(),
            url: None,
        })
    }

    async fn delete(
        &self,
        _: &social_accounts::Model,
        secret: &str,
        remote_id: &str,
    ) -> Result<(), PublishError> {
        self.0.delete(secret, remote_id).await
    }

    fn url(&self, _: &social_accounts::Model, remote_id: &str) -> Option<String> {
        Some(format!("https://www.facebook.com/{remote_id}"))
    }

    async fn comment(
        &self,
        _: &social_accounts::Model,
        secret: &str,
        remote_id: &str,
        text: &str,
    ) -> Result<String, PublishError> {
        self.0.comment(secret, remote_id, text).await
    }

    fn cover_types(&self) -> &'static [&'static str] {
        &COVER_TYPES
    }

    async fn set_cover(
        &self,
        _: &social_accounts::Model,
        secret: &str,
        remote_id: &str,
        cover: &Attachment,
    ) -> Result<(), PublishError> {
        let file = (&cover.bytes[..], cover.content_type.as_str());
        self.0.set_thumbnail(secret, remote_id, file).await
    }

    fn name(&self) -> &'static str {
        "Facebook"
    }

    fn reconnect(&self) -> &'static str {
        META_RECONNECT
    }

    fn by_url(&self) -> bool {
        true
    }

    fn timeout(&self) -> Duration {
        meta::PUBLISH_TIMEOUT
    }
}

#[async_trait]
impl Publisher for Instagram {
    async fn publish(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        _: Option<(&Remote, &Remote)>,
    ) -> Result<Remote, PublishError> {
        let what = "a JPEG image or an MP4 or MOV video";
        let items = by_type(media, &meta::INSTAGRAM_TYPES, what)?;
        let (id, url) = self
            .0
            .publish_instagram(&account.external_id, secret, body, &items, None)
            .await?;
        Ok(Remote {
            id,
            cid: String::new(),
            url,
        })
    }

    async fn publish_with_cover(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        cover: &Attachment,
    ) -> Result<Remote, PublishError> {
        let what = "a JPEG image or an MP4 or MOV video";
        let items = by_type(media, &meta::INSTAGRAM_TYPES, what)?;
        let cover = Some(cover.url.as_str());
        let (id, url) = self
            .0
            .publish_instagram(&account.external_id, secret, body, &items, cover)
            .await?;
        Ok(Remote {
            id,
            cid: String::new(),
            url,
        })
    }

    async fn delete(
        &self,
        _: &social_accounts::Model,
        secret: &str,
        remote_id: &str,
    ) -> Result<(), PublishError> {
        self.0.delete(secret, remote_id).await
    }

    /// An Instagram permalink needs a lookup, done at publish (`Remote::url`).
    fn url(&self, _: &social_accounts::Model, _: &str) -> Option<String> {
        None
    }

    async fn comment(
        &self,
        _: &social_accounts::Model,
        secret: &str,
        remote_id: &str,
        text: &str,
    ) -> Result<String, PublishError> {
        self.0.comment(secret, remote_id, text).await
    }

    /// Instagram's `cover_url` takes a JPEG only.
    fn cover_types(&self) -> &'static [&'static str] {
        &["image/jpeg"]
    }

    fn cover_in_publish(&self) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "Instagram"
    }

    fn reconnect(&self) -> &'static str {
        META_RECONNECT
    }

    fn by_url(&self) -> bool {
        true
    }

    fn timeout(&self) -> Duration {
        meta::PUBLISH_TIMEOUT
    }
}

/// The publisher for `platform`; `None` until its connector exists.
pub fn publisher<'a>(app: &'a AppState, platform: &str) -> Option<&'a dyn Publisher> {
    match platform {
        "bluesky" => Some(&app.bluesky),
        "threads" => Some(&app.threads),
        "linkedin" => Some(&app.linkedin),
        "youtube" => Some(&app.youtube),
        "x" => Some(&app.x),
        "tiktok" => Some(&app.tiktok),
        "facebook" => Some(&app.facebook),
        "instagram" => Some(&app.instagram),
        _ => None,
    }
}

/// Due posts claimed per tick; the rest wait for the next one.
const CLAIM_BATCH: i64 = 50;
/// Posts publishing at once, and targets publishing at once, across the process.
static POST_SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(8));
static TARGET_SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(8));
/// Posts a team publishes a day, scheduled or published now (`rate_limit::daily_reserve`).
pub const PUBLISHES_PER_DAY: i64 = 100;
const OVER_QUOTA: &str = "This team reached its limit of 100 published posts a day. It stays \
                          scheduled and goes out once the limit frees up, within a day.";

/// The daily publish bucket of a team.
pub fn quota_bucket(team_id: Uuid) -> String {
    format!("publish:team:{team_id}")
}

/// Longest one post (or thread part) may take to publish or delete, unless its publisher says
/// otherwise (`Publisher::timeout`).
const TARGET_TIMEOUT: Duration = Duration::from_secs(120);
/// A target `publishing` this long was cut off (crash, restart) and may have posted.
const STUCK_MINUTES: i64 = 15;
const PUBLISH_LOCK: i64 = 0x0050_5542_4c49_5348;

pub const RECONNECT: &str =
    "Bluesky refused the app password. Reconnect the account under Connections.";
pub const THREADS_RECONNECT: &str =
    "Threads refused the account's token. Reconnect the account under Connections.";
pub const LINKEDIN_RECONNECT: &str =
    "LinkedIn refused the account's token, or it expired. Reconnect the account under Connections.";
pub const YOUTUBE_RECONNECT: &str = "Google refused the channel's token, or access was removed. \
                                      Reconnect the channel under Connections.";
pub const TIKTOK_RECONNECT: &str = "TikTok refused the account's token, or access was removed. \
                                     Reconnect the account under Connections.";
pub const X_RECONNECT: &str = "X refused the account's token, or access was removed. \
                               Reconnect the account under Connections.";
pub const META_RECONNECT: &str = "Meta refused the Page's token or a permission is missing. \
                                  Reconnect Facebook and Instagram under Connections.";
const COVER_CUT_OFF: &str =
    "Not set: publishing stopped before the cover image was set, and it is not \
     retried. Set the cover on the platform yourself.";
const COMMENT_CUT_OFF: &str =
    "Not posted: publishing stopped before the first comment went out, and it is not retried.";
const STUCK: &str = "Unknown, check the account: publishing stopped midway and it may have posted.";

/// The token to call the platform with. YouTube's lasts an hour, X's two and TikTok's a day, so
/// they are renewed first when near their end (`controllers::youtube::fresh_token`,
/// `controllers::x::fresh_token`, `controllers::tiktok::fresh_token`); the others use the stored secret as it is, and an unreadable one is a
/// reconnect.
async fn credential(
    app: &AppState,
    account: &social_accounts::Model,
) -> Result<String, PublishError> {
    if account.provider == "youtube" {
        return crate::controllers::youtube::fresh_token(app, account).await;
    }
    if account.provider == "x" {
        return crate::controllers::x::fresh_token(app, account).await;
    }
    if account.provider == "tiktok" {
        return crate::controllers::tiktok::fresh_token(app, account).await;
    }
    app.cipher
        .decrypt(&account.access_token)
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or(PublishError::Reconnect)
}

fn sql(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

async fn ids(db: &impl ConnectionTrait, stmt: Statement, col: &str) -> Result<Vec<Uuid>, DbErr> {
    let rows = db.query_all_raw(stmt).await?;
    rows.iter().map(|r| r.try_get("", col)).collect()
}

/// One publishing round: fail cut-off targets, claim due posts, pick up posts left `queued`,
/// publish them all and wait. Skips while another instance holds the lock.
pub async fn tick(app: &AppState) -> Result<(), DbErr> {
    let lock = app.db.begin().await?;
    let row = lock
        .query_one_raw(sql(
            "SELECT pg_try_advisory_xact_lock($1) AS ok",
            vec![PUBLISH_LOCK.into()],
        ))
        .await?;
    let locked: bool = match row {
        Some(r) => r.try_get("", "ok")?,
        None => false,
    };
    if !locked {
        return Ok(());
    }
    // One failing step is logged and the tick goes on; per-post work never aborts it.
    if let Err(e) = recover_stuck(&app.db).await {
        tracing::warn!(error = %e, "stuck target recovery failed");
    }
    // The quota is reserved per post in `publish_post`, together with marking it reserved.
    let mut due: Vec<Uuid> = claim_due(&app.db).await?.into_iter().map(|c| c.0).collect();
    // Posts left `queued` by a crash between claim and settle: fan-out and claims are
    // idempotent, so running them again only finishes the job.
    let stmt = sql(
        "SELECT id FROM posts WHERE status = 'queued' \
         AND updated_at < now() - make_interval(mins => $1) LIMIT $2",
        vec![(STUCK_MINUTES as i32).into(), CLAIM_BATCH.into()],
    );
    due.extend(ids(&app.db, stmt, "id").await?);
    let runs: Vec<_> = due
        .into_iter()
        .map(|id| {
            let app = app.clone();
            tokio::spawn(async move {
                if let Err(e) = publish_post(&app, id).await {
                    tracing::error!(error = %e, post = %id, "publish failed");
                }
            })
        })
        .collect();
    for run in runs {
        let _ = run.await;
    }
    lock.commit().await
}

/// Claims up to 50 due `scheduled` posts, oldest first, `scheduled` to `queued`: only posts of
/// a live set, of a team under its daily publish cap. Returns `(post, team)`.
pub async fn claim_due(db: &DatabaseConnection) -> Result<Vec<(Uuid, Uuid)>, DbErr> {
    // A CTE runs once; a LIMIT inside `IN (...)` may be re-run and claim more.
    let stmt = sql(
        "WITH due AS ( \
           SELECT d.id FROM posts d \
           WHERE d.status = 'scheduled' AND d.scheduled_at <= now() \
           AND EXISTS (SELECT 1 FROM social_sets s \
                       WHERE s.id = d.social_set_id AND s.deleted_at IS NULL) \
           AND (SELECT count(*) FROM rate_limit_hits h \
                WHERE h.bucket = 'publish:team:' || d.team_id::text \
                AND h.created_at > now() - interval '1 day') < $1 \
           ORDER BY d.scheduled_at, d.id LIMIT $2 \
           FOR UPDATE SKIP LOCKED) \
         UPDATE posts p SET status = 'queued', updated_at = now() FROM due \
         WHERE p.id = due.id AND p.status = 'scheduled' \
         RETURNING p.id, p.team_id",
        vec![PUBLISHES_PER_DAY.into(), CLAIM_BATCH.into()],
    );
    let rows = db.query_all_raw(stmt).await?;
    rows.iter()
        .map(|r| Ok((r.try_get("", "id")?, r.try_get("", "team_id")?)))
        .collect()
}

pub enum Quota {
    Reserved,
    /// The team is at its daily cap.
    Over,
    /// The post changed meanwhile; nothing was reserved.
    Lost,
}

/// In one transaction: reserve a hit of the team's daily publish cap (unless the post already
/// holds one) and run `update`, which must set `quota_reserved = true` on exactly this post.
/// The hit is rolled back when the update touches nothing.
pub async fn reserve_and(
    db: &DatabaseConnection,
    team_id: Uuid,
    already: bool,
    update: Statement,
) -> Result<Quota, DbErr> {
    let txn = db.begin().await?;
    if !already {
        let cap = [(quota_bucket(team_id), PUBLISHES_PER_DAY)];
        if reserve_in(&txn, &cap).await?.is_none() {
            return Ok(Quota::Over);
        }
    }
    if txn.execute_raw(update).await?.rows_affected() != 1 {
        return Ok(Quota::Lost);
    }
    txn.commit().await?;
    Ok(Quota::Reserved)
}

/// Puts a claimed post back to `scheduled`, with `error` saying why. It reserves again when it
/// is next claimed.
async fn unclaim(db: &DatabaseConnection, id: Uuid, error: Option<&str>) -> Result<(), DbErr> {
    let stmt = sql(
        "UPDATE posts SET status = 'scheduled', error = COALESCE($2, error), \
         quota_reserved = false, updated_at = now() \
         WHERE id = $1 AND status = 'queued'",
        vec![id.into(), error.map(str::to_owned).into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// Targets cut off mid-publish become `failed` and are never sent again; their posts settle.
pub async fn recover_stuck(db: &DatabaseConnection) -> Result<(), DbErr> {
    // Cut off after the platform took it (the remote id is saved first): it is live, so it is
    // published, and stays takeable down. Cut off midway through a thread: failed, naming the
    // part that may or may not have gone out; the saved parts stay takeable down.
    let stmt = sql(
        "SELECT t.id, t.post_id, t.remote_id, jsonb_array_length(t.parts) AS done, \
         jsonb_array_length(p.thread) AS total, p.platform FROM post_targets t \
         JOIN posts p ON p.id = t.post_id \
         WHERE t.status = 'publishing' AND t.remote_id IS NOT NULL \
         AND t.updated_at < now() - make_interval(mins => $1)",
        vec![(STUCK_MINUTES as i32).into()],
    );
    let mut posts = Vec::new();
    for row in db.query_all_raw(stmt).await? {
        let (id, post, remote): (Uuid, Uuid, String) = (
            row.try_get("", "id")?,
            row.try_get("", "post_id")?,
            row.try_get("", "remote_id")?,
        );
        let (done, total): (i32, i32) = (row.try_get("", "done")?, row.try_get("", "total")?);
        let platform: String = row.try_get("", "platform")?;
        let (status, error) = if done < total {
            let msg = format!(
                "Unknown, check the account: publishing stopped midway and thread post {} of \
                 {total} may have posted.",
                done + 1
            );
            ("failed", Some(msg))
        } else {
            ("published", None)
        };
        // shortcut: only Bluesky, LinkedIn, YouTube and X links are made from the id (X's without
        // the username, which `/i/status` does not need); a Threads link (looked up at publish)
        // is left out here. Look it up if recovered Threads posts need one.
        let url = bluesky::post_url(&remote)
            .or_else(|| {
                remote
                    .starts_with("urn:li:")
                    .then(|| linkedin::post_url(&remote))
            })
            .or_else(|| (platform == "youtube").then(|| youtube::watch_url(&remote)))
            .or_else(|| (platform == "x").then(|| x::status_url(None, &remote)));
        let stmt = sql(
            &with_event(
                // Published here, so a first comment or a cover never went out and must not later: say so.
                "UPDATE post_targets SET status = $3, remote_url = $2, error = $4, \
                 comment_status = CASE WHEN $3 = 'published' AND EXISTS (SELECT 1 FROM posts \
                   WHERE id = post_targets.post_id AND first_comment IS NOT NULL) \
                   THEN 'failed' END, \
                 comment_error = CASE WHEN $3 = 'published' AND EXISTS (SELECT 1 FROM posts \
                   WHERE id = post_targets.post_id AND first_comment IS NOT NULL) \
                   THEN $5 END, \
                 cover_error = CASE WHEN $3 = 'published' AND EXISTS (SELECT 1 FROM posts \
                   WHERE id = post_targets.post_id AND cover IS NOT NULL) THEN $6 END, \
                 published_at = COALESCE(published_at, now()), updated_at = now() \
                 WHERE id = $1 AND status = 'publishing'",
            ),
            vec![
                id.into(),
                url.into(),
                status.into(),
                error.into(),
                COMMENT_CUT_OFF.into(),
                // Facebook and YouTube set the cover after publish; Instagram sends it with the video.
                matches!(platform.as_str(), "facebook" | "youtube")
                    .then_some(COVER_CUT_OFF)
                    .into(),
            ],
        );
        db.execute_raw(stmt).await?;
        posts.push(post);
    }
    // A first comment cut off midway may have posted: it stays unsent, never tried again.
    let stmt = sql(
        "UPDATE post_targets SET comment_status = 'failed', comment_error = $1, \
         updated_at = now() WHERE comment_status = 'posting' \
         AND updated_at < now() - make_interval(mins => $2)",
        vec![STUCK.into(), (STUCK_MINUTES as i32).into()],
    );
    db.execute_raw(stmt).await?;
    let stmt = sql(
        &with_event(
            "UPDATE post_targets SET status = 'failed', error = $1, updated_at = now() \
             WHERE status = 'publishing' AND remote_id IS NULL \
             AND updated_at < now() - make_interval(mins => $2)",
        ),
        vec![STUCK.into(), (STUCK_MINUTES as i32).into()],
    );
    posts.extend(ids(db, stmt, "post_id").await?);
    posts.sort();
    posts.dedup();
    for id in posts {
        if let Err(e) = settle(db, id).await {
            tracing::warn!(error = %e, post = %id, "settle after recovery failed");
        }
    }
    Ok(())
}

async fn fail_post(db: &DatabaseConnection, id: Uuid, msg: &str) -> Result<(), DbErr> {
    let stmt = sql(
        "UPDATE posts SET status = 'failed', error = $2, updated_at = now() \
         WHERE id = $1 AND status = 'queued'",
        vec![id.into(), msg.into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// Publishes a `queued` post: one target per account of its platform in its set, each
/// published at most once, then the post settles.
pub async fn publish_post(app: &AppState, id: Uuid) -> Result<(), DbErr> {
    let Ok(_slot) = POST_SLOTS.acquire().await else {
        return Ok(());
    };
    let db = &app.db;
    let Some(post) = posts::Entity::find_by_id(id).one(db).await? else {
        return Ok(());
    };
    if post.status != "queued" {
        return Ok(());
    }
    // Whatever queued it (a tick, publish now, recovery), nothing publishes without its hit.
    if !post.quota_reserved {
        let mark = sql(
            "UPDATE posts SET quota_reserved = true \
             WHERE id = $1 AND status = 'queued' AND NOT quota_reserved",
            vec![id.into()],
        );
        let back = match reserve_and(db, post.team_id, false, mark).await {
            Ok(Quota::Reserved) => None,
            Ok(Quota::Over) => Some(Some(OVER_QUOTA)),
            Ok(Quota::Lost) => return Ok(()),
            Err(e) => {
                tracing::warn!(error = %e, post = %id, "publish quota reserve failed");
                Some(None)
            }
        };
        if let Some(error) = back {
            return unclaim(db, id, error).await;
        }
    }
    let set_live = social_sets::Entity::find_by_id(post.social_set_id)
        .filter(social_sets::Column::DeletedAt.is_null())
        .one(db)
        .await?
        .is_some();
    if !set_live {
        // The set was deleted after the claim: back to `scheduled`, to go out if it returns.
        return unclaim(db, id, None).await;
    }
    if publisher(app, &post.platform).is_none() {
        let msg = format!("Publishing to {} is not available yet.", post.platform);
        return fail_post(db, id, &msg).await;
    }
    let in_set = Query::select()
        .column(membership::Column::SocialAccountId)
        .from(membership::Entity)
        .and_where(membership::Column::SocialSetId.eq(post.social_set_id))
        .to_owned();
    let accounts: Vec<Uuid> = social_accounts::Entity::find()
        .select_only()
        .column(social_accounts::Column::Id)
        .filter(social_accounts::Column::TeamId.eq(post.team_id))
        .filter(social_accounts::Column::Provider.eq(&post.platform))
        .filter(social_accounts::Column::Id.in_subquery(in_set))
        .into_tuple()
        .all(db)
        .await?;
    for account in &accounts {
        let stmt = sql(
            "INSERT INTO post_targets (id, post_id, social_account_id, status) \
             VALUES ($1, $2, $3, 'pending') ON CONFLICT (post_id, social_account_id) DO NOTHING",
            vec![Uuid::now_v7().into(), id.into(), (*account).into()],
        );
        db.execute_raw(stmt).await?;
    }
    let targets = post_targets::Entity::find()
        .filter(post_targets::Column::PostId.eq(id))
        .all(db)
        .await?;
    if targets.is_empty() {
        let msg = format!("No {} account is in this post's set.", post.platform);
        return fail_post(db, id, &msg).await;
    }
    let pending: Vec<Uuid> = targets
        .into_iter()
        .filter(|t| t.status == "pending")
        .map(|t| t.id)
        .collect();
    let stored = if pending.is_empty() {
        Arc::default()
    } else {
        Arc::new(ingest(app, &post).await)
    };
    let runs: Vec<_> = pending
        .into_iter()
        .map(|t| {
            let (app, post, stored) = (app.clone(), post.clone(), stored.clone());
            tokio::spawn(async move { publish_target(&app, &post, t, &stored).await })
        })
        .collect();
    for run in runs {
        match run.await {
            Ok(Err(e)) => tracing::error!(error = %e, post = %id, "target publish failed"),
            Err(e) => tracing::error!(error = %e, post = %id, "target task failed"),
            Ok(Ok(())) => {}
        }
    }
    let settled = settle(db, id).await;
    app.media.release_post(db, id).await;
    settled
}

/// Each media URL of a post and its thread, downloaded once into storage, or why not.
type Media = HashMap<String, Result<Stored, Fail>>;

async fn ingest(app: &AppState, post: &posts::Model) -> Media {
    let cap = max_bytes(&post.platform);
    let thread: Vec<ThreadPart> = serde_json::from_value(post.thread.clone()).unwrap_or_default();
    let root: Vec<String> = serde_json::from_value(post.media.clone()).unwrap_or_default();
    let urls: Vec<String> = root
        .into_iter()
        .chain(thread.into_iter().flat_map(|p| p.media))
        .collect();
    let cover: Vec<String> = post.cover.clone().into_iter().collect();
    let cover_cap = cover_bytes(&post.platform);
    let (mut found, covers) = tokio::join!(
        app.media.ingest_all(&app.db, post.id, &urls, cap),
        app.media.ingest_all(&app.db, post.id, &cover, cover_cap),
    );
    // A cover that is also a media link keeps the media's result, and then fails its type check.
    for (url, got) in covers {
        found.entry(url).or_insert(got);
    }
    found
}

/// The largest cover image stored for `platform`, as its API documents: Instagram 8 MB, Facebook
/// 10 MB, YouTube 2 MB (the limit YouTube states for thumbnails).
fn cover_bytes(platform: &str) -> usize {
    match platform {
        "instagram" => 8_000_000,
        "facebook" => 10_000_000,
        "youtube" => 2_000_000,
        _ => crate::media::MAX_BYTES,
    }
}

/// What to tell the user about a cover that could not be stored.
fn cover_error(fail: Fail, platform: &str, name: &str) -> String {
    match fail {
        Fail::TooLarge => format!(
            "The cover image is over {} MB, the largest {name} takes.",
            cover_bytes(platform) / 1_000_000
        ),
        f => media_error(f, 1, platform).replacen("Image 1", "The cover image", 1),
    }
}

/// The post's cover, checked and read before anything is posted, so a bad cover fails the target
/// and never a live post. It needs the post's one media item to be a video, and a type the
/// platform takes. Its bytes are read unless it goes in the publish call by URL.
async fn load_cover(
    app: &AppState,
    publisher: &dyn Publisher,
    post: &posts::Model,
    (stored, media): (&Media, &[String]),
) -> Result<Option<Attachment>, String> {
    let Some(url) = post.cover.as_deref() else {
        return Ok(None);
    };
    let video = matches!(media, [one] if matches!(
        stored.get(one), Some(Ok(s)) if s.content_type.starts_with("video/")));
    if !video {
        return Err("A cover needs a post with exactly one video.".into());
    }
    let (platform, name) = (post.platform.as_str(), publisher.name());
    let file = match stored.get(url) {
        Some(Ok(file)) => file,
        Some(Err(fail)) => return Err(cover_error(*fail, platform, name)),
        None => return Err(cover_error(Fail::Storage, platform, name)),
    };
    let types = publisher.cover_types();
    if !types.contains(&file.content_type.as_str()) {
        return Err(match types.len() {
            0 => format!("{name} cannot set a cover image."),
            1 => format!("{name} takes a JPEG cover image."),
            _ => format!("{name} takes a JPEG or PNG cover image."),
        });
    }
    let how = (publisher.cover_in_publish(), platform);
    let mut loaded = load(app, stored, &[url.to_owned()], how)
        .await
        .map_err(|m| m.replacen("Image 1", "The cover image", 1))?;
    Ok(loaded.pop())
}

/// The largest file stored for `platform`: Bluesky takes 1 MB images; Threads fetches up to 8 MB
/// images and 1 GB videos itself, so brain's own cap holds there.
fn max_bytes(platform: &str) -> usize {
    match platform {
        "bluesky" => bluesky::MAX_IMAGE_BYTES,
        _ => crate::media::MAX_BYTES,
    }
}

/// What to tell the user about image `n` that could not be stored.
fn media_error(fail: Fail, n: usize, platform: &str) -> String {
    match fail {
        Fail::Blocked => format!("Image {n} is not a public https URL."),
        Fail::NotShared => format!(
            "Image {n} is a Google Drive file that is not shared publicly. Share it with \
             anyone with the link."
        ),
        Fail::Http(code) => format!("Image {n} could not be downloaded (HTTP {code})."),
        Fail::Unreachable => format!("Image {n} could not be downloaded."),
        Fail::TooLarge if platform == "bluesky" => {
            format!("Image {n} is over 1 MB, Bluesky's limit.")
        }
        Fail::TooLarge => format!(
            "Image {n} is over {} MB, the largest file SocialReins takes.",
            max_bytes(platform) / 1_000_000
        ),
        Fail::Unsupported if platform == "linkedin" => {
            format!("Image {n} is not a JPEG, PNG or GIF image, an MP4 video or a PDF.")
        }
        Fail::Unsupported => format!("Image {n} is not a JPEG, PNG, WebP or GIF."),
        Fail::Storage => format!("Image {n} could not be stored. Try again."),
        Fail::TimedOut => format!(
            "Image {n} did not download in time: the post's media took over a minute. Use \
             faster or smaller links."
        ),
    }
}

/// The first media problem of the post or its thread, so nothing goes out with one.
fn media_problem(
    media: &Media,
    root: &[String],
    thread: &[ThreadPart],
    platform: &str,
) -> Option<String> {
    let first = |urls: &[String]| {
        urls.iter()
            .enumerate()
            .find_map(|(i, u)| match media.get(u) {
                Some(Err(f)) => Some(media_error(*f, i + 1, platform)),
                _ => None,
            })
    };
    first(root).or_else(|| {
        thread.iter().enumerate().find_map(|(i, p)| {
            let msg = first(&p.media)?;
            Some(format!("Thread post {} of {}: {msg}", i + 1, thread.len()))
        })
    })
}

/// Each URL's stored file: its type and brain link, and its bytes unless `by_url`.
async fn load(
    app: &AppState,
    media: &Media,
    urls: &[String],
    (by_url, platform): (bool, &str),
) -> Result<Vec<Attachment>, String> {
    let mut files = Vec::new();
    for (i, url) in urls.iter().enumerate() {
        let n = i + 1;
        let Some(Ok(s)) = media.get(url) else {
            return Err(media_error(Fail::Storage, n, platform));
        };
        let bytes = if by_url {
            Vec::new()
        } else {
            match app.media.read(&s.key).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::warn!(error = %e, "stored media read failed");
                    return Err(media_error(Fail::Storage, n, platform));
                }
            }
        };
        files.push(Attachment {
            bytes: bytes.into(),
            content_type: s.content_type.clone(),
            url: crate::media::link(app, s),
        });
    }
    Ok(files)
}

async fn fail_target(db: &DatabaseConnection, id: Uuid, msg: &str) -> Result<(), DbErr> {
    let stmt = sql(
        &with_event(
            "UPDATE post_targets SET status = 'failed', error = $2, updated_at = now() \
             WHERE id = $1 AND status = 'publishing'",
        ),
        vec![id.into(), msg.into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// Claims a target (`pending` with no remote id, to `publishing`); false when another run has it.
pub async fn claim_target(db: &DatabaseConnection, id: Uuid) -> Result<bool, DbErr> {
    let stmt = sql(
        "UPDATE post_targets SET status = 'publishing', updated_at = now() \
         WHERE id = $1 AND status = 'pending' AND remote_id IS NULL",
        vec![id.into()],
    );
    Ok(db.execute_raw(stmt).await?.rows_affected() == 1)
}

/// One post (or thread part), its media read from storage, with its time limit; `Err` holds what
/// to tell the user.
async fn send(
    (app, stored): (&AppState, &Media),
    publisher: &dyn Publisher,
    account: &social_accounts::Model,
    secret: &str,
    (body, media, cover, tiktok): (
        &str,
        &[String],
        Option<&Attachment>,
        Option<&tiktok::Options>,
    ),
    reply: Option<(&Remote, &Remote)>,
) -> Result<Remote, String> {
    let how = (publisher.by_url(), account.provider.as_str());
    let media = load(app, stored, media, how).await?;
    // Stored PDFs exist for LinkedIn's document posts; no other platform takes one.
    if let Some(n) = media
        .iter()
        .position(|a| a.content_type == linkedin::PDF_TYPE)
    {
        if account.provider != "linkedin" {
            return Err(format!(
                "Image {} is a PDF. Only LinkedIn posts take a PDF, as a document.",
                n + 1
            ));
        }
    }
    let sent = async {
        match (cover, tiktok) {
            (Some(c), _) => {
                publisher
                    .publish_with_cover(account, secret, body, &media, c)
                    .await
            }
            (None, Some(t)) => {
                publisher
                    .publish_with_tiktok(account, secret, body, &media, t)
                    .await
            }
            (None, None) => {
                publisher
                    .publish(account, secret, body, &media, reply)
                    .await
            }
        }
    };
    match tokio::time::timeout(publisher.timeout(), sent).await {
        Ok(Ok(remote)) => Ok(remote),
        Ok(Err(PublishError::Reconnect)) => Err(publisher.reconnect().to_owned()),
        Ok(Err(PublishError::Failed(msg))) => Err(msg),
        Err(_) => Err("Timed out; it may have posted, check the account.".to_owned()),
    }
}

async fn publish_target(
    app: &AppState,
    post: &posts::Model,
    id: Uuid,
    stored: &Media,
) -> Result<(), DbErr> {
    let Ok(_slot) = TARGET_SLOTS.acquire().await else {
        return Ok(());
    };
    let db = &app.db;
    if !claim_target(db, id).await? {
        return Ok(());
    }
    let target = post_targets::Entity::find_by_id(id).one(db).await?;
    let account = match target.and_then(|t| t.social_account_id) {
        Some(a) => social_accounts::Entity::find_by_id(a).one(db).await?,
        None => None,
    };
    let Some(account) = account else {
        return fail_target(db, id, "The account was disconnected.").await;
    };
    let Some(publisher) = publisher(app, &post.platform) else {
        return fail_target(db, id, "This platform cannot publish yet.").await;
    };
    let secret = match credential(app, &account).await {
        Ok(secret) => secret,
        Err(PublishError::Reconnect) => return fail_target(db, id, publisher.reconnect()).await,
        Err(PublishError::Failed(msg)) => return fail_target(db, id, &msg).await,
    };
    let media: Vec<String> = serde_json::from_value(post.media.clone()).unwrap_or_default();
    let thread: Vec<ThreadPart> = serde_json::from_value(post.thread.clone()).unwrap_or_default();
    if let Some(msg) = media_problem(stored, &media, &thread, &post.platform) {
        return fail_target(db, id, &msg).await;
    }
    let cover = match load_cover(app, publisher, post, (stored, &media)).await {
        Ok(cover) => cover,
        Err(msg) => return fail_target(db, id, &msg).await,
    };
    let tiktok: Option<tiktok::Options> = post
        .tiktok
        .clone()
        .and_then(|v| serde_json::from_value(v).ok());
    let with = (app, stored);
    let in_call = cover.as_ref().filter(|_| publisher.cover_in_publish());
    let root = match send(
        with,
        publisher,
        &account,
        &secret,
        (&post.body, &media, in_call, tiktok.as_ref()),
        None,
    )
    .await
    {
        Ok(remote) => remote,
        Err(msg) => return fail_target(db, id, &msg).await,
    };
    // The remote id first: whatever fails after this, the post is known to be live.
    let stmt = sql(
        "UPDATE post_targets SET remote_id = $2, remote_cid = $3, remote_url = $4, \
         published_at = now(), updated_at = now() WHERE id = $1",
        vec![
            id.into(),
            root.id.clone().into(),
            root.cid.clone().into(),
            root.url
                .clone()
                .or_else(|| publisher.url(&account, &root.id))
                .into(),
        ],
    );
    db.execute_raw(stmt).await?;
    // Each part right after the platform takes it, so its parent is always saved before it.
    let mut parent = root.clone();
    for (i, part) in thread.iter().enumerate() {
        // A thread can outlast the token it started with: fetch the credential again before
        // each part (renewed when near its end, through the locked path).
        let secret = match credential(app, &account).await {
            Ok(secret) => secret,
            Err(e) => {
                let why = match e {
                    PublishError::Reconnect => publisher.reconnect().to_owned(),
                    PublishError::Failed(msg) => msg,
                };
                let msg = format!(
                    "Thread post {} of {}: {why} The posts before it are up.",
                    i + 1,
                    thread.len()
                );
                return fail_target(db, id, &msg).await;
            }
        };
        let content = (part.body.as_str(), part.media.as_slice(), None, None);
        let made = match send(
            with,
            publisher,
            &account,
            &secret,
            content,
            Some((&parent, &root)),
        )
        .await
        {
            Ok(remote) => remote,
            Err(msg) => {
                let msg = format!(
                    "Thread post {} of {}: {msg} The posts before it are up.",
                    i + 1,
                    thread.len()
                );
                return fail_target(db, id, &msg).await;
            }
        };
        let saved = serde_json::json!([{
            "remote_id": made.id, "cid": made.cid,
            "remote_url": made.url.clone().or_else(|| publisher.url(&account, &made.id)),
            "published_at": chrono::Utc::now().fixed_offset(),
        }]);
        let stmt = sql(
            "UPDATE post_targets SET parts = parts || $2, updated_at = now() WHERE id = $1",
            vec![id.into(), saved.into()],
        );
        if let Err(e) = db.execute_raw(stmt).await {
            let msg = format!(
                "Thread post {} of {} went out but could not be saved: it may be live, check \
                 the account.",
                i + 1,
                thread.len()
            );
            // Best effort: with the database down, recovery fails it later with a like message.
            let _ = fail_target(db, id, &msg).await;
            return Err(e);
        }
        parent = made;
    }
    // A post with a first comment is claimed for it in this same statement (`posting`, only
    // from null), so a crash after it falls into recovery of a stuck `posting` and never leaves
    // a published target whose comment is silently forgotten.
    let stmt = sql(
        &with_event(
            "UPDATE post_targets SET status = 'published', error = NULL, \
             comment_status = CASE WHEN comment_status IS NULL AND EXISTS (SELECT 1 FROM posts \
               WHERE id = post_targets.post_id AND first_comment IS NOT NULL) \
               THEN 'posting' ELSE comment_status END, \
             updated_at = now() WHERE id = $1 AND status = 'publishing'",
        ),
        vec![id.into()],
    );
    let saved = db.execute_raw(stmt).await?;
    // Only after the post is saved as published on this target.
    if saved.rows_affected() != 1 {
        return Ok(());
    }
    if let Some(cover) = cover.filter(|_| !publisher.cover_in_publish()) {
        set_cover(app, publisher, &account, (id, &root.id), &cover).await;
    }
    match post.first_comment.as_deref() {
        Some(text) => first_comment(app, publisher, &account, (id, &root.id), text).await,
        None => Ok(()),
    }
}

/// Sets the cover of a live video, once. Whatever happens only sets the target's `cover_error`
/// (a warning): the target stays `published`, and nothing is retried, so no step can post a
/// second copy.
async fn set_cover(
    app: &AppState,
    publisher: &dyn Publisher,
    account: &social_accounts::Model,
    (target, remote_id): (Uuid, &str),
    cover: &Attachment,
) {
    tokio::time::sleep(publisher.comment_wait()).await;
    let sent = async {
        // The token may have aged during the upload: fetch it again (renewed when near its end).
        let secret = credential(app, account).await?;
        publisher
            .set_cover(account, &secret, remote_id, cover)
            .await
    };
    let why = match tokio::time::timeout(TARGET_TIMEOUT, sent).await {
        Ok(Ok(())) => return,
        Ok(Err(PublishError::Reconnect)) => publisher.reconnect().to_owned(),
        Ok(Err(PublishError::Failed(msg))) => msg,
        Err(_) => "Timed out; the cover may have been set, check the account.".to_owned(),
    };
    tracing::info!(target = %target, "cover not set");
    let msg = format!("The video is up, but its cover image was not set. {why}");
    let stmt = sql(
        "UPDATE post_targets SET cover_error = $2, updated_at = now() WHERE id = $1",
        vec![target.into(), msg.into()],
    );
    if let Err(e) = app.db.execute_raw(stmt).await {
        tracing::warn!(error = %e, target = %target, "cover warning not saved");
    }
}

/// Posts the first comment on a published target, at most once: the claim (`comment_status`
/// null to `posting`) was made by the statement that published the target and nothing resets
/// it, so a crash, a timeout or a refusal is never followed by a second try that could post
/// twice. Whatever happens only sets the comment's own columns: the target stays `published`.
async fn first_comment(
    app: &AppState,
    publisher: &dyn Publisher,
    account: &social_accounts::Model,
    (target, remote_id): (Uuid, &str),
    text: &str,
) -> Result<(), DbErr> {
    let db = &app.db;
    // Only the claim made by that statement may send: any other state means it is not ours.
    let held = sql(
        "UPDATE post_targets SET updated_at = now() \
         WHERE id = $1 AND status = 'published' AND comment_status = 'posting'",
        vec![target.into()],
    );
    if db.execute_raw(held).await?.rows_affected() != 1 {
        return Ok(());
    }
    tokio::time::sleep(publisher.comment_wait()).await;
    let sent = async {
        // The token may have aged during the upload: fetch it again (renewed when near its end).
        let secret = credential(app, account).await?;
        publisher.comment(account, &secret, remote_id, text).await
    };
    let outcome = match tokio::time::timeout(TARGET_TIMEOUT, sent).await {
        Ok(Ok(comment_id)) => Ok(comment_id),
        Ok(Err(PublishError::Reconnect)) => Err(publisher.reconnect().to_owned()),
        Ok(Err(PublishError::Failed(msg))) => Err(msg),
        Err(_) => Err("Timed out; the comment may have posted, check the account.".to_owned()),
    };
    let (status, comment_id, error) = match outcome {
        Ok(comment_id) => ("posted", Some(comment_id), None),
        Err(msg) => {
            tracing::info!(target = %target, "first comment failed");
            ("failed", None, Some(msg))
        }
    };
    let stmt = sql(
        "UPDATE post_targets SET comment_status = $2, comment_id = $3, comment_error = $4, \
         updated_at = now() WHERE id = $1 AND comment_status = 'posting'",
        vec![
            target.into(),
            status.into(),
            comment_id.into(),
            error.into(),
        ],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// An account in messages: `@handle`, else its name (a Facebook Page has no handle).
fn named(a: &social_accounts::Model) -> String {
    match (&a.handle, &a.display_name) {
        (Some(h), _) => format!("@{h}"),
        (None, Some(n)) => n.clone(),
        (None, None) => "@account".into(),
    }
}

/// Settles a `queued` post once no target is `pending` or `publishing`: `failed` when every
/// target failed with nothing up, else `published`, with each failed account named in `error`. The post row
/// is locked, so concurrent settles agree.
pub async fn settle(db: &DatabaseConnection, id: Uuid) -> Result<(), DbErr> {
    let txn = db.begin().await?;
    let post = posts::Entity::find_by_id(id)
        .lock_exclusive()
        .one(&txn)
        .await?;
    if !post.is_some_and(|p| p.status == "queued") {
        return txn.commit().await;
    }
    let targets = post_targets::Entity::find()
        .filter(post_targets::Column::PostId.eq(id))
        .all(&txn)
        .await?;
    let open = targets
        .iter()
        .any(|t| t.status == "pending" || t.status == "publishing");
    if targets.is_empty() || open {
        return txn.commit().await;
    }
    let failed: Vec<&post_targets::Model> =
        targets.iter().filter(|t| t.status == "failed").collect();
    let mut errors = Vec::new();
    for t in &failed {
        let account = match t.social_account_id {
            Some(a) => social_accounts::Entity::find_by_id(a).one(&txn).await?,
            None => None,
        };
        let name = account.map_or_else(|| "A disconnected account".to_owned(), |a| named(&a));
        errors.push(format!(
            "{name}: {}",
            t.error.as_deref().unwrap_or("failed")
        ));
    }
    // A target that failed midway through a thread is still up, so its post is published.
    let status = if failed.len() == targets.len() && targets.iter().all(|t| t.remote_id.is_none()) {
        "failed"
    } else {
        "published"
    };
    let error = (!errors.is_empty()).then(|| errors.join("; "));
    let stmt = sql(
        "UPDATE posts SET status = $2, error = $3, updated_at = now(), \
         published_at = CASE WHEN $2 = 'published' THEN now() ELSE published_at END \
         WHERE id = $1 AND status = 'queued'",
        vec![id.into(), status.into(), error.into()],
    );
    txn.execute_raw(stmt).await?;
    txn.commit().await
}

/// Takes a `published` post down from every account it is still up on, a thread's parts last
/// first, then its root. Each part that comes down leaves the target's `parts` and each target
/// whose root comes down is marked `deleted`, so a retry only tries the rest; the post becomes
/// `deleted` once none is left. Returns what to tell the user about each target that stayed up.
pub async fn takedown(app: &AppState, post: &posts::Model) -> Result<Vec<String>, DbErr> {
    let db = &app.db;
    // A target that failed midway through a thread is up too.
    let up = post_targets::Entity::find()
        .filter(post_targets::Column::PostId.eq(post.id))
        .filter(post_targets::Column::Status.is_in(["published", "failed"]))
        .filter(post_targets::Column::RemoteId.is_not_null())
        .all(db)
        .await?;
    let mut problems = Vec::new();
    for t in up {
        let account = match t.social_account_id {
            Some(a) => social_accounts::Entity::find_by_id(a).one(db).await?,
            None => None,
        };
        let Some(account) = account else {
            problems.push(
                "A disconnected account: connect it again, or delete the post in its app.".into(),
            );
            continue;
        };
        let name = named(&account);
        let secret = match credential(app, &account).await {
            Ok(secret) => Some(secret),
            Err(PublishError::Reconnect) => None,
            Err(PublishError::Failed(msg)) => {
                problems.push(format!(
                    "{name}: {msg} Delete the post in the app if it is still up."
                ));
                continue;
            }
        };
        // The credential above only tells early that the account needs a reconnect; each
        // delete fetches its own (`delete_one`).
        let (Some(publisher), Some(_), Some(root)) = (
            publisher(app, &post.platform),
            secret,
            t.remote_id.as_deref(),
        ) else {
            problems.push(format!(
                "{name}: reconnect the account under Connections, or delete the post in the app."
            ));
            continue;
        };
        let parts: Vec<serde_json::Value> =
            serde_json::from_value(t.parts.clone()).unwrap_or_default();
        let mut problem = None;
        for (i, part) in parts.iter().enumerate().rev() {
            let remote_id = part["remote_id"].as_str().unwrap_or_default();
            problem = delete_one(app, publisher, &account, remote_id, &name).await;
            if problem.is_some() {
                break;
            }
            let stmt = sql(
                "UPDATE post_targets SET parts = parts - $2::int, updated_at = now() \
                 WHERE id = $1",
                vec![t.id.into(), (i as i32).into()],
            );
            db.execute_raw(stmt).await?;
        }
        if problem.is_none() {
            problem = delete_one(app, publisher, &account, root, &name).await;
        }
        match problem {
            Some(p) => problems.push(p),
            None => {
                let stmt = sql(
                    &with_event(
                        "UPDATE post_targets SET status = 'deleted', updated_at = now() \
                         WHERE id = $1 AND status IN ('published', 'failed')",
                    ),
                    vec![t.id.into()],
                );
                db.execute_raw(stmt).await?;
            }
        }
    }
    if problems.is_empty() {
        let stmt = sql(
            "UPDATE posts SET status = 'deleted', updated_at = now() \
             WHERE id = $1 AND status = 'published' AND NOT EXISTS \
             (SELECT 1 FROM post_targets WHERE post_id = $1 AND remote_id IS NOT NULL \
              AND status IN ('published', 'failed'))",
            vec![post.id.into()],
        );
        db.execute_raw(stmt).await?;
    }
    Ok(problems)
}

/// Deletes one post on the platform; `Some` says what to tell the user when it stayed up. The
/// credential is fetched again for each call (renewed when near its end, through the locked
/// path), since a thread's take down can outlast the token it started with.
async fn delete_one(
    app_state: &AppState,
    publisher: &dyn Publisher,
    account: &social_accounts::Model,
    remote_id: &str,
    name: &str,
) -> Option<String> {
    let app = publisher.name();
    // The row as it is now: an earlier call of this take down may have renewed the token.
    let account = match social_accounts::Entity::find_by_id(account.id)
        .one(&app_state.db)
        .await
    {
        Ok(Some(fresh)) => fresh,
        _ => account.clone(),
    };
    let account = &account;
    let secret = match credential(app_state, account).await {
        Ok(secret) => secret,
        Err(PublishError::Reconnect) => {
            return Some(format!(
                "{name}: {} Or delete the post in the {app} app.",
                publisher.reconnect()
            ))
        }
        Err(PublishError::Failed(msg)) => {
            return Some(format!(
                "{name}: {msg} Delete the post in the {app} app if it is still up."
            ))
        }
    };
    let done = tokio::time::timeout(
        TARGET_TIMEOUT,
        publisher.delete(account, &secret, remote_id),
    )
    .await;
    match done {
        Ok(Ok(())) => None,
        Ok(Err(PublishError::Reconnect)) => Some(format!(
            "{name}: {} Or delete the post in the {app} app.",
            publisher.reconnect()
        )),
        Ok(Err(PublishError::Failed(msg))) => Some(format!(
            "{name}: {msg} Delete the post in the {app} app if it is still up."
        )),
        Err(_) => Some(format!(
            "{name}: {app} did not answer in time. Try again, or delete it in the app."
        )),
    }
}
