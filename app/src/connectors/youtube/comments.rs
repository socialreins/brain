//! YouTube comments for the inbox: the comment threads of a channel (`commentThreads.list` with
//! `allThreadsRelatedToChannelId`), the replies of one thread, video titles, a public reply, and
//! the channel owner's moderation. Everything needs `youtube.force-ssl`, which every connected
//! channel holds already. Each call costs quota units (a read 1, a write 50); the caller keeps the
//! count (`youtube_inbox`). No call is retried.
use chrono::{DateTime, FixedOffset};
use reqwest::Url;
use serde_json::{json, Value};

use super::{code, failed, refusal, video_id, watch_url, YouTube};
use crate::connectors::bluesky::{Author, PostError};

/// A comment, top level or reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    pub id: String,
    /// The video it is on; `None` for a comment on the channel itself.
    pub video: Option<String>,
    /// The top-level comment a reply answers.
    pub parent: Option<String>,
    pub author: Author,
    pub text: String,
    pub at: DateTime<FixedOffset>,
}

impl Comment {
    /// The public link, highlighting the comment on a video.
    pub fn url(&self) -> Option<String> {
        let v = self.video.as_deref()?;
        Some(format!("{}&lc={}", watch_url(v), self.id))
    }
}

/// A top-level comment with the replies YouTube sent along (a subset) and how many there are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    pub comment: Comment,
    pub replies: Vec<Comment>,
    pub total_replies: u32,
}

#[derive(Default)]
pub struct Threads {
    /// Newest first.
    pub items: Vec<Thread>,
    pub next: Option<String>,
}

pub struct Video {
    pub id: String,
    pub title: String,
    pub thumbnail: Option<String>,
}

/// The `@handle` in a channel link, when it has one.
fn handle_of(url: &str) -> Option<String> {
    let at = url.rsplit('/').next()?;
    at.strip_prefix('@')
        .filter(|h| !h.is_empty())
        .map(str::to_owned)
}

fn comment(v: &Value, video: Option<&str>, parent: Option<&str>) -> Option<Comment> {
    let s = &v["snippet"];
    let text = |k: &str| s[k].as_str().filter(|t| !t.is_empty()).map(str::to_owned);
    let author = s["authorChannelId"]["value"].as_str()?.to_owned();
    Some(Comment {
        id: v["id"].as_str().filter(|i| !i.is_empty())?.to_owned(),
        video: video.map(str::to_owned),
        parent: parent.map(str::to_owned),
        author: Author {
            handle: s["authorChannelUrl"].as_str().and_then(handle_of),
            display_name: text("authorDisplayName"),
            avatar: text("authorProfileImageUrl").filter(|u| u.starts_with("https://")),
            did: author,
        },
        text: text("textDisplay").unwrap_or_default(),
        at: DateTime::parse_from_rfc3339(s["publishedAt"].as_str()?).ok()?,
    })
}

fn thread(v: &Value) -> Option<Thread> {
    let video = v["snippet"]["videoId"].as_str().filter(|i| video_id(i));
    let top = comment(&v["snippet"]["topLevelComment"], video, video)?;
    let replies = v["replies"]["comments"]
        .as_array()
        .map(|a| {
            let parent = Some(top.id.as_str());
            a.iter().filter_map(|r| comment(r, video, parent)).collect()
        })
        .unwrap_or_default();
    Some(Thread {
        total_replies: v["snippet"]["totalReplyCount"].as_u64().unwrap_or(0) as u32,
        replies,
        comment: top,
    })
}

