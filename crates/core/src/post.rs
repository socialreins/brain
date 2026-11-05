//! Post rules that do not need a database: per-platform limits and link facets.
use unicode_segmentation::UnicodeSegmentation;

/// Bluesky: 300 graphemes, 4 images.
pub const BLUESKY_MAX_GRAPHEMES: usize = 300;
/// Bluesky also caps the text at 3000 UTF-8 bytes.
pub const BLUESKY_MAX_BYTES: usize = 3000;
pub const BLUESKY_MAX_IMAGES: usize = 4;
/// Threads: 500 characters, 20 images or videos (a carousel).
pub const THREADS_MAX_CHARS: usize = 500;
pub const THREADS_MAX_MEDIA: usize = 20;
/// LinkedIn: 3000 characters, 20 images (or one video).
pub const LINKEDIN_MAX_CHARS: usize = 3000;
pub const LINKEDIN_MAX_MEDIA: usize = 20;
/// Facebook: 63206 characters; 10 images or one video, kept to Instagram's carousel size so one
/// post group fits both.
pub const FACEBOOK_MAX_CHARS: usize = 63_206;
pub const FACEBOOK_MAX_MEDIA: usize = 10;
/// Instagram: a 2200-character caption on 1 to 10 images or videos (a carousel).
pub const INSTAGRAM_MAX_CHARS: usize = 2200;
pub const INSTAGRAM_MAX_MEDIA: usize = 10;
/// YouTube: one video; its title is the body's first line (100 characters), its description the
/// rest (5000 bytes). YouTube refuses `<` and `>` in either.
pub const YOUTUBE_MAX_TITLE: usize = 100;
pub const YOUTUBE_MAX_DESCRIPTION: usize = 5000;
/// TikTok: a caption of 2200 characters on one video or 1 to 35 photos (TikTok has no text-only
/// posts).
pub const TIKTOK_MAX_CHARS: usize = 2200;
pub const TIKTOK_MAX_MEDIA: usize = 35;
/// X: 280 weighted characters (`x_weight`), 4 images or one video or GIF.
pub const X_MAX_WEIGHT: usize = 280;
pub const X_MAX_MEDIA: usize = 4;
/// First comment limits, in characters. Meta, LinkedIn and Google document none for comments
/// through their APIs (Google only answers `commentTextTooLong`), so these are the figures the
/// platforms are widely reported to enforce; a comment over a real limit is refused at
/// publish and shows as the target's `comment_error`, never as a failed post.
pub const FACEBOOK_COMMENT_MAX_CHARS: usize = 8000;
/// Instagram: the app's 2200-character caption and comment limit.
pub const INSTAGRAM_COMMENT_MAX_CHARS: usize = 2200;
pub const LINKEDIN_COMMENT_MAX_CHARS: usize = 1250;
pub const YOUTUBE_COMMENT_MAX_CHARS: usize = 10_000;
/// Follow-up parts a thread may have after its first post.
pub const MAX_THREAD_PARTS: usize = 25;
const MAX_URL_LEN: usize = 2048;

/// Why a post cannot be saved; `code` is the API error.
#[derive(Debug, PartialEq, Eq)]
pub enum Invalid {
    UnsupportedPlatform,
    Empty,
    TooLong,
    TooManyImages,
    InvalidMedia,
    TooManyParts,
    /// The platform's publisher cannot post threads yet.
    ThreadUnsupported,
    /// Instagram posts need an image or a video; YouTube posts need a video.
    MediaRequired,
    /// YouTube refuses `<` and `>` in a title or description.
    InvalidText,
    /// The platform's publisher cannot post a first comment.
    FirstCommentUnsupported,
    FirstCommentTooLong,
    /// The platform has no way to set a video's cover image by API.
    CoverUnsupported,
    /// The cover is not a public https URL.
    InvalidCover,
    /// A cover belongs to a post with exactly one media item, which must be a video.
    CoverNeedsOneVideo,
    /// TikTok settings on a post for another platform.
    TikTokUnsupported,
    /// TikTok needs the user to choose who may see the post.
    TikTokPrivacyRequired,
    InvalidTikTokPrivacy,
    /// Your brand or Branded content ticked without the disclosure turned on.
    InvalidTikTokDisclosure,
    /// The disclosure is on but neither Your brand nor Branded content is ticked.
    TikTokDisclosureIncomplete,
    /// Branded content cannot be private (`SELF_ONLY`).
    TikTokBrandedPrivate,
}

