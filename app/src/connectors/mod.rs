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