/// Words a refused comment call the way a person can act on.
fn comment_refusal(status: reqwest::StatusCode, body: &Value, step: &str) -> PostError {
    let said = |m: &str| failed(format!("{step}: {m}"));
    match (status.as_u16(), code(body)) {
        (403, "commentsDisabled") => said("comments are turned off on this video."),
        (403, "ineligibleAccount") => said(
            "this Google account must be merged with its YouTube account before it can comment.",
        ),
        (403, "forbidden") => said("YouTube did not allow it for this channel."),
        (404, "commentNotFound" | "parentCommentNotFound") => {
            said("the comment is gone on YouTube (deleted, or held back by its author).")
        }
        (400, "parentCommentIsPrivate") => said("the comment is private."),
        (400, "commentTextTooLong") => said("the reply is too long for YouTube."),
        (400, "processingFailure") => said("YouTube could not process it. Try again later."),
        _ => refusal(status, body, step),
    }
}

impl YouTube {
    /// One page (100) of the comment threads on the channel and its videos, newest first (1
    /// quota unit). `page` is a `next` of an earlier call. Held-for-review comments are not
    /// listed: YouTube returns published ones unless asked otherwise.
    pub async fn threads(
        &self,
        token: &str,
        channel: &str,
        page: Option<&str>,
    ) -> Result<Threads, PostError> {
        let mut params = vec![
            ("part", "snippet,replies"),
            ("allThreadsRelatedToChannelId", channel),
            ("maxResults", "100"),
            ("order", "time"),
            ("textFormat", "plainText"),
        ];
        params.extend(page.map(|p| ("pageToken", p)));
        let body = self
            .list_as("commentThreads", &params, "Comments", token)
            .await?;
        Ok(Threads {
            items: body["items"]
                .as_array()
                .map(|a| a.iter().filter_map(thread).collect())
                .unwrap_or_default(),
            next: body["nextPageToken"].as_str().map(str::to_owned),
        })
    }