impl Invalid {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::Empty => "empty_post",
            Self::TooLong => "too_long",
            Self::TooManyImages => "too_many_images",
            Self::InvalidMedia => "invalid_media",
            Self::TooManyParts => "too_many_parts",
            Self::ThreadUnsupported => "thread_not_supported",
            Self::MediaRequired => "media_required",
            Self::InvalidText => "invalid_text",
            Self::FirstCommentUnsupported => "first_comment_not_supported",
            Self::FirstCommentTooLong => "first_comment_too_long",
            Self::CoverUnsupported => "cover_not_supported",
            Self::InvalidCover => "invalid_cover",
            Self::CoverNeedsOneVideo => "cover_needs_one_video",
            Self::TikTokUnsupported => "tiktok_not_supported",
            Self::TikTokPrivacyRequired => "tiktok_privacy_required",
            Self::InvalidTikTokPrivacy => "invalid_tiktok_privacy",
            Self::InvalidTikTokDisclosure => "invalid_tiktok_disclosure",
            Self::TikTokDisclosureIncomplete => "tiktok_disclosure_incomplete",
            Self::TikTokBrandedPrivate => "tiktok_branded_content_private",
        }
    }
}

/// A public `https://` URL: a host, no spaces or control characters, at most 2048 bytes.
pub fn https_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    url.len() <= MAX_URL_LEN
        && !host.is_empty()
        && !host.contains('@')
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// A YouTube post's `(title, description)`: the body's first line, and the rest, both trimmed.
pub fn youtube_text(body: &str) -> (&str, &str) {
    let body = body.trim();
    let (title, rest) = body.split_once('\n').unwrap_or((body, ""));
    (title.trim(), rest.trim())
}

/// What a link counts for on X, however long it is (X wraps every link in a t.co link).
const X_LINK_WEIGHT: usize = 23;

/// Whether `word` looks like a link X would wrap: `http(s)://...`, or a bare domain such as
/// `example.com/path` (letters, digits and hyphens in each label, a final label of two or more
/// letters).
fn x_link(word: &str) -> bool {
    if word.starts_with("https://") || word.starts_with("http://") {
        return true;
    }
    let host = word.split(['/', '?', '#']).next().unwrap_or_default();
    let labels: Vec<&str> = host.split('.').collect();
    let tld = labels.last().copied().unwrap_or_default();
    labels.len() >= 2
        && labels
            .iter()
            .all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        && tld.len() >= 2
        && tld.chars().all(|c| c.is_ascii_alphabetic())
}

/// The length of `text` as X counts it, against `X_MAX_WEIGHT`: most Latin, Greek, Cyrillic and
/// punctuation characters count 1, everything else (CJK, emoji) 2, and every link 23.
/// shortcut: conservative. A bare domain is counted as a link even where X would not (a file
/// name such as `notes.txt`, for X's list of top-level domains), and text is not normalized to
/// NFC, so a post may be refused here that X would take, never the other way round. Mirror
/// twitter-text exactly if that bites.
pub fn x_weight(text: &str) -> usize {
    let weight = |c: char| match c as u32 {
        0..=0x10FF | 0x2000..=0x200D | 0x2010..=0x201F | 0x2032..=0x2037 => 1,
        _ => 2,
    };
    let mut total = 0;
    let mut rest = text;
    while !rest.is_empty() {
        let white = rest.len() - rest.trim_start().len();
        total += rest[..white].chars().map(weight).sum::<usize>();
        rest = &rest[white..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..end];
        // Trailing punctuation stays out of the link, as X does.
        let bare = word.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '"', '\'']);
        if x_link(bare) {
            total += X_LINK_WEIGHT + word[bare.len()..].chars().map(weight).sum::<usize>();
        } else {
            total += word.chars().map(weight).sum::<usize>();
        }
        rest = &rest[end..];
    }
    total
}

