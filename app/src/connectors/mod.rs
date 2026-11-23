//! Platform connectors: the outbound HTTP each provider needs, free of request handling.
pub mod bluesky;
pub mod google;
pub mod linkedin;
pub mod meta;
pub mod threads;
pub mod tiktok;
pub mod x;
pub mod youtube;

tokio::task_local! {
    /// Extra processing checks for the publish running in this task: `publish::send` sets it
    /// from the size of the post's videos, so no connector call has to carry it.
    pub static PATIENCE: u32;
}

/// Status checks a processing poll adds to its own for a large video (`publish::patience`: one
/// per 100 MB, at most 10); none outside a publish and for a small file.
pub fn extra_polls() -> u32 {
    PATIENCE.try_with(|n| *n).unwrap_or(0)
}

/// Ends a publish failure that certainly made nothing on the platform: the call was refused for
/// a rate limit, or its connection never opened. Only the connectors add it (`unsent`), and only
/// on calls that come before the one that creates the post or are that call, so
/// `publish::retry_safe` can send such a target once more. Every other failure, a 5xx or a
/// dropped connection included, may have posted and is never retried.
pub const UNSENT: &str = "Nothing was posted.";

/// `msg`, marked `UNSENT` when `sure` says nothing was posted.
pub fn unsent(msg: String, sure: bool) -> String {
    if sure {
        format!("{msg} {UNSENT}")
    } else {
        msg
    }
}

/// `e` without the `UNSENT` mark: for a failure once the platform may already hold the post,
/// such as a token renewal after the last bytes of an upload went out.
pub fn maybe_sent(e: bluesky::PostError) -> bluesky::PostError {
    match e {
        bluesky::PostError::Failed(msg) => {
            bluesky::PostError::Failed(msg.replace(&format!(" {UNSENT}"), ""))
        }
        other => other,
    }
}

/// A request that failed before its connection opened sent nothing.
pub fn never_sent(e: &reqwest::Error) -> bool {
    e.is_connect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unsent_mark_comes_and_goes() {
        let marked = unsent("Publish: X could not be reached.".into(), true);
        assert_eq!(
            marked,
            "Publish: X could not be reached. Nothing was posted."
        );
        assert_eq!(unsent("Timed out.".into(), false), "Timed out.");
        let bluesky::PostError::Failed(kept) = maybe_sent(bluesky::PostError::Failed(marked))
        else {
            panic!("still a failure");
        };
        assert_eq!(kept, "Publish: X could not be reached.");
    }
}
