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

/// Checks a post for `platform` (`bluesky`, `threads`, `linkedin`, `facebook`, `instagram` or
/// `youtube`, the ones that publish today).
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
    if matches!(platform, "instagram" | "youtube") && media.is_empty() {
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

/// Checks a thread's follow-up parts `(body, media)` for `platform`, each as a post of its own.
/// A part's error comes with its 1-based number.
pub fn validate_thread(
    platform: &str,
    parts: &[(&str, &[String])],
) -> Result<(), (Invalid, Option<usize>)> {
    if parts.is_empty() {
        return Ok(());
    }
    if !matches!(platform, "bluesky" | "threads") {
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
        assert_eq!(validate("x", "hi", &[]), Err(Invalid::UnsupportedPlatform));
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
    fn thread_parts_follow_the_platform_rules() {
        let img = vec!["https://a.example/1.png".to_owned(); 5];
        let part = |body: &'static str, n: usize| (body, &img[..n]);
        assert_eq!(validate_thread("bluesky", &[]), Ok(()));
        assert_eq!(validate_thread("x", &[]), Ok(()));
        assert_eq!(
            validate_thread("x", &[part("hi", 0)]),
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