/// Checks a post for `platform` (`bluesky`, `threads`, `linkedin`, `facebook`, `instagram`,
/// `youtube`, `x` or `tiktok`, the ones that publish today).
pub fn validate(platform: &str, body: &str, media: &[String]) -> Result<(), Invalid> {
    let chars = || body.chars().count();
    let (too_long, max_media) = match platform {
        "bluesky" => (
            body.len() > BLUESKY_MAX_BYTES || body.graphemes(true).count() > BLUESKY_MAX_GRAPHEMES,
            BLUESKY_MAX_IMAGES,
        ),
        // shortcut: counts characters, while Threads counts an emoji by its UTF-8 bytes, so an
        // emoji-heavy post near 500 may still be refused at publish; count like Threads if it bites.
        "threads" => (chars() > THREADS_MAX_CHARS, THREADS_MAX_MEDIA),
        "linkedin" => (chars() > LINKEDIN_MAX_CHARS, LINKEDIN_MAX_MEDIA),
        "facebook" => (chars() > FACEBOOK_MAX_CHARS, FACEBOOK_MAX_MEDIA),
        "instagram" => (chars() > INSTAGRAM_MAX_CHARS, INSTAGRAM_MAX_MEDIA),
        "x" => (x_weight(body) > X_MAX_WEIGHT, X_MAX_MEDIA),
        // TikTok counts UTF-16 code units, so an emoji is two.
        "tiktok" => (
            body.encode_utf16().count() > TIKTOK_MAX_CHARS,
            TIKTOK_MAX_MEDIA,
        ),
        "youtube" => {
            let (title, description) = youtube_text(body);
            let too_long = title.chars().count() > YOUTUBE_MAX_TITLE
                || description.len() > YOUTUBE_MAX_DESCRIPTION;
            (too_long, 1)
        }
        _ => return Err(Invalid::UnsupportedPlatform),
    };
    // A video needs a title, so a YouTube post needs a body even with its video.
    if body.trim().is_empty() && (media.is_empty() || platform == "youtube") {
        return Err(Invalid::Empty);
    }
    if matches!(platform, "instagram" | "youtube" | "tiktok") && media.is_empty() {
        return Err(Invalid::MediaRequired);
    }
    if platform == "youtube" && body.contains(['<', '>']) {
        return Err(Invalid::InvalidText);
    }
    if too_long {
        return Err(Invalid::TooLong);
    }
    if media.len() > max_media {
        return Err(Invalid::TooManyImages);
    }
    if !media.iter().all(|m| https_url(m)) {
        return Err(Invalid::InvalidMedia);
    }
    Ok(())
}

/// The first comment as stored: `None` when absent or blank.
pub fn first_comment(text: Option<&str>) -> Option<&str> {
    text.filter(|t| !t.trim().is_empty())
}

/// Checks a first comment (already through `first_comment`) for `platform`: only Facebook,
/// Instagram, LinkedIn and YouTube post one.
pub fn validate_first_comment(platform: &str, text: Option<&str>) -> Result<(), Invalid> {
    let Some(text) = text else {
        return Ok(());
    };
    let max = match platform {
        "facebook" => FACEBOOK_COMMENT_MAX_CHARS,
        "instagram" => INSTAGRAM_COMMENT_MAX_CHARS,
        "linkedin" => LINKEDIN_COMMENT_MAX_CHARS,
        "youtube" => YOUTUBE_COMMENT_MAX_CHARS,
        _ => return Err(Invalid::FirstCommentUnsupported),
    };
    if text.chars().count() > max {
        return Err(Invalid::FirstCommentTooLong);
    }
    Ok(())
}

/// The cover image URL as stored: `None` when absent or blank.
pub fn cover(url: Option<&str>) -> Option<&str> {
    url.map(str::trim).filter(|u| !u.is_empty())
}

/// Checks a video cover (already through `cover`) for `platform`. Only Instagram (`cover_url`),
/// Facebook (a Reel's thumbnail) and YouTube (`thumbnails.set`) have an API for one: TikTok only
/// picks a frame (`video_cover_timestamp_ms`), and the rest have none. The post must have
/// exactly one media item; that it is a video is known only once it is downloaded, so publish
/// checks that, before anything goes out.
pub fn validate_cover(
    platform: &str,
    cover: Option<&str>,
    media: &[String],
) -> Result<(), Invalid> {
    let Some(cover) = cover else {
        return Ok(());
    };
    if !matches!(platform, "instagram" | "facebook" | "youtube") {
        return Err(Invalid::CoverUnsupported);
    }
    if !https_url(cover) {
        return Err(Invalid::InvalidCover);
    }
    if media.len() != 1 {
        return Err(Invalid::CoverNeedsOneVideo);
    }
    Ok(())
}