    /// Every reply under a top-level comment, oldest first (1 unit a page of 100; one page is
    /// read, and a thread with more keeps its newest replies for the next sweep's inline ones).
    pub async fn replies(
        &self,
        token: &str,
        parent: &str,
        video: Option<&str>,
    ) -> Result<Vec<Comment>, PostError> {
        let params = [
            ("part", "snippet"),
            ("parentId", parent),
            ("maxResults", "100"),
            ("textFormat", "plainText"),
        ];
        let body = self.list_as("comments", &params, "Replies", token).await?;
        Ok(body["items"]
            .as_array()
            .map(|a| {
                let video = video.filter(|v| video_id(v));
                a.iter()
                    .filter_map(|r| comment(r, video, Some(parent)))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn list_as(
        &self,
        path: &str,
        params: &[(&str, &str)],
        step: &str,
        token: &str,
    ) -> Result<Value, PostError> {
        let url = Url::parse_with_params(&format!("{}/youtube/v3/{path}", self.api_url), params)
            .map_err(|_| failed(format!("{step}: not a valid request.")))?;
        let (status, _, body) = self
            .raw(self.client.get(url).bearer_auth(token), step)
            .await?;
        if status.is_success() {
            Ok(body)
        } else {
            Err(comment_refusal(status, &body, step))
        }
    }

    /// Titles and thumbnails of up to 50 videos (`videos.list`, 1 unit). A video that is gone or
    /// private is left out.
    pub async fn videos(&self, token: &str, ids: &[&str]) -> Result<Vec<Video>, PostError> {
        let ids: Vec<&str> = ids.iter().copied().filter(|i| video_id(i)).collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let joined = ids.join(",");
        let body = self
            .list_as(
                "videos",
                &[("part", "snippet"), ("id", &joined)],
                "Videos",
                token,
            )
            .await?;
        Ok(body["items"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| {
                        let t = &v["snippet"]["thumbnails"];
                        let thumb = ["medium", "default", "high"]
                            .iter()
                            .find_map(|k| t[*k]["url"].as_str())
                            .filter(|u| u.starts_with("https://"));
                        Some(Video {
                            id: v["id"].as_str()?.to_owned(),
                            title: v["snippet"]["title"].as_str()?.to_owned(),
                            thumbnail: thumb.map(str::to_owned),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Replies publicly under the top-level comment `parent` as the channel (`comments.insert`,
    /// 50 units). The new comment's id. Once, never retried.
    pub async fn reply(&self, token: &str, parent: &str, text: &str) -> Result<String, PostError> {
        let url = format!("{}/youtube/v3/comments?part=snippet", self.api_url);
        let body = json!({"snippet": {"parentId": parent, "textOriginal": text}});
        let req = self.client.post(url).bearer_auth(token).json(&body);
        let (status, _, body) = self.raw(req, "Reply").await?;
        if !status.is_success() {
            return Err(comment_refusal(status, &body, "Reply"));
        }
        body["id"]
            .as_str()
            .filter(|i| !i.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                failed("Reply: YouTube sent no comment id. It may have gone out, check the video.")
            })
    }

    /// Sets the moderation status of a comment on the channel's videos (`comments.
    /// setModerationStatus`, 50 units): `heldForReview` hides it until the owner approves,
    /// `published` shows it again, `rejected` hides it for good (it cannot be published again),
    /// with `ban` also banning its author from the channel's comments.
    pub async fn moderate(
        &self,
        token: &str,
        id: &str,
        status: &str,
        ban: bool,
    ) -> Result<(), PostError> {
        let mut params = vec![("id", id), ("moderationStatus", status)];
        if ban {
            params.push(("banAuthor", "true"));
        }
        let url = Url::parse_with_params(
            &format!("{}/youtube/v3/comments/setModerationStatus", self.api_url),
            params,
        )
        .map_err(|_| failed("Moderate: not a valid request."))?;
        let (status, _, body) = self
            .raw(self.client.post(url).bearer_auth(token), "Moderate")
            .await?;
        if status.is_success() {
            Ok(())
        } else {
            Err(comment_refusal(status, &body, "Moderate"))
        }
    }

    /// Deletes a comment the channel wrote (`comments.delete`, 50 units): only its author can.
    /// One already gone counts as deleted.
    pub async fn delete_comment(&self, token: &str, id: &str) -> Result<(), PostError> {
        let url = Url::parse_with_params(
            &format!("{}/youtube/v3/comments", self.api_url),
            [("id", id)],
        )
        .map_err(|_| failed("Delete: not a valid request."))?;
        let (status, _, body) = self
            .raw(self.client.delete(url).bearer_auth(token), "Delete")
            .await?;
        if status.is_success() || code(&body) == "commentNotFound" {
            Ok(())
        } else {
            Err(comment_refusal(status, &body, "Delete"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_keeps_its_video_replies_and_the_authors_handle() {
        let v = json!({"id": "T1", "snippet": {"videoId": "vid_12345A", "totalReplyCount": 3,
            "topLevelComment": {"id": "T1", "snippet": {"authorDisplayName": "Ann",
                "authorChannelUrl": "http://www.youtube.com/@ann", "authorChannelId": {"value": "UC1"},
                "authorProfileImageUrl": "https://yt3.example/a.jpg", "textDisplay": "nice",
                "publishedAt": "2026-11-24T10:00:00Z"}}},
            "replies": {"comments": [{"id": "T1.R1", "snippet": {"authorChannelId": {"value": "UC2"},
                "textDisplay": "same", "publishedAt": "2026-11-24T11:00:00Z", "parentId": "T1"}}]}});
        let t = thread(&v).unwrap();
        assert_eq!(t.comment.author.handle.as_deref(), Some("ann"));
        assert_eq!(t.comment.video.as_deref(), Some("vid_12345A"));
        assert_eq!(t.comment.parent.as_deref(), Some("vid_12345A"));
        assert_eq!(t.total_replies, 3);
        assert_eq!(t.replies[0].parent.as_deref(), Some("T1"));
        assert_eq!(
            t.comment.url().as_deref(),
            Some("https://www.youtube.com/watch?v=vid_12345A&lc=T1")
        );
        // A comment with no author channel, or no time, is left out rather than guessed.
        assert!(thread(
            &json!({"id": "x", "snippet": {"topLevelComment": {"id": "x", "snippet": {}}}})
        )
        .is_none());
    }
}
