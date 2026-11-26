//! Publishing: claim due posts, fan each out to its set's accounts, publish every target once,
//! settle the post; and take published posts down. A thread goes out root first, then each
//! part as a reply to the one before, each saved on its target the moment the platform takes
//! it. Every step is a conditional update, so two instances (or a tick and "publish now") never
//! send a post twice. A target retries by itself once, about 5 minutes later, only after an error that certainly sent nothing (`retry_safe`); anything else that failed is re-queued only by `POST /posts/{id}/retry`, on request. A scheduled post more than `LATE_MINUTES` past its time is never published: it goes back to `draft` (`draft_late`). Each target status change queues its webhook event in the same
//! statement (`webhooks::with_event`). Media is downloaded into brain's storage before any
//! target is claimed (`media.rs`) and deleted once the post settles.
use std::{
    collections::{HashMap, HashSet},
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
    billing,
    connectors::{
        bluesky::{self, Bluesky},
        linkedin::{self, LinkedIn},
        meta::{self, Facebook, Instagram},
        pinterest::{self, Pinterest},
        threads::{self, Threads},
        tiktok::{self, TikTok},
        unsent,
        x::{self, X},
        youtube::{self, YouTube},
    },
    controllers::posts::ThreadPart,
    expiry,
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

/// One stored image or video: its bytes, type, and the unguessable brain URL that serves it
/// while the post publishes (`media::link`). An image, cover or PDF is held in memory; a video,
/// and anything for a publisher that fetches by URL, stays in the store (`media::Blob`).
pub struct Attachment {
    pub bytes: crate::media::Blob,
    pub content_type: String,
    pub url: String,
    /// The image's alt text; empty for none.
    pub alt: String,
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

    /// `publish` with the YouTube settings the user chose for the post. Only YouTube has any.
    async fn publish_with_youtube(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        _: &youtube::Options,
    ) -> Result<Remote, PublishError> {
        self.publish(account, secret, body, media, None).await
    }

    /// `publish` with the Pinterest settings the user chose for the post. Only Pinterest has any.
    async fn publish_with_pinterest(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        _: &pinterest::Options,
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

pub(crate) fn creds<'a>(
    account: &'a social_accounts::Model,
    secret: &'a str,
) -> bluesky::Creds<'a> {
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
        let media: Vec<(&crate::media::Blob, &str, &str)> = media
            .iter()
            .map(|a| (&a.bytes, a.content_type.as_str(), a.alt.as_str()))
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

    fn timeout(&self) -> Duration {
        bluesky::PUBLISH_TIMEOUT
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
            Ok(threads::Item {
                url: &a.url,
                video,
                alt: &a.alt,
            })
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
                alt: &a.alt,
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
                alt: &a.alt,
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
    /// A post without YouTube settings: private.
    async fn publish(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        _: Option<(&Remote, &Remote)>,
    ) -> Result<Remote, PublishError> {
        let options = youtube::Options::default();
        self.publish_with_youtube(account, secret, body, media, &options)
            .await
    }

    /// One MP4 or MOV video, titled by the first line of the body, with the chosen visibility.
    /// `secret` is already renewed (`credential`). The video id is returned the moment YouTube
    /// has the bytes, and the caller saves it first.
    async fn publish_with_youtube(
        &self,
        _: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        options: &youtube::Options,
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
        let id = self
            .upload(
                secret,
                socialreins_core::post::youtube_text(body),
                (&video.bytes, &video.content_type),
                options.privacy(),
            )
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
            bytes: cover.bytes.held()?,
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

impl Pinterest {
    /// One pin on the board the post names, else the account's default: one JPEG or PNG image, 2
    /// to 5 of them (a carousel), or one MP4 or MOV video. The post's body is the description.
    /// `secret` is already renewed (`credential`). Pinterest takes one alt text for the pin: the
    /// first one given, cut to its limit.
    async fn pin(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        options: &pinterest::Options,
    ) -> Result<Remote, PublishError> {
        let board = options
            .board_id
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .or_else(|| account.meta["default_board_id"].as_str())
            .ok_or_else(|| {
                PublishError::Failed(
                    "No board to pin to. Choose a board for this pin, or set the account's default board under Connections."
                        .into(),
                )
            })?;
        let image = |a: &Attachment| pinterest::IMAGE_TYPES.contains(&a.content_type.as_str());
        let videos = media
            .iter()
            .filter(|a| a.content_type.starts_with("video/"));
        let source = match (videos.count(), media) {
            (1, [v]) if pinterest::VIDEO_TYPES.contains(&v.content_type.as_str()) => {
                pinterest::Media::Video(&v.bytes, &v.content_type)
            }
            (0, [one]) if image(one) => pinterest::Media::Image(&one.url),
            (0, many) if (2..=5).contains(&many.len()) && many.iter().all(image) => {
                pinterest::Media::Carousel(many.iter().map(|a| a.url.as_str()).collect())
            }
            _ => {
                return Err(PublishError::Failed(
                    "A Pinterest pin needs one MP4 or MOV video, or 1 to 5 JPEG or PNG images."
                        .into(),
                ))
            }
        };
        let mut alts = media.iter().map(|a| a.alt.trim()).filter(|a| !a.is_empty());
        let alt = alts.next();
        let notes = (alts.count() > 0)
            .then_some("Pinterest takes one alt text for a pin, so only the first was used.")
            .into_iter()
            .chain(
                alt.filter(|a| a.chars().count() > socialreins_core::post::PINTEREST_MAX_ALT)
                    .map(|_| "The alt text was cut to Pinterest's limit of 500 characters."),
            );
        for note in notes {
            let _ = crate::connectors::ALT_NOTES.try_with(|n| n.borrow_mut().push(note.to_owned()));
        }
        let alt: Option<String> = alt.map(|a| {
            a.chars()
                .take(socialreins_core::post::PINTEREST_MAX_ALT)
                .collect()
        });
        let (title, link) = pinterest::title_and_link(options);
        let pin = pinterest::Pin {
            board_id: board,
            title,
            description: body.trim(),
            link,
            alt: alt.as_deref(),
        };
        let id = self.post(secret, &pin, &source).await?;
        let url = pinterest::pin_url(&id);
        Ok(Remote {
            id,
            cid: String::new(),
            url,
        })
    }
}

#[async_trait]
impl Publisher for Pinterest {
    /// A post without Pinterest settings goes to the account's default board.
    async fn publish(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        _: Option<(&Remote, &Remote)>,
    ) -> Result<Remote, PublishError> {
        let options = pinterest::Options::default();
        self.pin(account, secret, body, media, &options).await
    }

    async fn publish_with_pinterest(
        &self,
        account: &social_accounts::Model,
        secret: &str,
        body: &str,
        media: &[Attachment],
        options: &pinterest::Options,
    ) -> Result<Remote, PublishError> {
        self.pin(account, secret, body, media, options).await
    }

    async fn delete(
        &self,
        _: &social_accounts::Model,
        secret: &str,
        remote_id: &str,
    ) -> Result<(), PublishError> {
        self.delete_pin(secret, remote_id).await
    }

    fn url(&self, _: &social_accounts::Model, remote_id: &str) -> Option<String> {
        pinterest::pin_url(remote_id)
    }

    fn name(&self) -> &'static str {
        "Pinterest"
    }

    fn reconnect(&self) -> &'static str {
        PINTEREST_RECONNECT
    }

    fn by_url(&self) -> bool {
        true
    }

    fn timeout(&self) -> Duration {
        pinterest::PUBLISH_TIMEOUT
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
        let held = cover.bytes.held()?;
        let file = (&held[..], cover.content_type.as_str());
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
        "pinterest" => Some(&app.pinterest),
        "facebook" => Some(&app.facebook),
        "instagram" => Some(&app.instagram),
        _ => None,
    }
}

/// Due posts claimed per tick; the rest wait for the next one.
const CLAIM_BATCH: i64 = 50;
/// Posts publishing at once, and targets publishing at once, across the process.
const POSTS_AT_ONCE: usize = 8;
static POST_SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(POSTS_AT_ONCE));
static TARGET_SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(8));
/// Posts, and targets, one team publishes at once in this process, taken before the shared
/// slots above: a team with many large videos waits for its own, and never holds all of those.
pub const TEAM_AT_ONCE: usize = 3;
type TeamSlots = std::sync::Mutex<HashMap<Uuid, Arc<Semaphore>>>;
static TEAM_POSTS: LazyLock<TeamSlots> = LazyLock::new(TeamSlots::default);
static TEAM_TARGETS: LazyLock<TeamSlots> = LazyLock::new(TeamSlots::default);

/// The team's semaphore in `slots`. shortcut: one small entry per team ever seen, never removed;
/// prune idle ones if a process ever serves very many teams.
fn team_slot(slots: &TeamSlots, team: Uuid) -> Arc<Semaphore> {
    let mut map = slots.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(team)
        .or_insert_with(|| Arc::new(Semaphore::new(TEAM_AT_ONCE)))
        .clone()
}

/// What a post fails with when its set is deleted before it goes out.
pub const SET_DELETED: &str =
    "Not posted: its account set was deleted before it was due. Restoring the set does not \
     schedule it again.";
/// How long a stopping process (`stop`) waits for the posts it is publishing before it exits
/// (`drain`). Keep it under the grace the platform gives between SIGTERM and the kill (on
/// Railway `RAILWAY_DEPLOYMENT_DRAINING_SECONDS`, which has to be set above this to matter: 180
/// on SocialReins). A Threads post, which waits for its media to process, can take a minute or two.
/// This is the default; `PUBLISH_DRAIN_SECONDS` (`set_drain`) changes it.
pub const DRAIN: Duration = Duration::from_secs(120);
static DRAIN_LIMIT: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();

/// Sets how long `drain` waits (`Config::publish_drain`), once, at start.
pub fn set_drain(limit: Duration) {
    let _ = DRAIN_LIMIT.set(limit);
}
static STOPPED: std::sync::OnceLock<tokio::time::Instant> = std::sync::OnceLock::new();