/// The privacy levels TikTok knows; an account is offered some of them.
pub const TIKTOK_PRIVACY_LEVELS: [&str; 4] = [
    "PUBLIC_TO_EVERYONE",
    "MUTUAL_FOLLOW_FRIENDS",
    "FOLLOWER_OF_CREATOR",
    "SELF_ONLY",
];

/// What a user chose for a TikTok post, as far as the rules of TikTok's Content Sharing
/// Guidelines go.
pub struct TikTokChoice<'a> {
    pub privacy_level: Option<&'a str>,
    pub commercial_content: bool,
    pub your_brand: bool,
    pub branded_content: bool,
}

/// Checks a post's TikTok settings for `platform`: only TikTok takes them. The user must pick
/// the privacy level (it has no default), the disclosure needs one of its two boxes, and
/// branded content cannot be private. Whether the account is offered the level is known only
/// at publish.
pub fn validate_tiktok(platform: &str, choice: Option<TikTokChoice>) -> Result<(), Invalid> {
    let Some(c) = choice else {
        return Ok(());
    };
    if platform != "tiktok" {
        return Err(Invalid::TikTokUnsupported);
    }
    let level = c.privacy_level.ok_or(Invalid::TikTokPrivacyRequired)?;
    if !TIKTOK_PRIVACY_LEVELS.contains(&level) {
        return Err(Invalid::InvalidTikTokPrivacy);
    }
    match (c.commercial_content, c.your_brand || c.branded_content) {
        (false, true) => return Err(Invalid::InvalidTikTokDisclosure),
        (true, false) => return Err(Invalid::TikTokDisclosureIncomplete),
        _ => {}
    }
    if c.branded_content && level == "SELF_ONLY" {
        return Err(Invalid::TikTokBrandedPrivate);
    }
    Ok(())
}

/// Checks a thread's follow-up parts `(body, media)` for `platform`, each as a post of its own.
/// A part's error comes with its 1-based number.
pub fn validate_thread(
    platform: &str,
    parts: &[(&str, &[String])],
) -> Result<(), (Invalid, Option<usize>)> {
    if parts.is_empty() {
        return Ok(());
    }
    if !matches!(platform, "bluesky" | "threads" | "x") {
        return Err((Invalid::ThreadUnsupported, None));
    }
    if parts.len() > MAX_THREAD_PARTS {
        return Err((Invalid::TooManyParts, None));
    }
    for (i, (body, media)) in parts.iter().enumerate() {
        validate(platform, body, media).map_err(|e| (e, Some(i + 1)))?;
    }
    Ok(())
}

/// A link in a post body: UTF-8 byte offsets `[start, end)` and the URL, as Bluesky's link
/// facets want them.
#[derive(Debug, PartialEq, Eq)]
pub struct Link {
    pub start: usize,
    pub end: usize,
    pub url: String,
}

/// `http(s)://` links in `text`, each running to the next whitespace, without trailing
/// punctuation such as `.` `,` `)` or `!`.
pub fn links(text: &str) -> Vec<Link> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = next_scheme(&text[from..]) {
        let start = from + at;
        let tail = &text[start..];
        let len = tail.find(char::is_whitespace).unwrap_or(tail.len());
        let url = tail[..len].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '"', '\'']);
        let end = start + url.len();
        if url.len() > "https://".len() {
            found.push(Link {
                start,
                end,
                url: url.to_owned(),
            });
        }
        from = start + len.max(1);
    }
    found
}

/// Where the next `http://` or `https://` starts, at the start of the text or after
/// whitespace or an opening bracket.
fn next_scheme(text: &str) -> Option<usize> {
    let mut search = 0;
    loop {
        let rel = text[search..]
            .find("https://")
            .into_iter()
            .chain(text[search..].find("http://"))
            .min()?;
        let at = search + rel;
        let before = text[..at].chars().next_back();
        if before.is_none_or(|c| c.is_whitespace() || c == '(') {
            return Some(at);
        }
        search = at + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bluesky_limits() {
        let ok = |body: &str, media: &[&str]| {
            let media: Vec<String> = media.iter().map(|m| (*m).to_owned()).collect();
            validate("bluesky", body, &media)
        };
        assert_eq!(ok("hello", &[]), Ok(()));
        assert_eq!(ok("", &["https://a.example/i.png"]), Ok(()));
        let drive = "https://drive.google.com/file/d/1AbC-d_9/view?usp=sharing";
        assert_eq!(ok("", &[drive]), Ok(()), "Drive share links are media");
        assert_eq!(ok("  ", &[]), Err(Invalid::Empty));
        // Graphemes, not chars: 300 "e" plus a combining accent are 600 chars but 300 graphemes.
        assert_eq!(ok(&"e\u{301}".repeat(300), &[]), Ok(()));
        // And bytes: a family emoji is one grapheme of 18 bytes.
        let family = "👨‍👩‍👧";
        assert_eq!(family.len(), 18);
        assert_eq!(ok(&family.repeat(166), &[]), Ok(()), "2988 bytes");
        assert_eq!(
            ok(&family.repeat(167), &[]),
            Err(Invalid::TooLong),
            "3006 bytes"
        );
        assert_eq!(ok(&family.repeat(300), &[]), Err(Invalid::TooLong));
        assert_eq!(ok(&"a".repeat(301), &[]), Err(Invalid::TooLong));
        let five = ["https://a.example/1.png"; 5];
        assert_eq!(ok("x", &five), Err(Invalid::TooManyImages));
        assert_eq!(ok("x", &five[..4]), Ok(()));
        for bad in [
            "http://a.example/i.png",
            "https://",
            "https:///x",
            "https://a b.example/x",
            "https://user@a.example/x",
            "ftp://a.example/x",
        ] {
            assert_eq!(ok("x", &[bad]), Err(Invalid::InvalidMedia), "{bad}");
        }
        assert_eq!(
            validate("myspace", "hi", &[]),
            Err(Invalid::UnsupportedPlatform)
        );
    }

    #[test]
    fn threads_limits() {
        let media = |n: usize| vec!["https://a.example/1.png".to_owned(); n];
        assert_eq!(validate("threads", &"a".repeat(500), &[]), Ok(()));
        assert_eq!(validate("threads", &"é".repeat(500), &[]), Ok(()));
        assert_eq!(
            validate("threads", &"a".repeat(501), &[]),
            Err(Invalid::TooLong)
        );
        assert_eq!(validate("threads", "x", &media(20)), Ok(()));
        assert_eq!(
            validate("threads", "x", &media(21)),
            Err(Invalid::TooManyImages)
        );
        assert_eq!(validate("threads", " ", &[]), Err(Invalid::Empty));
        assert_eq!(
            validate_thread("threads", &[("two", &media(0)[..])]),
            Ok(())
        );
    }

    #[test]
    fn x_counts_weight_with_links_at_23() {
        assert_eq!(x_weight("hello"), 5);
        assert_eq!(x_weight("é ü"), 3);
        // CJK and emoji count 2.
        assert_eq!(x_weight("日本"), 4);
        assert_eq!(x_weight("🙂"), 2);
        // Every link is 23, however long or short, and trailing punctuation stays outside it.
        assert_eq!(x_weight("https://example.com/a/very/long/path?x=1"), 23);
        assert_eq!(x_weight("http://a.co"), 23);
        assert_eq!(x_weight("see https://example.com."), 4 + 23 + 1);
        assert_eq!(x_weight("example.com/x"), 23);
        assert_eq!(x_weight("e.g. this"), 9);
        assert_eq!(x_weight("a\nb  c"), 6);
    }

    #[test]
    fn x_limits() {
        let media = |n: usize| vec!["https://a.example/1.png".to_owned(); n];
        assert_eq!(validate("x", &"a".repeat(280), &[]), Ok(()));
        assert_eq!(validate("x", &"a".repeat(281), &[]), Err(Invalid::TooLong));
        // 140 CJK characters weigh 280.
        assert_eq!(validate("x", &"日".repeat(140), &[]), Ok(()));
        assert_eq!(validate("x", &"日".repeat(141), &[]), Err(Invalid::TooLong));
        let link = |n| format!("{} https://example.com/{}", "a".repeat(n), "b".repeat(80));
        assert_eq!(validate("x", &link(256), &[]), Ok(()));
        assert_eq!(validate("x", &link(257), &[]), Err(Invalid::TooLong));
        assert_eq!(validate("x", "x", &media(4)), Ok(()));
        assert_eq!(validate("x", "x", &media(5)), Err(Invalid::TooManyImages));
        assert_eq!(validate("x", "", &media(1)), Ok(()));
        assert_eq!(validate("x", " ", &[]), Err(Invalid::Empty));
        assert_eq!(
            validate_thread("x", &[("two", &media(0)[..]), ("three", &media(4)[..])]),
            Ok(())
        );
        assert_eq!(
            validate_thread("x", &[("two", &media(0)[..]), (&"a".repeat(281), &[])]),
            Err((Invalid::TooLong, Some(2)))
        );
    }

    #[test]
    fn linkedin_limits() {
        let media = |n: usize| vec!["https://a.example/1.png".to_owned(); n];
        assert_eq!(validate("linkedin", &"a".repeat(3000), &[]), Ok(()));
        assert_eq!(
            validate("linkedin", &"a".repeat(3001), &[]),
            Err(Invalid::TooLong)
        );
        assert_eq!(validate("linkedin", "x", &media(20)), Ok(()));
        assert_eq!(
            validate("linkedin", "x", &media(21)),
            Err(Invalid::TooManyImages)
        );
        assert_eq!(
            validate_thread("linkedin", &[("two", &media(0)[..])]),
            Err((Invalid::ThreadUnsupported, None))
        );
    }

    #[test]
    fn tiktok_takes_a_caption_on_one_video_or_up_to_35_photos() {
        let media = |n: usize| -> Vec<String> {
            (0..n)
                .map(|i| format!("https://a.example/{i}.mp4"))
                .collect()
        };
        assert_eq!(validate("tiktok", "hi", &media(1)), Ok(()));
        assert_eq!(validate("tiktok", "", &media(1)), Ok(()));
        assert_eq!(validate("tiktok", "hi", &media(35)), Ok(()));
        assert_eq!(
            validate("tiktok", "hi", &media(36)),
            Err(Invalid::TooManyImages)
        );
        assert_eq!(validate("tiktok", "hi", &[]), Err(Invalid::MediaRequired));
        assert_eq!(validate("tiktok", "", &[]), Err(Invalid::Empty));
        assert_eq!(validate("tiktok", &"a".repeat(2200), &media(1)), Ok(()));
        assert_eq!(
            validate("tiktok", &"a".repeat(2201), &media(1)),
            Err(Invalid::TooLong)
        );
        // An emoji counts as two units.
        assert_eq!(validate("tiktok", &"🙂".repeat(1100), &media(1)), Ok(()));
        assert_eq!(
            validate("tiktok", &"🙂".repeat(1101), &media(1)),
            Err(Invalid::TooLong)
        );
    }

    #[test]
    fn youtube_takes_one_video_titled_by_the_first_line() {
        let video = |n: usize| vec!["https://a.example/1.mp4".to_owned(); n];
        assert_eq!(
            youtube_text("  Title \n\n rest\nmore \n"),
            ("Title", "rest\nmore")
        );
        assert_eq!(youtube_text("Only a title"), ("Only a title", ""));
        assert_eq!(validate("youtube", "Title\nabout it", &video(1)), Ok(()));
        // One video, and a body to title it.
        assert_eq!(
            validate("youtube", "Title", &[]),
            Err(Invalid::MediaRequired)
        );
        assert_eq!(
            validate("youtube", "Title", &video(2)),
            Err(Invalid::TooManyImages)
        );
        assert_eq!(validate("youtube", "  \n ", &video(1)), Err(Invalid::Empty));
        assert_eq!(validate("youtube", "", &[]), Err(Invalid::Empty));
        // 100 characters of title, 5000 bytes of description.
        let title = |n: usize| "é".repeat(n);
        assert_eq!(validate("youtube", &title(100), &video(1)), Ok(()));
        assert_eq!(
            validate("youtube", &title(101), &video(1)),
            Err(Invalid::TooLong)
        );
        let body = |n: usize| format!("T\n{}", "a".repeat(n));
        assert_eq!(validate("youtube", &body(5000), &video(1)), Ok(()));
        assert_eq!(
            validate("youtube", &body(5001), &video(1)),
            Err(Invalid::TooLong)
        );
        let bytes = format!("T\n{}", "é".repeat(2501));
        assert_eq!(
            validate("youtube", &bytes, &video(1)),
            Err(Invalid::TooLong)
        );
        assert_eq!(
            validate("youtube", "a <b>", &video(1)),
            Err(Invalid::InvalidText)
        );
        assert_eq!(
            validate("youtube", "T\n1 > 0", &video(1)),
            Err(Invalid::InvalidText)
        );
        assert_eq!(
            validate_thread("youtube", &[("two", &video(0)[..])]),
            Err((Invalid::ThreadUnsupported, None))
        );
    }

    #[test]
    fn facebook_and_instagram_limits() {
        let media = |n: usize| vec!["https://a.example/1.jpg".to_owned(); n];
        assert_eq!(validate("facebook", "text only", &[]), Ok(()));
        assert_eq!(
            validate("facebook", &"a".repeat(63_206), &media(10)),
            Ok(())
        );
        assert_eq!(
            validate("facebook", &"a".repeat(63_207), &[]),
            Err(Invalid::TooLong)
        );
        assert_eq!(
            validate("facebook", "x", &media(11)),
            Err(Invalid::TooManyImages)
        );
        assert_eq!(
            validate("instagram", "no picture", &[]),
            Err(Invalid::MediaRequired)
        );
        assert_eq!(validate("instagram", "", &media(1)), Ok(()));
        assert_eq!(validate("instagram", &"a".repeat(2200), &media(10)), Ok(()));
        assert_eq!(
            validate("instagram", &"a".repeat(2201), &media(1)),
            Err(Invalid::TooLong)
        );
        assert_eq!(
            validate("instagram", "x", &media(11)),
            Err(Invalid::TooManyImages)
        );
        assert_eq!(validate("instagram", " ", &[]), Err(Invalid::Empty));
        for p in ["facebook", "instagram"] {
            assert_eq!(
                validate_thread(p, &[("two", &media(1)[..])]),
                Err((Invalid::ThreadUnsupported, None))
            );
        }
    }

    #[test]
    fn first_comments_are_for_four_platforms_with_their_limits() {
        assert_eq!(first_comment(None), None);
        assert_eq!(first_comment(Some(" \n")), None);
        assert_eq!(first_comment(Some("src")), Some("src"));
        for p in ["bluesky", "threads", "x", "tiktok"] {
            assert_eq!(validate_first_comment(p, None), Ok(()), "{p} without");
            assert_eq!(
                validate_first_comment(p, Some("hi")),
                Err(Invalid::FirstCommentUnsupported),
                "{p}"
            );
        }
        for (p, max) in [
            ("facebook", 8000),
            ("instagram", 2200),
            ("linkedin", 1250),
            ("youtube", 10_000),
        ] {
            assert_eq!(validate_first_comment(p, Some(&"é".repeat(max))), Ok(()));
            assert_eq!(
                validate_first_comment(p, Some(&"é".repeat(max + 1))),
                Err(Invalid::FirstCommentTooLong),
                "{p}"
            );
        }
    }

    #[test]
    fn a_cover_is_for_one_video_on_three_platforms() {
        let one = vec!["https://a.example/v.mp4".to_owned()];
        let two = vec![one[0].clone(), one[0].clone()];
        let c = Some("https://a.example/c.jpg");
        assert_eq!(cover(None), None);
        assert_eq!(cover(Some("  ")), None);
        assert_eq!(cover(Some(" https://a.example/c.jpg ")), c);
        for p in ["instagram", "facebook", "youtube", "tiktok", "x"] {
            assert_eq!(validate_cover(p, None, &two), Ok(()), "{p} without");
        }
        for p in ["instagram", "facebook", "youtube"] {
            assert_eq!(validate_cover(p, c, &one), Ok(()), "{p}");
            assert_eq!(validate_cover(p, c, &[]), Err(Invalid::CoverNeedsOneVideo));
            assert_eq!(validate_cover(p, c, &two), Err(Invalid::CoverNeedsOneVideo));
            assert_eq!(
                validate_cover(p, Some("http://a.example/c.jpg"), &one),
                Err(Invalid::InvalidCover)
            );
        }
        for p in ["tiktok", "x", "linkedin", "threads", "bluesky"] {
            assert_eq!(validate_cover(p, c, &one), Err(Invalid::CoverUnsupported));
        }
    }

    #[test]
    fn tiktok_settings_follow_the_content_sharing_guidelines() {
        let c = |level: Option<&'static str>, on, brand, branded| {
            Some(TikTokChoice {
                privacy_level: level,
                commercial_content: on,
                your_brand: brand,
                branded_content: branded,
            })
        };
        let public = Some("PUBLIC_TO_EVERYONE");
        assert_eq!(validate_tiktok("x", None), Ok(()));
        assert_eq!(validate_tiktok("tiktok", None), Ok(()));
        assert_eq!(
            validate_tiktok("x", c(public, false, false, false)),
            Err(Invalid::TikTokUnsupported)
        );
        assert_eq!(
            validate_tiktok("tiktok", c(public, false, false, false)),
            Ok(())
        );
        assert_eq!(
            validate_tiktok("tiktok", c(None, false, false, false)),
            Err(Invalid::TikTokPrivacyRequired)
        );
        assert_eq!(
            validate_tiktok("tiktok", c(Some("EVERYONE"), false, false, false)),
            Err(Invalid::InvalidTikTokPrivacy)
        );
        assert_eq!(
            validate_tiktok("tiktok", c(public, false, true, false)),
            Err(Invalid::InvalidTikTokDisclosure)
        );
        assert_eq!(
            validate_tiktok("tiktok", c(public, true, false, false)),
            Err(Invalid::TikTokDisclosureIncomplete)
        );
        assert_eq!(
            validate_tiktok("tiktok", c(public, true, true, true)),
            Ok(())
        );
        let private = Some("SELF_ONLY");
        assert_eq!(
            validate_tiktok("tiktok", c(private, true, true, false)),
            Ok(())
        );
        assert_eq!(
            validate_tiktok("tiktok", c(private, true, false, true)),
            Err(Invalid::TikTokBrandedPrivate)
        );
        assert_eq!(
            validate_tiktok("tiktok", c(private, true, true, true)),
            Err(Invalid::TikTokBrandedPrivate)
        );
    }

    #[test]
    fn thread_parts_follow_the_platform_rules() {
        let img = vec!["https://a.example/1.png".to_owned(); 5];
        let part = |body: &'static str, n: usize| (body, &img[..n]);
        assert_eq!(validate_thread("bluesky", &[]), Ok(()));
        assert_eq!(validate_thread("linkedin", &[]), Ok(()));
        assert_eq!(
            validate_thread("linkedin", &[part("hi", 0)]),
            Err((Invalid::ThreadUnsupported, None))
        );
        let full = vec![part("hi", 4); MAX_THREAD_PARTS];
        assert_eq!(validate_thread("bluesky", &full), Ok(()));
        let over = vec![part("hi", 0); MAX_THREAD_PARTS + 1];
        assert_eq!(
            validate_thread("bluesky", &over),
            Err((Invalid::TooManyParts, None))
        );
        let long = "a".repeat(301);
        let parts = [part("ok", 0), (long.as_str(), &img[..0])];
        assert_eq!(
            validate_thread("bluesky", &parts),
            Err((Invalid::TooLong, Some(2)))
        );
        assert_eq!(
            validate_thread("bluesky", &[part("ok", 5)]),
            Err((Invalid::TooManyImages, Some(1)))
        );
        assert_eq!(
            validate_thread("bluesky", &[part(" ", 0)]),
            Err((Invalid::Empty, Some(1)))
        );
    }

    #[test]
    fn links_use_byte_offsets_and_drop_trailing_punctuation() {
        let text = "🦀 see https://example.com/a?b=1. and (http://x.org/y), ok";
        let found = links(text);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].url, "https://example.com/a?b=1");
        assert_eq!(&text[found[0].start..found[0].end], found[0].url);
        assert_eq!(found[0].start, "🦀 see ".len());
        assert_eq!(found[1].url, "http://x.org/y");
        assert_eq!(&text[found[1].start..found[1].end], found[1].url);
    }

    #[test]
    fn not_links() {
        assert!(links("no links here").is_empty());
        assert!(links("xhttps://example.com").is_empty());
        assert!(links("https:// alone").is_empty());
        assert_eq!(links("https://a.io").len(), 1);
    }
}