/// The process is about to exit (SIGTERM): no round claims a post any more (`begin`), and a post
/// still waiting for a slot is not started but handed back (`release`). Posts already
/// publishing go on.
pub fn stop() {
    let _ = STOPPED.set(tokio::time::Instant::now());
    POST_SLOTS.close();
}

/// `publish_post` calls in progress, a publish-now one that only waits for a slot included:
/// `drain` waits for them too, so a post that hands itself back does it before the process exits.
static RUNNING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct Running;

impl Running {
    fn begin() -> Self {
        RUNNING.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Running
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// After `stop`: waits until no post is publishing or handing itself back, at most `DRAIN`
/// from the stop. A post still publishing then is cut off as by a crash (`recover_stuck`).
pub async fn drain() {
    let until = STOPPED
        .get()
        .copied()
        .unwrap_or_else(tokio::time::Instant::now)
        + DRAIN_LIMIT.get().copied().unwrap_or(DRAIN);
    let idle = async {
        while POST_SLOTS.available_permits() < POSTS_AT_ONCE
            || in_flight(|flying| !flying.is_empty())
            || RUNNING.load(std::sync::atomic::Ordering::SeqCst) > 0
        {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };
    if tokio::time::timeout_at(until, idle).await.is_err() {
        tracing::warn!("stopped with posts still publishing");
    }
}
/// Posts a team publishes a day, scheduled or published now (`rate_limit::daily_reserve`).
pub const PUBLISHES_PER_DAY: i64 = 100;
const OVER_QUOTA: &str = "This team reached its limit of 100 published posts a day. It stays \
                          scheduled and goes out if the limit frees up within an hour of its \
                          time; after that it goes back to drafts.";

/// The daily publish bucket of a team.
pub fn quota_bucket(team_id: Uuid) -> String {
    format!("publish:team:{team_id}")
}

/// Longest one post (or thread part) may take to publish or delete, unless its publisher says
/// otherwise (`Publisher::timeout`).
const TARGET_TIMEOUT: Duration = Duration::from_secs(120);
/// Longest one target's publish may take, however large its video (`patience`).
pub const PUBLISH_LIMIT: Duration = Duration::from_secs(14 * 60);
/// What may pass around a post's downloads and its publish without it being cut off: its
/// credentials, cover and image conversions.
const STUCK_MARGIN: Duration = Duration::from_secs(5 * 60);
/// A target `publishing`, or a post `queued`, this long was cut off (crash, restart) and may
/// have posted: longer than a healthy post can take, its downloads (`media::INGEST_CEILING`)
/// then its publish (`PUBLISH_LIMIT`), plus `STUCK_MARGIN`. 31 minutes.
pub const STUCK_MINUTES: i64 =
    ((crate::media::INGEST_CEILING.as_secs() + PUBLISH_LIMIT.as_secs() + STUCK_MARGIN.as_secs())
        / 60) as i64;
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
pub const PINTEREST_RECONNECT: &str =
    "Pinterest refused the account's token, or access was removed. \
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
pub(crate) async fn credential(
    app: &AppState,
    account: &social_accounts::Model,
) -> Result<String, PublishError> {
    let got = read_credential(app, account).await;
    if matches!(got, Err(PublishError::Reconnect)) {
        expiry::flag_reconnect(&app.db, account.id).await;
    }
    got
}

/// What a failure says when the platform refused a permission, not the token.
fn denied(publisher: &dyn Publisher) -> String {
    format!(
        "{} refused a permission this needs. Reconnect the account and allow everything it asks for.",
        publisher.name()
    )
}

/// What a publisher says when the platform refused the account's token on the main call of a
/// publish, after flagging the account so the connections list asks for a reconnect. Side calls
/// (cover, first comment, take down) and refused permissions (`Denied`) never flag: they only
/// report their own error.
async fn refused(
    app: &AppState,
    publisher: &dyn Publisher,
    account: &social_accounts::Model,
) -> String {
    expiry::flag_reconnect(&app.db, account.id).await;
    publisher.reconnect().to_owned()
}

async fn read_credential(
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
    if account.provider == "pinterest" {
        return crate::controllers::pinterest::fresh_token(app, account).await;
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

/// Posts a round of this process started that have not finished. Claims stop at `CLAIM_BATCH`
/// of them; posts picked up as left behind come on top, up to `CLAIM_BATCH` a round.
static IN_FLIGHT: std::sync::Mutex<Option<HashSet<Uuid>>> = std::sync::Mutex::new(None);

fn in_flight<T>(with: impl FnOnce(&mut HashSet<Uuid>) -> T) -> T {
    let mut flying = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    with(flying.get_or_insert_with(HashSet::new))
}

/// A post in `IN_FLIGHT` until this is dropped, however its task ends.
struct Flight(Uuid);

/// Marks `id` as in flight until the result is dropped: for tests of a round that meets a post
/// whose task has not ended.
#[doc(hidden)]
pub fn hold(id: Uuid) -> impl Drop {
    in_flight(|flying| flying.insert(id));
    Flight(id)
}

impl Drop for Flight {
    fn drop(&mut self) {
        in_flight(|flying| flying.remove(&self.0));
    }
}

/// One publishing round: fail cut-off targets, claim due posts, pick up posts left `queued`,
/// and start each as its own task, which are returned and not waited for: the lock is let go
/// once the posts are claimed, so a slow post (a large video takes up to `STUCK_MINUTES`) never
/// keeps the next round from claiming the posts that came due after it. Skips while another
/// instance holds the lock.
///
/// A post still goes out once. Claiming is one `UPDATE` from `scheduled` to `queued` under the
/// lock, so one round takes a post; a round claims no more than `CLAIM_BATCH` less what this
/// process still has in flight, and never picks up as left behind a post it is itself running;
/// `POST_SLOTS` and `TARGET_SLOTS` bound what runs at once as before. Should another instance
/// take a post for left behind and run it too, each target is still claimed by one of them
/// (`claim_target`) and published by that one alone. A process that stops (`stop`) claims no
/// more, hands back the posts it has not started and waits `DRAIN` for the rest; what it still
/// abandons, or all of it when it is killed: the targets fail as cut off (`recover_stuck`) and
/// the rest is picked up as left behind.
pub async fn begin(app: &AppState) -> Result<Vec<tokio::task::JoinHandle<()>>, DbErr> {
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
    if !locked || POST_SLOTS.is_closed() {
        return Ok(Vec::new());
    }
    // One failing step is logged and the tick goes on; per-post work never aborts it.
    if let Err(e) = recover_stuck(&app.db).await {
        tracing::warn!(error = %e, "stuck target recovery failed");
    }
    let running: Vec<Uuid> = in_flight(|flying| flying.iter().copied().collect());
    if let Err(e) = draft_late(&app.db, &running).await {
        tracing::warn!(error = %e, "late posts not put back to draft");
    }
    // Posts left `queued` by a crash between claim and settle: fan-out and claims are
    // idempotent, so running them again only finishes the job. Also posts with a target whose
    // automatic retry is due.
    let stmt = sql(
        "SELECT id FROM posts WHERE status = 'queued' \
         AND (updated_at < now() - make_interval(mins => $1) OR EXISTS (SELECT 1 FROM \
           post_targets t WHERE t.post_id = posts.id AND t.status = 'pending' \
           AND t.retry_at <= now())) LIMIT $2",
        vec![(STUCK_MINUTES as i32).into(), CLAIM_BATCH.into()],
    );
    let left = ids(&app.db, stmt, "id").await?;
    // The quota is reserved per post in `publish_post`, together with marking it reserved.
    let mut flights: Vec<(Flight, Option<Uuid>)> = claim_flights(app, None)
        .await?
        .into_iter()
        .map(|(f, team)| (f, Some(team)))
        .collect();
    // Never picked up as left behind while this process still runs it.
    let left = left
        .into_iter()
        .filter(|id| in_flight(|flying| flying.insert(*id)));
    flights.extend(left.map(|id| (Flight(id), None)));
    // Mail about posts that did not go out, queued by earlier rounds (`post_mail`).
    let mail = {
        let app = app.clone();
        tokio::spawn(async move { crate::post_mail::send_due(&app).await })
    };
    let runs = std::iter::once(mail)
        .chain(
            flights
                .into_iter()
                .map(|(flight, team)| tokio::spawn(run(app.clone(), flight, team))),
        )
        .collect();
    lock.commit().await?;
    Ok(runs)
}

/// Claims for `claim` and marks each post in flight; one already in flight here (a task that has
/// not ended yet, claimed again) goes back to `scheduled` for the next round, never left `queued`
/// with nothing to run it. One claim at a time (`CLAIM_GATE`), with what is in flight read inside
/// it, so a team's places are counted once however many posts end together.
async fn claim_flights(app: &AppState, only: Option<Uuid>) -> Result<Vec<(Flight, Uuid)>, DbErr> {
    let _gate = CLAIM_GATE.lock().await;
    let running: Vec<Uuid> = in_flight(|flying| flying.iter().copied().collect());
    let room = (CLAIM_BATCH - running.len() as i64).max(0);
    let busy = busy_teams(&app.db, &running).await?;
    let mut flights = Vec::new();
    for (id, team) in claim(&app.db, room, app.billing_enabled, &busy, only).await? {
        if in_flight(|flying| flying.insert(id)) {
            flights.push((Flight(id), team));
        } else if let Err(e) = unclaim(&app.db, id, None).await {
            tracing::warn!(error = %e, post = %id, "returning a claimed post failed");
        }
    }
    Ok(flights)
}

/// Serialises `claim_flights` in this process.
static CLAIM_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Whether a post that `publish_post` returned from freed its team's place for the next one:
/// not after an error, and not for a post it put back to `scheduled` (the plan ended, the daily
/// cap, a quota error), which a refill would only claim again at once.
fn frees_a_place(published: bool, status: &str) -> bool {
    published && status != "scheduled"
}

/// Publishes a claimed post, then fills the place it frees for its team (`refill`), so a team's
/// throughput is its places and how long a post takes, never one claim a tick. Returns when the
/// posts it started in turn have ended too.
fn run(
    app: AppState,
    flight: Flight,
    team: Option<Uuid>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        let id = flight.0;
        let published = match publish_post(&app, id).await {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(error = %e, post = %id, "publish failed");
                false
            }
        };
        drop(flight);
        if let Some(team) = team {
            let status = posts::Entity::find_by_id(id)
                .select_only()
                .column(posts::Column::Status)
                .into_tuple::<String>()
                .one(&app.db)
                .await
                .ok()
                .flatten()
                .unwrap_or_default();
            if frees_a_place(published, &status) {
                refill(&app, team).await;
            }
        }
    })
}

/// Claims `team`'s next due posts, as many as it has places left in this process, and runs them.
/// A stopping process claims nothing.
async fn refill(app: &AppState, team: Uuid) {
    if POST_SLOTS.is_closed() {
        return;
    }
    let claimed = claim_flights(app, Some(team)).await;
    let runs: Vec<_> = match claimed {
        Ok(flights) => flights
            .into_iter()
            .map(|(flight, team)| tokio::spawn(run(app.clone(), flight, Some(team))))
            .collect(),
        Err(e) => return tracing::warn!(error = %e, "a team's next posts not claimed"),
    };
    for r in runs {
        let _ = r.await;
    }
}

/// `begin`, then waits for the posts it started: what tests and one-off runs want.
pub async fn tick(app: &AppState) -> Result<(), DbErr> {
    for run in begin(app).await? {
        let _ = run.await;
    }
    Ok(())
}

/// Claims up to 50 due `scheduled` posts, oldest first, `scheduled` to `queued`: only posts of
/// a live set, of a team under its daily publish cap (and with `billing`, on a live plan), and none `rejected`, nor `pending` while its
/// team requires approval. Returns `(post, team)`.
pub async fn claim_due(db: &DatabaseConnection, billing: bool) -> Result<Vec<(Uuid, Uuid)>, DbErr> {
    claim(db, CLAIM_BATCH, billing, &HashMap::new(), None).await
}

/// How many of `posts` each team has in flight in this process.
async fn busy_teams(db: &DatabaseConnection, posts: &[Uuid]) -> Result<HashMap<Uuid, i32>, DbErr> {
    let stmt = sql(
        "SELECT team_id, count(*)::int AS n FROM posts WHERE id = ANY($1::uuid[]) \
         GROUP BY team_id",
        vec![posts.to_vec().into()],
    );
    let mut busy = HashMap::new();
    for row in db.query_all_raw(stmt).await? {
        busy.insert(row.try_get("", "team_id")?, row.try_get("", "n")?);
    }
    Ok(busy)
}

/// `claim_due` of at most `limit` posts. A claimed post with a repeat rule yields its next copy
/// in the same transaction (`repeat::spawn`), so it does once. A team is claimed no more posts
/// than it has places left (`TEAM_AT_ONCE` less its posts in `busy`, in flight here), `only`
/// that team when set: a post that could only wait for its team's places stays `scheduled`, so
/// it counts nowhere against the batch, is not `queued` long enough to be called stuck, and no
/// other instance mistakes it for left behind. A team's backlog never keeps another team's post
/// from going out, and each post that ends claims the team's next one at once (`refill`), so the
/// tick is not the team's throughput. A post still `scheduled` an hour past its time is late
/// (`draft_late`) as any other.
async fn claim(
    db: &DatabaseConnection,
    limit: i64,
    billing: bool,
    busy: &HashMap<Uuid, i32>,
    only: Option<Uuid>,
) -> Result<Vec<(Uuid, Uuid)>, DbErr> {
    // A CTE runs once; a LIMIT inside `IN (...)` may be re-run and claim more. The posts of one
    // group (`group_id`, one per platform) are claimed together or not at all, so they are all
    // judged by the same clock (`LATE_MINUTES`): the batch is cut between groups, never inside
    // one, and the first group of a team and of the batch always fits.
    let stmt = sql(
        "WITH cand AS ( \
           SELECT d.id, d.team_id, COALESCE(d.group_id, d.id) AS gk, d.scheduled_at \
           FROM posts d \
           WHERE d.status = 'scheduled' AND d.scheduled_at <= now() \
           AND ($8::uuid IS NULL OR d.team_id = $8) \
           AND post_lateness(d.scheduled_at) <= make_interval(mins => $4) \
           AND d.approval IS DISTINCT FROM 'rejected' \
           AND (d.approval IS DISTINCT FROM 'pending' OR NOT EXISTS ( \
                SELECT 1 FROM teams t WHERE t.id = d.team_id AND t.require_approval)) \
           AND EXISTS (SELECT 1 FROM social_sets s \
                       WHERE s.id = d.social_set_id AND s.deleted_at IS NULL) \
           AND (NOT $3 OR EXISTS (SELECT 1 FROM subscriptions b WHERE b.team_id = d.team_id \
                AND b.status IN ('trialing', 'active', 'past_due'))) \
           AND (SELECT count(*) FROM rate_limit_hits h \
                WHERE h.bucket = 'publish:team:' || d.team_id::text \
                AND h.created_at > now() - interval '1 day') < $1), \
         grp AS (SELECT team_id, gk, min(scheduled_at) AS at, count(*) AS n \
                 FROM cand GROUP BY team_id, gk), \
         room AS ( \
           SELECT g.team_id, g.gk, g.at, g.n FROM (SELECT *, sum(n) OVER (PARTITION BY team_id \
                  ORDER BY at, gk) - n AS before FROM grp) g \
           LEFT JOIN unnest($5::uuid[], $6::int[]) AS b(team_id, n) ON b.team_id = g.team_id \
           WHERE g.before < $7 - COALESCE(b.n, 0)), \
         fit AS ( \
           SELECT gk FROM (SELECT gk, sum(n) OVER (ORDER BY at, gk) - n AS before \
                           FROM room) g WHERE before < $2), \
         due AS ( \
           SELECT p.id FROM posts p JOIN cand c ON c.id = p.id JOIN fit f ON f.gk = c.gk \
           ORDER BY c.scheduled_at, p.id FOR UPDATE OF p SKIP LOCKED) \
         UPDATE posts p SET status = 'queued', updated_at = now() FROM due \
         WHERE p.id = due.id AND p.status = 'scheduled' \
         RETURNING p.id, p.team_id",
        vec![
            PUBLISHES_PER_DAY.into(),
            limit.into(),
            billing.into(),
            (LATE_MINUTES as i32).into(),
            busy.keys().copied().collect::<Vec<_>>().into(),
            busy.values().copied().collect::<Vec<_>>().into(),
            (TEAM_AT_ONCE as i32).into(),
            only.into(),
        ],
    );
    let txn = db.begin().await?;
    let rows = txn.query_all_raw(stmt).await?;
    let claimed = rows
        .iter()
        .map(|r| Ok((r.try_get("", "id")?, r.try_get("", "team_id")?)))
        .collect::<Result<Vec<(Uuid, Uuid)>, DbErr>>()?;
    let ids: Vec<Uuid> = claimed.iter().map(|c| c.0).collect();
    crate::repeat::spawn(&txn, &ids, None).await?;
    txn.commit().await?;
    Ok(claimed)
}

/// A scheduled post this many minutes past its time is never published (decision 0008), not
/// counting the time brain itself was not running (`note_tick`, `post_lateness`).
pub const LATE_MINUTES: i64 = 60;

/// A publish round later than this after the last one means brain was down in between.
const MISSED_TICKS: &str = "2 minutes";
/// The longest stretch of downtime that is given back to the posts due in it.
const MAX_DOWNTIME: &str = "6 hours";

/// Records that the publish loop is running. When it last ran more than `MISSED_TICKS` ago,
/// that stretch (at most `MAX_DOWNTIME`, so a post is at most about that much later than its time and any longer outage still drafts what was due before it) is kept as brain's own downtime: `post_lateness`
/// takes it off a post's lateness, so a deploy that fails or a Postgres that is away for hours
/// does not draft every post due meanwhile. Call it before `begin`. Another instance doing the
/// same only keeps the stamp fresher.
// shortcut: only the last stretch is kept, so a second gap replaces the first; keep a list of stretches if that bites.
pub async fn note_tick(db: &DatabaseConnection) -> Result<(), DbErr> {
    if POST_SLOTS.is_closed() {
        return Ok(());
    }
    db.execute_unprepared(&format!(
        "INSERT INTO scheduler_ticks (name, at) VALUES ('publish', now()) \
         ON CONFLICT (name) DO UPDATE SET \
           down_from = CASE WHEN now() - scheduler_ticks.at > interval '{MISSED_TICKS}' \
             THEN GREATEST(scheduler_ticks.at, now() - interval '{MAX_DOWNTIME}') \
             ELSE scheduler_ticks.down_from END, \
           down_to = CASE WHEN now() - scheduler_ticks.at > interval '{MISSED_TICKS}' \
             THEN now() ELSE scheduler_ticks.down_to END, \
           at = now()"
    ))
    .await
    .map(|_| ())
}
/// Starts the `error` of a post `draft_late` put back. It names the due time in the set's queue
/// zone (UTC without one, or with a zone Postgres does not know), the zone the web shows the post in.
pub const LATE: &str = "Not posted: it was due at";

/// Puts every post more than `LATE_MINUTES` past its time back to `draft`, saying why: a
/// `scheduled` one (held up by approval, an ended plan, the daily cap or an outage), and one
/// left `queued` by a crash before it was fanned out into targets (`STUCK_MINUTES`). A post that
/// was fanned out is never drafted, whatever its age: it may be a retry (a manual retry keeps
/// the old `scheduled_at`, and a thread's pending target holds its live root), and `begin`
/// picks it up as left behind, as before. A `rejected` post is left alone: it is not going out and its series ends, as it
/// did before this. Posts in `running` (this process is publishing them, maybe waiting for a
/// slot) are left alone too. A post with a repeat rule yields its next copy here
/// (`repeat::spawn`), so a late post never ends its series. Returns the posts.
///
/// The posts are locked first, and only then are their targets looked at, in a new statement:
/// `claim_target` takes the same lock before it starts a target, so a target that started
/// before the lock is seen here, and one that comes after fails to claim a draft.
pub async fn draft_late(db: &DatabaseConnection, running: &[Uuid]) -> Result<Vec<Uuid>, DbErr> {
    let txn = db.begin().await?;
    let locked = ids(
        &txn,
        sql(
            "SELECT p.id FROM posts p \
             WHERE p.scheduled_at < now() - make_interval(mins => $1) \
             AND post_lateness(p.scheduled_at) > make_interval(mins => $1) \
             AND p.approval IS DISTINCT FROM 'rejected' AND p.id <> ALL($3::uuid[]) \
             AND (p.status = 'scheduled' OR (p.status = 'queued' AND NOT p.fanned_out \
               AND p.updated_at < now() - make_interval(mins => $2))) \
             ORDER BY p.id FOR UPDATE OF p",
            vec![
                (LATE_MINUTES as i32).into(),
                (STUCK_MINUTES as i32).into(),
                running.to_vec().into(),
            ],
        ),
        "id",
    )
    .await?;
    if locked.is_empty() {
        txn.commit().await?;
        return Ok(locked);
    }
    let stmt = sql(
        &format!(
            "UPDATE posts p SET status = 'draft', quota_reserved = false, fanned_out = false, \
             error = '{LATE} ' || to_char(p.scheduled_at AT TIME ZONE z.name, \
               'YYYY-MM-DD HH24:MI') || ' ' || z.name || ' and is more than an hour late. \
               Schedule it again.', \
             updated_at = now() \
             FROM (SELECT p2.id, COALESCE(n.name, 'UTC') AS name FROM posts p2 \
               LEFT JOIN queue_schedules q ON q.social_set_id = p2.social_set_id \
               LEFT JOIN pg_timezone_names n ON n.name = q.time_zone) z \
             WHERE z.id = p.id AND p.id = ANY($1::uuid[]) \
             AND (p.status = 'scheduled' OR (NOT p.fanned_out AND NOT EXISTS (SELECT 1 \
                 FROM post_targets t WHERE t.post_id = p.id AND t.status <> 'pending'))) \
             RETURNING p.id"
        ),
        vec![locked.into()],
    );
    let late = ids(&txn, stmt, "id").await?;
    if late.is_empty() {
        txn.commit().await?;
        return Ok(late);
    }
    post_targets::Entity::delete_many()
        .filter(post_targets::Column::PostId.is_in(late.iter().copied()))
        .filter(post_targets::Column::Status.eq("pending"))
        .exec(&txn)
        .await?;
    crate::repeat::spawn(&txn, &late, None).await?;
    for id in &late {
        crate::post_mail::queue(&txn, *id, crate::post_mail::Kind::Late).await?;
    }
    txn.commit().await?;
    tracing::info!(posts = late.len(), "late posts put back to draft");
    Ok(late)
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
/// The hit is rolled back when the update touches nothing. `repeats` is the post and the time it
/// was scheduled at, when the update takes it out of `scheduled`: its repeat rule then yields
/// the next copy here (`repeat::spawn`).
pub async fn reserve_and(
    db: &DatabaseConnection,
    team_id: Uuid,
    already: bool,
    update: Statement,
    repeats: Option<(Uuid, chrono::DateTime<chrono::FixedOffset>)>,
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
    if let Some((id, at)) = repeats {
        crate::repeat::spawn(&txn, &[id], Some(at)).await?;
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

/// Hands a post the stopping process claimed and never started back to `scheduled`, so the next
/// process claims it at once and not as left behind `STUCK_MINUTES` later. Only a post a round
/// claimed and nothing touched: one with its quota reserved (publish now, or underway) or any
/// target (a retry, or underway) stays `queued`.
async fn release(db: &DatabaseConnection, id: Uuid) -> Result<(), DbErr> {
    release_where(db, Some(id)).await.map(|_| ())
}

/// A post that waited for a slot as the process stopped and holds a hit of the daily cap
/// (publish now) goes back to `scheduled` with the hit given back, so the next process takes it
/// at its first round, not as left behind `STUCK_MINUTES` later. Nothing is touched once a target
/// exists. Its time is the moment publish now was pressed, well inside the late window.
pub async fn hand_back_reserved(db: &DatabaseConnection, id: Uuid) -> Result<(), DbErr> {
    let stmt = sql(
        "WITH back AS (UPDATE posts SET status = 'scheduled', quota_reserved = false, \
           updated_at = now() WHERE id = $1 AND status = 'queued' AND quota_reserved \
           AND NOT fanned_out \
           AND NOT EXISTS (SELECT 1 FROM post_targets t WHERE t.post_id = posts.id) \
           RETURNING team_id) \
         DELETE FROM rate_limit_hits WHERE id = (SELECT h.id FROM rate_limit_hits h, back \
           WHERE h.bucket = 'publish:team:' || back.team_id::text \
           ORDER BY h.created_at DESC LIMIT 1 FOR UPDATE OF h SKIP LOCKED)",
        vec![id.into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// On start: `release` for every post, so the posts a killed process had claimed and not started
/// go out at the next round, not `STUCK_MINUTES` later. Safe beside other instances: a post one
/// of them claimed is only put back while it has no quota reserved, and that instance's run then
/// fails to reserve it (`Quota::Lost`, the post is no longer `queued`) and leaves it to the
/// round that claims it again; one it already started is never touched. Returns how many.
pub async fn release_unstarted(db: &DatabaseConnection) -> Result<u64, DbErr> {
    release_where(db, None).await
}

async fn release_where(db: &DatabaseConnection, id: Option<Uuid>) -> Result<u64, DbErr> {
    let stmt = sql(
        "UPDATE posts SET status = 'scheduled', updated_at = now() \
         WHERE ($1::uuid IS NULL OR id = $1) AND status = 'queued' AND NOT quota_reserved \
         AND NOT fanned_out \
         AND NOT EXISTS (SELECT 1 FROM post_targets t WHERE t.post_id = posts.id)",
        vec![id.into()],
    );
    Ok(db.execute_raw(stmt).await?.rows_affected())
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
            let msg = cut_off_part(done + 1, total);
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
    fail_post_mail(db, id, msg, true).await
}

/// `fail_post`, mailing the failure only when `mail`: not for an outcome the user caused.
async fn fail_post_mail(
    db: &DatabaseConnection,
    id: Uuid,
    msg: &str,
    mail: bool,
) -> Result<(), DbErr> {
    let stmt = sql(
        "UPDATE posts SET status = 'failed', error = $2, updated_at = now() \
         WHERE id = $1 AND status = 'queued'",
        vec![id.into(), msg.into()],
    );
    let txn = db.begin().await?;
    if txn.execute_raw(stmt).await?.rows_affected() == 1 && mail {
        crate::post_mail::queue(&txn, id, crate::post_mail::Kind::Failed).await?;
    }
    txn.commit().await
}

const PLAN_ENDED: &str = "The team's plan has ended. The post goes out if a plan is chosen on the \
                          Billing page within an hour of its time; after that it goes back to \
                          drafts.";

/// Publishes a `queued` post: one target per account of its platform in its set, each
/// published at most once, then the post settles.
pub async fn publish_post(app: &AppState, id: Uuid) -> Result<(), DbErr> {
    let _running = Running::begin();
    let db = &app.db;
    // The team's own slot first, so posts waiting on it never hold a shared one.
    let team = posts::Entity::find_by_id(id)
        .select_only()
        .column(posts::Column::TeamId)
        .into_tuple::<Uuid>()
        .one(db)
        .await?;
    let Some(team) = team else {
        return Ok(());
    };
    let _team_slot = team_slot(&TEAM_POSTS, team).acquire_owned().await;
    let Ok(_slot) = POST_SLOTS.acquire().await else {
        release(db, id).await?;
        return hand_back_reserved(db, id).await;
    };
    let Some(post) = posts::Entity::find_by_id(id).one(db).await? else {
        return Ok(());
    };
    if post.status != "queued" {
        return Ok(());
    }
    // A team whose plan ended keeps its posts, scheduled, and the claim skips them until it pays.
    // A post already fanned out (a retry) never goes back to `scheduled`: its pending targets
    // fail with the reason and it settles as `published` or `failed` again.
    if app.billing_enabled && !crate::billing::has_plan(db, post.team_id).await? {
        if !post.fanned_out {
            return unclaim(db, id, Some(PLAN_ENDED)).await;
        }
        let stmt = sql(
            &with_event(
                "UPDATE post_targets SET status = 'failed', error = $2, updated_at = now() \
                 WHERE post_id = $1 AND status = 'pending'",
            ),
            vec![id.into(), PLAN_ENDED.into()],
        );
        db.execute_raw(stmt).await?;
        return settle(db, id).await;
    }
    // Whatever queued it (a tick, publish now, recovery), nothing publishes without its hit.
    if !post.quota_reserved {
        let mark = sql(
            "UPDATE posts SET quota_reserved = true \
             WHERE id = $1 AND status = 'queued' AND NOT quota_reserved",
            vec![id.into()],
        );
        let back = match reserve_and(db, post.team_id, false, mark, None).await {
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
        // The set was deleted after the claim: as the delete fails its scheduled posts, and
        // as that, with no mail: the user did it.
        return fail_post_mail(db, id, SET_DELETED, false).await;
    }
    if publisher(app, &post.platform).is_none() {
        let msg = format!("Publishing to {} is not available yet.", post.platform);
        return fail_post(db, id, &msg).await;
    }
    // Once only: a retry (or a restart picking the post up) works on the targets it has, and an
    // account added to the set since gets none.
    if !post.fanned_out {
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
        // Only the accounts the post was given, when it names them. One gone since (left the
        // set, or disconnected) gets a failed target saying so, never a silent skip.
        let chosen: Option<Vec<Uuid>> = post
            .account_ids
            .clone()
            .and_then(|v| serde_json::from_value(v).ok());
        let mut gone = Vec::new();
        let accounts = match chosen {
            None => accounts,
            Some(chosen) => {
                for c in chosen.iter().filter(|c| !accounts.contains(c)) {
                    gone.push(
                        match social_accounts::Entity::find_by_id(*c).one(db).await? {
                            Some(_) => (Some(*c), "Not sent: the account left this post's set."),
                            None => (None, "Not sent: the account was disconnected."),
                        },
                    );
                }
                accounts
                    .into_iter()
                    .filter(|a| chosen.contains(a))
                    .collect()
            }
        };
        // One transaction, so a crash before `fanned_out` leaves no target behind. A gone
        // account that was disconnected has no id to conflict on: count those already there.
        // Under a shared lock on the post, which must still be `queued`: `draft_late` takes the
        // same lock, so it never drafts a post whose targets are being made.
        let txn = db.begin().await?;
        let live = sql(
            "SELECT 1 AS n FROM posts WHERE id = $1 AND status = 'queued' FOR SHARE",
            vec![id.into()],
        );
        if txn.query_one_raw(live).await?.is_none() {
            return txn.commit().await;
        }
        let stmt = sql(
            "SELECT count(*) AS n FROM post_targets WHERE post_id = $1 AND social_account_id IS NULL",
            vec![id.into()],
        );
        let had: i64 = match txn.query_one_raw(stmt).await? {
            Some(r) => r.try_get("", "n")?,
            None => 0,
        };
        let mut nulls = 0;
        for (account, why) in gone {
            if account.is_none() {
                nulls += 1;
                if nulls <= had {
                    continue;
                }
            }
            let target = Uuid::now_v7();
            let stmt = sql(
                "INSERT INTO post_targets (id, post_id, social_account_id, status) \
                 VALUES ($1, $2, $3, 'pending') ON CONFLICT (post_id, social_account_id) DO NOTHING",
                vec![target.into(), id.into(), account.into()],
            );
            txn.execute_raw(stmt).await?;
            let stmt = sql(
                &with_event(
                    "UPDATE post_targets SET status = 'failed', error = $2, updated_at = now() \
                     WHERE id = $1",
                ),
                vec![target.into(), why.into()],
            );
            txn.execute_raw(stmt).await?;
        }
        for account in &accounts {
            let stmt = sql(
                "INSERT INTO post_targets (id, post_id, social_account_id, status) \
                 VALUES ($1, $2, $3, 'pending') ON CONFLICT (post_id, social_account_id) DO NOTHING",
                vec![Uuid::now_v7().into(), id.into(), (*account).into()],
            );
            txn.execute_raw(stmt).await?;
        }
        let mark = sql(
            "UPDATE posts SET fanned_out = true WHERE id = $1",
            vec![id.into()],
        );
        txn.execute_raw(mark).await?;
        txn.commit().await?;
    }
    let targets = post_targets::Entity::find()
        .filter(post_targets::Column::PostId.eq(id))
        .all(db)
        .await?;
    if targets.is_empty() {
        let msg = format!("No {} account is in this post's set.", post.platform);
        return fail_post(db, id, &msg).await;
    }
    // A target waiting for its automatic retry runs once that is due (`begin` picks it up).
    let now = chrono::Utc::now();
    let pending: Vec<Uuid> = targets
        .into_iter()
        .filter(|t| t.status == "pending" && t.retry_at.is_none_or(|r| r <= now))
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
    // The retry reads the same stored media; whatever is left goes once the post settles. A run
    // that lost the claim of every target (another run has them) never settles the post, so it
    // never releases the media the other is using.
    let done = posts::Entity::find_by_id(id)
        .one(db)
        .await?
        .is_some_and(|p| p.status != "queued");
    if done {
        app.media.release_post(db, id).await;
    }
    settled
}

/// Each media URL of a post and its thread, downloaded once into storage, or why not.
type Media = HashMap<String, Result<Stored, Fail>>;

async fn ingest(app: &AppState, post: &posts::Model) -> Media {
    // No cap of the post's own: `Media` holds a video to `max_bytes`, anything else to 100 MB.
    let cap = crate::media::MAX_BYTES;
    let thread: Vec<ThreadPart> = serde_json::from_value(post.thread.clone()).unwrap_or_default();
    let root: Vec<String> = serde_json::from_value(post.media.clone()).unwrap_or_default();
    let urls: Vec<String> = root
        .into_iter()
        .chain(thread.into_iter().flat_map(|p| p.media))
        .collect();
    let cover: Vec<String> = post.cover.clone().into_iter().collect();
    let cover_cap = cover_bytes(&post.platform);
    let (mut found, covers) = tokio::join!(
        app.media
            .ingest_all(&app.db, (post.id, post.group_id), &urls, cap),
        app.media
            .ingest_all(&app.db, (post.id, None), &cover, cover_cap),
    );
    // A cover that is also a media link keeps the media's result, and then fails its type check.
    for (url, got) in covers {
        found.entry(url).or_insert(got);
    }
    // Instagram (posts, carousel items, covers) takes JPEG images only, TikTok photos JPEG or
    // WebP: other images it can read become JPEGs. Other platforms keep the original file.
    let to_jpeg: &[&str] = match post.platform.as_str() {
        "instagram" => &["image/png", "image/webp"],
        "tiktok" => &["image/png"],
        "pinterest" => &["image/webp"],
        _ => &[],
    };
    for (url, got) in found.iter_mut() {
        if let Ok(file) = got {
            if to_jpeg.contains(&file.content_type.as_str()) {
                *file = app.media.as_jpeg(&app.db, post.id, url, file.clone()).await;
            }
        }
    }
    // A still image over the platform's limit for one is shrunk to fit; one within it keeps its
    // bytes. A GIF is never re-encoded, nor an animated WebP or PNG (`media::fit` leaves it),
    // and one that does not fit is refused by `media_problem`.
    if let Some((_, max, side)) = socialreins_core::post::image_limit(&post.platform) {
        for (url, got) in found.iter_mut() {
            if let Ok(file) = got {
                if STILL.contains(&file.content_type.as_str()) && file.size > max as i64 {
                    let limits = (max as usize, side);
                    *file = (app.media)
                        .fitted(&app.db, (post.id, url), file.clone(), limits)
                        .await;
                }
            }
        }
    }
    found
}

/// The image types `media::fit` shrinks.
const STILL: [&str; 3] = ["image/jpeg", "image/png", "image/webp"];

/// The largest cover image stored for `platform`, as its API documents: Instagram 8 MB, Facebook
/// 10 MB, YouTube 2 MB (the limit YouTube states for thumbnails).
fn cover_bytes(platform: &str) -> usize {
    socialreins_core::post::cover_limits(platform).1 as usize
}

/// What to tell the user about a cover that could not be stored.
fn cover_error(fail: Fail, platform: &str, name: &str) -> String {
    match fail {
        Fail::TooLarge => format!(
            "The cover image is over {} MB, the largest {name} takes.",
            cover_bytes(platform) / 1_000_000
        ),
        f => media_error(f, 1, platform, 0).replacen("Image 1", "The cover image", 1),
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

/// What to tell the user about image `n` that could not be stored.
fn media_error(fail: Fail, n: usize, platform: &str, cap: usize) -> String {
    match fail {
        Fail::Blocked => format!("Image {n} is not a public https URL."),
        Fail::NotShared => format!(
            "Image {n} is a Google Drive or Dropbox file that is not shared publicly. Share it \
             with anyone with the link."
        ),
        Fail::DriveQuota => format!(
            "Image {n} is a Google Drive file that too many people viewed or downloaded \
             recently, so Google Drive will not hand it out for now. Try again later, or use \
             a copy of the file."
        ),
        Fail::DriveConfirm => format!(
            "Image {n} is a Google Drive file too large for Drive to scan for viruses, and \
             SocialReins could not get past Drive's confirmation page. Try again, or use \
             another link."
        ),
        // A host or store that failed for a moment: nothing was posted, so the target tries once more.
        Fail::Http(code) => unsent(
            format!("Image {n} could not be downloaded (HTTP {code})."),
            matches!(code, 429 | 500..=599),
        ),
        Fail::Unreachable => unsent(format!("Image {n} could not be downloaded."), true),
        Fail::TooLarge => format!(
            "Image {n} is over {} MB, the largest video SocialReins takes.",
            cap / 1_000_000
        ),
        Fail::TooLargeStill => format!(
            "Image {n} is over {} MB, the largest image or document SocialReins takes.",
            crate::media::MAX_STILL_BYTES / 1_000_000
        ),
        Fail::Unsupported if platform == "linkedin" => {
            format!("Image {n} is not a JPEG, PNG or GIF image, an MP4 video or a PDF.")
        }
        Fail::Unsupported => format!("Image {n} is not a JPEG, PNG, WebP or GIF."),
        Fail::Storage => unsent(format!("Image {n} could not be stored. Try again."), true),
        Fail::TimedOut => format!(
            "Image {n} did not download in time: it came slower than 2 MB a second, or the \
             post's media took over 12 minutes together. Use a faster link."
        ),
        Fail::Stalled => unsent(
            format!(
                "Image {n} stopped downloading: its server sent nothing for 30 seconds. Try \
                 again, or use another link."
            ),
            true,
        ),
    }
}

/// The first media problem of the post or its thread, so nothing goes out with one.
fn media_problem(
    media: &Media,
    root: &[String],
    thread: &[ThreadPart],
    (platform, cap): (&str, usize),
) -> Option<String> {
    let first = |urls: &[String]| {
        let video = |u: &String| matches!(media.get(u), Some(Ok(s)) if s.content_type.starts_with("video/"));
        if platform == "bluesky" && urls.len() > 1 && urls.iter().any(video) {
            return Some(bluesky::MIXED_MEDIA.to_owned());
        }
        urls.iter()
            .enumerate()
            .find_map(|(i, u)| match media.get(u) {
                Some(Err(f)) => Some(media_error(*f, i + 1, platform, cap)),
                // A video over the platform's own limit, where that is under brain's.
                Some(Ok(s)) if s.content_type.starts_with("video/") => {
                    let (name, max) = socialreins_core::post::video_limit(platform)?;
                    (s.size > max as i64).then(|| {
                        format!(
                            "Video {} is over {} MB, {name}'s limit for a video.",
                            i + 1,
                            max / 1_000_000
                        )
                    })
                }
                // Only known once stored, and `ingest` shrinks what it can: this is an image
                // that could not be (a GIF or other animation, one that does not decode, one
                // over 25 megapixels), or a file that is no image (a PDF).
                Some(Ok(s)) => {
                    let (name, max, _) = socialreins_core::post::image_limit(platform)?;
                    let gif = s.content_type == "image/gif";
                    // X takes a larger GIF, and its connector checks that.
                    if s.size <= max as i64 || (gif && platform == "x") {
                        return None;
                    }
                    let (n, mb) = (i + 1, max / 1_000_000);
                    Some(if gif {
                        format!("Image {n} is a GIF over {mb} MB, {name}'s limit.")
                    } else if s.content_type.starts_with("image/") {
                        format!(
                            "Image {n} is over {mb} MB, {name}'s limit, and could not be made \
                             smaller."
                        )
                    } else {
                        format!("File {n} is over {mb} MB, {name}'s limit.")
                    })
                }
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

/// Each URL's stored file: its type and brain link, and its bytes, read into memory unless it
/// is a video or the platform fetches it `by_url`.
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
            return Err(media_error(Fail::Storage, n, platform, 0));
        };
        let bytes = if by_url || s.content_type.starts_with("video/") {
            app.media.blob(s)
        } else {
            match app.media.read(&s.key).await {
                Ok(bytes) => crate::media::Blob::Held(bytes.into()),
                Err(e) => {
                    tracing::warn!(error = %e, "stored media read failed");
                    return Err(media_error(Fail::Storage, n, platform, 0));
                }
            }
        };
        files.push(Attachment {
            bytes,
            content_type: s.content_type.clone(),
            url: crate::media::link(app, s),
            alt: String::new(),
        });
    }
    Ok(files)
}

/// Saves what the platform answered for a post that is out, trying again after a short wait: a
/// pool timeout or a blink of the database must not leave a live post with no id. If every try
/// fails the remote id is logged for a person (`error` level) and the target fails with `lost`,
/// which says it may be live, so it is never sent again; with the database still down, recovery
/// fails it later with a like message.
async fn save_sent(
    db: &DatabaseConnection,
    id: Uuid,
    (stmt, remote_id): (Statement, &str),
    lost: &str,
) -> Result<(), DbErr> {
    let mut last = None;
    for wait in SAVE_WAITS {
        tokio::time::sleep(wait).await;
        match db.execute_raw(stmt.clone()).await {
            Ok(_) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    let e = last.expect("SAVE_WAITS is not empty");
    tracing::error!(error = %e, target = %id, remote_id, "post is live but its id was not saved");
    let _ = fail_target(db, id, lost).await;
    Err(e)
}

/// Saves thread part number `at` (0 is the first after the root): only while `parts` holds
/// exactly the `at` parts before it, so a try repeated after the first one committed (its answer
/// lost) changes nothing instead of saving the part twice.
pub fn part_saved(
    id: Uuid,
    part: serde_json::Value,
    alt_note: Option<String>,
    at: usize,
) -> Statement {
    sql(
        "UPDATE post_targets SET parts = parts || $2, alt_error = CASE \
         WHEN $3::text IS NULL THEN alt_error WHEN alt_error IS NULL THEN $3 \
         ELSE alt_error || ' ' || $3 END, updated_at = now() \
         WHERE id = $1 AND jsonb_array_length(parts) = $4",
        vec![id.into(), part.into(), alt_note.into(), (at as i32).into()],
    )
}

/// Waits before each try of `save_sent`.
const SAVE_WAITS: [Duration; 4] = [
    Duration::ZERO,
    Duration::from_millis(200),
    Duration::from_secs(1),
    Duration::from_secs(3),
];

/// Credits held for the X posts of one target, until it is known how many went out.
struct XHold {
    reference: String,
    amount: i64,
    /// Posts the platform took in this attempt, counted as each `send` returns, before anything
    /// is saved: a post that is out but not recorded is still paid for.
    sent: i64,
}

impl XHold {
    /// A call that failed with an unknown outcome (a timeout, a cut-off: `outcome_unknown`, or a
    /// dropped connection) may have been taken and billed by X, so it stays paid for. A 5xx
    /// answer is not told apart from a refusal and is still given back.
    fn failed(&mut self, msg: &str) {
        // A dropped connection that was not certainly never opened may have carried the post.
        let dropped =
            msg.contains("could not be reached") && !msg.contains(crate::connectors::UNSENT);
        if outcome_unknown(Some(msg)) || dropped {
            self.sent += 1;
        }
    }
}

/// Gives back the credits held for X posts that did not go out. A failure here is logged; the
/// team is then charged for posts that never went out, which is the safe side for us.
async fn release_x(app: &AppState, post: &posts::Model, id: Uuid, hold: XHold) {
    let back = hold.amount - hold.sent;
    if back <= 0 {
        return;
    }
    let reference = Some(hold.reference.as_str());
    let kind = billing::Kind::X;
    if let Err(e) = billing::grant(
        &app.db,
        post.team_id,
        kind,
        back,
        "x post unsent",
        reference,
    )
    .await
    {
        tracing::error!(error = %e, %id, back, "x credits not released");
    }
}

/// How long a target waits for its one automatic retry.
const RETRY_AFTER_MINUTES: i32 = 5;

/// Whether a failure certainly sent nothing (`connectors::UNSENT`: a rate limit, or a connection
/// that never opened), so the target may go out once more by itself.
pub fn retry_safe(error: &str) -> bool {
    error.contains(crate::connectors::UNSENT) && !outcome_unknown(Some(error))
}

/// Fails a target being published, saying why; or, after an error that sent nothing
/// (`retry_safe`), puts it back to `pending` for one retry `RETRY_AFTER_MINUTES` on, while it has
/// no remote id and was not retried yet. A waiting target keeps its post `queued`; `begin` picks
/// the post up once the retry is due.
async fn fail_target(db: &DatabaseConnection, id: Uuid, msg: &str) -> Result<(), DbErr> {
    if retry_safe(msg) {
        let again = sql(
            "UPDATE post_targets SET status = 'pending', error = $2, retried = true, \
             retry_at = now() + make_interval(mins => $3), updated_at = now() \
             WHERE id = $1 AND status = 'publishing' AND remote_id IS NULL AND NOT retried",
            vec![id.into(), msg.into(), RETRY_AFTER_MINUTES.into()],
        );
        if db.execute_raw(again).await?.rows_affected() == 1 {
            tracing::info!(target = %id, "target retries in {RETRY_AFTER_MINUTES} minutes");
            return Ok(());
        }
    }
    let stmt = sql(
        &with_event(
            "UPDATE post_targets SET status = 'failed', error = $2, updated_at = now() \
             WHERE id = $1 AND status = 'publishing'",
        ),
        vec![id.into(), msg.into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// A pending target that is up but whose thread is not all out yet.
const UNFINISHED: &str = "jsonb_array_length(t.parts) < \
    (SELECT jsonb_array_length(p.thread) FROM posts p WHERE p.id = t.post_id)";

const TIMED_OUT: &str = "Timed out; it may have posted, check the account.";

/// What a thread part that went out but could not be saved fails its target with.
fn unsaved_part(n: usize, total: usize) -> String {
    format!(
        "Thread post {n} of {total} went out but could not be saved: it may be live, check \
         the account."
    )
}

/// What a target cut off midway through a thread fails with.
fn cut_off_part(n: i32, total: i32) -> String {
    format!(
        "Unknown, check the account: publishing stopped midway and thread post {n} of \
         {total} may have posted."
    )
}

/// Whether a failure may already have posted: a timeout, a run cut off, a platform that never
/// said it finished. Resending could post twice, so a retry leaves these to the caller, who
/// checks the platform. Every such message holds one of these phrases (`MAY_HAVE_POSTED`).
pub fn outcome_unknown(error: Option<&str>) -> bool {
    error.is_some_and(|e| MAY_HAVE_POSTED.iter().any(|p| e.contains(p)))
}

pub const MAY_HAVE_POSTED: [&str; 3] = ["may have posted", "may still post", "may be live"];

/// Whether a failed `target` of `post` could be sent again: it failed before anything went out,
/// or midway through a thread (the parts saved stay up and the rest follow them). A failed
/// target whose thread is all out is done.
pub fn resendable(target: &post_targets::Model, post: &posts::Model) -> bool {
    let len = |v: &serde_json::Value| v.as_array().map_or(0, Vec::len);
    target.status == "failed"
        && (target.remote_id.is_none() || len(&target.parts) < len(&post.thread))
}

/// Whether a retry re-queues `target`: `resendable`, and not an `outcome_unknown` failure.
pub fn retryable(target: &post_targets::Model, post: &posts::Model) -> bool {
    resendable(target, post) && !outcome_unknown(target.error.as_deref())
}

/// Claims a target (`pending`, to `publishing`); false when another run has it. A target with a
/// remote id is claimed only to finish its thread (a retry), never to send its root again.
pub async fn claim_target(db: &DatabaseConnection, id: Uuid) -> Result<bool, DbErr> {
    // The post is locked (shared) first and must still be `queued`: `draft_late` takes the same
    // lock, so a post never turns draft while one of its targets starts. The CTE runs before
    // the target row is touched, so the lock order is always post, then target.
    let stmt = sql(
        &format!(
            "WITH live AS (SELECT p.id FROM posts p \
               WHERE p.id = (SELECT post_id FROM post_targets WHERE id = $1) \
               AND p.status = 'queued' FOR SHARE OF p) \
             UPDATE post_targets t SET status = 'publishing', updated_at = now() FROM live \
             WHERE t.id = $1 AND live.id = t.post_id \
             AND t.status = 'pending' AND (t.remote_id IS NULL OR {UNFINISHED}) \
             AND (t.retry_at IS NULL OR t.retry_at <= now())"
        ),
        vec![id.into()],
    );
    Ok(db.execute_raw(stmt).await?.rows_affected() == 1)
}

/// What `send` posts: the body, the media links with their alt texts, and the cover, TikTok,
/// YouTube and Pinterest settings.
type Content<'a> = (
    &'a str,
    (&'a [String], &'a [String]),
    Option<&'a Attachment>,
    Option<&'a tiktok::Options>,
    Option<&'a youtube::Options>,
    Option<&'a pinterest::Options>,
);

/// One post (or thread part), its media read from storage, with its time limit; `Err` holds what
/// to tell the user.
async fn send(
    (app, stored): (&AppState, &Media),
    publisher: &dyn Publisher,
    account: &social_accounts::Model,
    secret: &str,
    (body, (media, alt), cover, tiktok, youtube, pinterest): Content<'_>,
    reply: Option<(&Remote, &Remote)>,
) -> Result<(Remote, Option<String>), String> {
    let how = (publisher.by_url(), account.provider.as_str());
    let mut media = load(app, stored, media, how).await?;
    for (file, text) in media.iter_mut().zip(alt) {
        file.alt.clone_from(text);
    }
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
        match (cover, tiktok, youtube, pinterest) {
            (Some(c), _, _, _) => {
                publisher
                    .publish_with_cover(account, secret, body, &media, c)
                    .await
            }
            (None, Some(t), _, _) => {
                publisher
                    .publish_with_tiktok(account, secret, body, &media, t)
                    .await
            }
            (None, None, Some(y), _) => {
                publisher
                    .publish_with_youtube(account, secret, body, &media, y)
                    .await
            }
            (None, None, None, Some(p)) => {
                publisher
                    .publish_with_pinterest(account, secret, body, &media, p)
                    .await
            }
            (None, None, None, None) => {
                publisher
                    .publish(account, secret, body, &media, reply)
                    .await
            }
        }
    };
    let (limit, extra) = patience(publisher.timeout(), &media);
    let sent = crate::connectors::PATIENCE.scope(extra, sent);
    // What the alt text steps could not do (never a reason to fail the post).
    let notes = std::cell::RefCell::new(Vec::new());
    let sent = crate::connectors::ALT_NOTES.scope(notes, async {
        let made = sent.await;
        let notes = crate::connectors::ALT_NOTES.with(|n| n.take());
        (made, notes)
    });
    // YouTube's token lasts an hour and a large upload many minutes: `upload` may renew it.
    let refresh = (account.provider == "youtube")
        .then(|| crate::controllers::youtube::unseal(app, account))
        .flatten();
    let sent = youtube::REFRESH.scope(refresh, sent);
    match tokio::time::timeout(limit, sent).await {
        Ok((Ok(remote), notes)) => Ok((remote, (!notes.is_empty()).then(|| notes.join(" ")))),
        Ok((Err(PublishError::Reconnect), _)) => Err(refused(app, publisher, account).await),
        Ok((Err(PublishError::Denied), _)) => Err(denied(publisher)),
        Ok((Err(PublishError::Failed(msg)), _)) => Err(msg),
        Err(_) => Err(TIMED_OUT.to_owned()),
    }
}

/// How long a publish of `media` may take, and the processing checks its platform's poll adds
/// (`connectors::extra_polls`). Nothing changes for a post without a large video: `base` and
/// none. Videos add the time their bytes take at `media::MIN_SPEED` (brain's upload, or the
/// platform's download from brain) and one check, about 50 seconds, per 100 MB, at most 10. The
/// limit is `PUBLISH_LIMIT` at most, which `STUCK_MINUTES` is derived from, so a slow publish
/// is never taken for one cut off.
fn patience(base: Duration, media: &[Attachment]) -> (Duration, u32) {
    let videos = media
        .iter()
        .filter(|a| a.content_type.starts_with("video/"));
    let bytes: u64 = videos.map(|a| a.bytes.len()).sum();
    let extra = (bytes / 100_000_000).min(10) as u32;
    let limit = base + Duration::from_secs(bytes / crate::media::MIN_SPEED + 50 * u64::from(extra));
    (limit.min(PUBLISH_LIMIT), extra)
}

async fn publish_target(
    app: &AppState,
    post: &posts::Model,
    id: Uuid,
    stored: &Media,
) -> Result<(), DbErr> {
    let mut hold = None;
    let done = publish_target_held(app, post, id, stored, &mut hold).await;
    if let Some(hold) = hold {
        release_x(app, post, id, hold).await;
    }
    done
}

async fn publish_target_held(
    app: &AppState,
    post: &posts::Model,
    id: Uuid,
    stored: &Media,
    hold: &mut Option<XHold>,
) -> Result<(), DbErr> {
    let _team_slot = team_slot(&TEAM_TARGETS, post.team_id).acquire_owned().await;
    let Ok(_slot) = TARGET_SLOTS.acquire().await else {
        return Ok(());
    };
    let db = &app.db;
    if !claim_target(db, id).await? {
        return Ok(());
    }
    // A database error before the first call to the platform sent nothing: give the target the
    // one retry of an unsent failure (`fail_target`) instead of leaving it `publishing` for
    // `recover_stuck` to call "may have posted" 31 minutes on.
    let mut sending = false;
    match publish_claimed(app, post, id, stored, hold, &mut sending).await {
        Err(e) if !sending => {
            tracing::warn!(error = %e, target = %id, "database error before anything was sent");
            let msg = unsent("Publishing hit a database error.".into(), true);
            let _ = fail_target(db, id, &msg).await;
            Err(e)
        }
        done => done,
    }
}

/// `publish_target_held` once the target is claimed. `sending` is set true right before the
/// first call to the platform, so the caller knows an error came before anything was sent.
async fn publish_claimed(
    app: &AppState,
    post: &posts::Model,
    id: Uuid,
    stored: &Media,
    hold: &mut Option<XHold>,
    sending: &mut bool,
) -> Result<(), DbErr> {
    let db = &app.db;
    let target = post_targets::Entity::find_by_id(id).one(db).await?;
    let account = match target.as_ref().and_then(|t| t.social_account_id) {
        Some(a) => social_accounts::Entity::find_by_id(a).one(db).await?,
        None => None,
    };
    let Some(account) = account else {
        return fail_target(db, id, "The account was disconnected.").await;
    };
    let Some(publisher) = publisher(app, &post.platform) else {
        return fail_target(db, id, "This platform cannot publish yet.").await;
    };
    // An account already known to need a reconnect fails at once, without a call.
    if expiry::status(&account, chrono::Utc::now()) == expiry::Status::Reconnect {
        return fail_target(db, id, publisher.reconnect()).await;
    }
    let secret = match credential(app, &account).await {
        Ok(secret) => secret,
        Err(PublishError::Reconnect | PublishError::Denied) => {
            return fail_target(db, id, publisher.reconnect()).await
        }
        Err(PublishError::Failed(msg)) => return fail_target(db, id, &msg).await,
    };
    let media: Vec<String> = serde_json::from_value(post.media.clone()).unwrap_or_default();
    let alt: Vec<String> = post
        .media_alt
        .clone()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let thread: Vec<ThreadPart> = serde_json::from_value(post.thread.clone()).unwrap_or_default();
    let limits = (post.platform.as_str(), app.media.max_bytes);
    if let Some(msg) = media_problem(stored, &media, &thread, limits) {
        return fail_target(db, id, &msg).await;
    }
    let cover = match load_cover(app, publisher, post, (stored, &media)).await {
        Ok(cover) => cover,
        Err(msg) => return fail_target(db, id, &msg).await,
    };
    // The user chose these settings, so unreadable ones fail the target rather than post with defaults.
    let tiktok = match post
        .tiktok
        .clone()
        .map(serde_json::from_value::<tiktok::Options>)
    {
        Some(Err(_)) => {
            let msg = "The TikTok settings of this post could not be read. Open the post, choose them again and save it.";
            return fail_target(db, id, msg).await;
        }
        Some(Ok(options)) => Some(options),
        None => None,
    };
    let youtube = match post
        .youtube
        .clone()
        .map(serde_json::from_value::<youtube::Options>)
    {
        Some(Err(_)) => {
            let msg = "The YouTube settings of this post could not be read. Open the post, choose them again and save it.";
            return fail_target(db, id, msg).await;
        }
        Some(Ok(options)) => Some(options),
        None => None,
    };
    let pinterest = match post
        .pinterest
        .clone()
        .map(serde_json::from_value::<pinterest::Options>)
    {
        Some(Err(_)) => {
            let msg = "The Pinterest settings of this post could not be read. Open the post, choose them again and save it.";
            return fail_target(db, id, msg).await;
        }
        Some(Ok(options)) => Some(options),
        None => None,
    };
    let with = (app, stored);
    let in_call = cover.as_ref().filter(|_| publisher.cover_in_publish());
    // A retried thread goes on from the parts already out: its root and saved parts stay.
    let saved: Vec<serde_json::Value> = target
        .as_ref()
        .and_then(|t| t.parts.as_array().cloned())
        .unwrap_or_default();
    let resumed = target.as_ref().and_then(|t| {
        Some(Remote {
            id: t.remote_id.clone()?,
            cid: t.remote_cid.clone().unwrap_or_default(),
            url: t.remote_url.clone(),
        })
    });
    // On Cloud an X post costs the team one credit each, the root and every thread part. They
    // are taken here, before anything is sent, so posts due together cannot spend the same
    // credit; `publish_target` gives back what did not go out. A crash in between keeps them.
    if post.platform == "x" && app.billing_enabled {
        let todo = i64::from(resumed.is_none()) + thread.len().saturating_sub(saved.len()) as i64;
        if todo > 0 {
            let reference = format!("{id}:{}", Uuid::now_v7());
            let kind = billing::Kind::X;
            match billing::spend(db, post.team_id, kind, todo, "x post", Some(&reference)).await? {
                billing::Spent::Taken | billing::Spent::Repeated => {
                    *hold = Some(XHold {
                        reference,
                        amount: todo,
                        sent: 0,
                    });
                }
                billing::Spent::Short(have) => {
                    let msg = format!(
                        "Posting to X needs {todo} credit{} and the team has {have}. Add X credits to post.",
                        if todo == 1 { "" } else { "s" }
                    );
                    return fail_target(db, id, &msg).await;
                }
            }
        }
    }
    *sending = true;
    let root = match resumed {
        Some(root) => root,
        None => {
            let root = match send(
                with,
                publisher,
                &account,
                &secret,
                (
                    &post.body,
                    (&media, &alt),
                    in_call,
                    tiktok.as_ref(),
                    youtube.as_ref(),
                    pinterest.as_ref(),
                ),
                None,
            )
            .await
            {
                Ok((remote, note)) => {
                    if let Some(h) = hold.as_mut() {
                        h.sent += 1;
                    }
                    (remote, note)
                }
                Err(msg) => {
                    if let Some(h) = hold.as_mut() {
                        h.failed(&msg);
                    }
                    return fail_target(db, id, &msg).await;
                }
            };
            let (root, alt_note) = root;
            // The remote id first: whatever fails after this, the post is known to be live.
            let stmt = sql(
                "UPDATE post_targets SET remote_id = $2, remote_cid = $3, remote_url = $4, \
                 alt_error = $5, published_at = now(), updated_at = now() WHERE id = $1",
                vec![
                    id.into(),
                    root.id.clone().into(),
                    root.cid.clone().into(),
                    root.url
                        .clone()
                        .or_else(|| publisher.url(&account, &root.id))
                        .into(),
                    alt_note.into(),
                ],
            );
            let lost = "It went out but could not be saved: it may be live, check the account.";
            save_sent(db, id, (stmt, &root.id), lost).await?;
            root
        }
    };
    // Each part right after the platform takes it, so its parent is always saved before it.
    let mut parent = saved.last().map_or_else(
        || root.clone(),
        |p| Remote {
            id: p["remote_id"].as_str().unwrap_or_default().to_owned(),
            cid: p["cid"].as_str().unwrap_or_default().to_owned(),
            url: p["remote_url"].as_str().map(str::to_owned),
        },
    );
    for (i, part) in thread.iter().enumerate().skip(saved.len()) {
        // A thread can outlast the token it started with: fetch the credential again before
        // each part (renewed when near its end, through the locked path).
        let secret = match credential(app, &account).await {
            Ok(secret) => secret,
            Err(e) => {
                let why = match e {
                    PublishError::Reconnect => publisher.reconnect().to_owned(),
                    PublishError::Denied => denied(publisher),
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
        let content = (
            part.body.as_str(),
            (part.media.as_slice(), part.media_alt.as_slice()),
            None,
            None,
            None,
            None,
        );
        let (made, alt_note) = match send(
            with,
            publisher,
            &account,
            &secret,
            content,
            Some((&parent, &root)),
        )
        .await
        {
            Ok((remote, note)) => {
                if let Some(h) = hold.as_mut() {
                    h.sent += 1;
                }
                (remote, note.map(|n| format!("Thread post {}: {n}", i + 1)))
            }
            Err(msg) => {
                let msg = format!(
                    "Thread post {} of {}: {msg} The posts before it are up.",
                    i + 1,
                    thread.len()
                );
                if let Some(h) = hold.as_mut() {
                    h.failed(&msg);
                }
                return fail_target(db, id, &msg).await;
            }
        };
        let saved = serde_json::json!([{
            "remote_id": made.id, "cid": made.cid,
            "remote_url": made.url.clone().or_else(|| publisher.url(&account, &made.id)),
            "published_at": chrono::Utc::now().fixed_offset(),
        }]);
        let stmt = part_saved(id, saved, alt_note, i);
        save_sent(db, id, (stmt, &made.id), &unsaved_part(i + 1, thread.len())).await?;
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
        Ok(Err(PublishError::Denied)) => denied(publisher),
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
        Ok(Err(PublishError::Denied)) => Err(denied(publisher)),
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
         published_at = CASE WHEN $2 = 'published' THEN COALESCE(published_at, now()) \
           ELSE published_at END \
         WHERE id = $1 AND status = 'queued'",
        vec![id.into(), status.into(), error.into()],
    );
    txn.execute_raw(stmt).await?;
    if !failed.is_empty() {
        let kind = match status {
            "failed" => crate::post_mail::Kind::Failed,
            _ => crate::post_mail::Kind::Partial,
        };
        crate::post_mail::queue(&txn, id, kind).await?;
    }
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
            Err(PublishError::Reconnect | PublishError::Denied) => None,
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
        Err(PublishError::Reconnect | PublishError::Denied) => {
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
        Ok(Err(PublishError::Reconnect | PublishError::Denied)) => Some(format!(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_post_that_ran_frees_its_place_for_the_next() {
        assert!(frees_a_place(true, "published"));
        assert!(frees_a_place(true, "failed"));
        assert!(
            frees_a_place(true, "queued"),
            "a retry waiting five minutes"
        );
        // Put back to scheduled (plan ended, daily cap), or an error: a refill would claim the
        // same post again at once, round and round until it is an hour late.
        assert!(!frees_a_place(true, "scheduled"));
        assert!(!frees_a_place(false, "queued"));
    }

    #[test]
    fn a_large_video_gets_more_time_and_a_small_file_none() {
        let file = |content_type: &str, size: u64| Attachment {
            bytes: crate::media::Blob::Stored {
                store: Arc::new(object_store::memory::InMemory::new()),
                key: "posts/p/f".into(),
                size,
            },
            content_type: content_type.into(),
            url: String::new(),
            alt: String::new(),
        };
        let base = Duration::from_secs(420);
        let limit = |files: &[Attachment]| patience(base, files);
        assert_eq!(limit(&[]), (base, 0));
        assert_eq!(limit(&[file("image/jpeg", 90_000_000)]), (base, 0));
        // 20 MB: ten seconds for the bytes, no more checks.
        let small = limit(&[file("video/mp4", 20_000_000)]);
        assert_eq!(small, (base + Duration::from_secs(10), 0));
        // 300 MB: 150 s for the bytes and three checks of 50 s.
        let large = limit(&[file("video/mp4", 300_000_000)]);
        assert_eq!(large, (base + Duration::from_secs(300), 3));
        // 1 GiB: bounded under the time a target counts as cut off.
        let huge = limit(&[file("video/quicktime", 1 << 30)]);
        assert_eq!(huge, (Duration::from_secs(14 * 60), 10));
    }

    #[test]
    fn every_failure_that_may_have_posted_is_left_to_the_caller() {
        let maybe = [
            TIMED_OUT.to_owned(),
            format!("Thread post 2 of 3: {TIMED_OUT} The posts before it are up."),
            STUCK.to_owned(),
            cut_off_part(2, 3),
            unsaved_part(2, 3),
            tiktok::NOT_DONE.to_owned(),
            youtube::NO_VIDEO_ID.to_owned(),
            youtube::UNCONFIRMED.to_owned(),
        ];
        for m in &maybe {
            assert!(outcome_unknown(Some(m)), "{m}");
        }
        for sure in [
            "Bluesky refused it (InvalidRequest).",
            "Thread post 2 of 3: Bluesky refused it (InvalidRequest). The posts before it are up.",
            "The account was disconnected.",
            "TikTok could not publish it (spam_risk).",
        ] {
            assert!(!outcome_unknown(Some(sure)), "{sure}");
        }
        assert!(!outcome_unknown(None));
    }

    #[test]
    fn a_file_over_the_image_limit_is_named_for_what_it_is() {
        let url = "https://a.example/1".to_owned();
        let problem = |content_type: &str, platform: &str| {
            let file = Stored {
                key: "k".into(),
                content_type: content_type.into(),
                size: 6_000_000,
            };
            let media = Media::from([(url.clone(), Ok(file))]);
            media_problem(&media, std::slice::from_ref(&url), &[], (platform, 0))
        };
        assert_eq!(
            problem("image/webp", "bluesky").as_deref(),
            Some("Image 1 is over 1 MB, Bluesky's limit, and could not be made smaller.")
        );
        assert_eq!(
            problem("application/pdf", "x").as_deref(),
            Some("File 1 is over 5 MB, X's limit.")
        );
        assert_eq!(
            problem("image/gif", "bluesky").as_deref(),
            Some("Image 1 is a GIF over 1 MB, Bluesky's limit.")
        );
        // X's connector checks its larger GIF limit, a video has its own, Threads has none.
        assert_eq!(problem("image/gif", "x"), None);
        assert_eq!(problem("video/mp4", "bluesky"), None);
        let video = |size: i64, platform: &str| {
            let file = Stored {
                key: "k".into(),
                content_type: "video/mp4".into(),
                size,
            };
            let media = Media::from([(url.clone(), Ok(file))]);
            media_problem(&media, std::slice::from_ref(&url), &[], (platform, 0))
        };
        assert_eq!(
            video(100_000_001, "bluesky").as_deref(),
            Some("Video 1 is over 100 MB, Bluesky's limit for a video.")
        );
        assert_eq!(
            video(512_000_001, "x").as_deref(),
            Some("Video 1 is over 512 MB, X's limit for a video.")
        );
        assert_eq!(video(512_000_000, "x"), None);
        assert_eq!(video(1 << 30, "youtube"), None);
        assert_eq!(problem("application/pdf", "threads"), None);
    }

    #[test]
    fn an_unknown_outcome_keeps_its_x_credit_and_a_refusal_gives_it_back() {
        let mut hold = XHold {
            reference: "r".into(),
            amount: 3,
            sent: 1,
        };
        hold.failed("X refused it (403). Nothing was posted.");
        assert_eq!(hold.sent, 1);
        hold.failed(TIMED_OUT);
        assert_eq!(hold.sent, 2);
        hold.failed(&cut_off_part(2, 3));
        assert_eq!(hold.sent, 3);
        assert_eq!(hold.amount - hold.sent, 0);
        let mut hold = XHold {
            reference: "r".into(),
            amount: 2,
            sent: 0,
        };
        hold.failed(&unsent("Publish: X could not be reached.".into(), true));
        assert_eq!(hold.sent, 0, "never connected");
        hold.failed("Publish: X could not be reached.");
        assert_eq!(hold.sent, 1, "dropped");
    }

    #[test]
    fn only_a_media_failure_that_can_pass_is_marked_unsent() {
        let fail = |f| media_error(f, 1, "bluesky", 0);
        for f in [
            Fail::Storage,
            Fail::Unreachable,
            Fail::Stalled,
            Fail::Http(503),
            Fail::Http(429),
        ] {
            assert!(retry_safe(&fail(f)), "{f:?}");
        }
        for f in [
            Fail::Http(404),
            Fail::Http(403),
            Fail::Blocked,
            Fail::NotShared,
            Fail::TooLarge,
            Fail::Unsupported,
            Fail::TimedOut,
        ] {
            assert!(!retry_safe(&fail(f)), "{f:?}");
        }
    }
}
