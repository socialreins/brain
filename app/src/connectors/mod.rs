//! Platform connectors: the outbound HTTP each provider needs, free of request handling.
pub mod bluesky;
pub mod google;
pub mod linkedin;
pub mod meta;
pub mod pinterest;
pub mod threads;
pub mod tiktok;
pub mod x;
pub mod youtube;

tokio::task_local! {
    /// The slots the target publishing in this task holds (`publish::publish_target_held`).
    pub static SLOTS: std::sync::Arc<Slots>;
}

/// The global post slot a post holds while it publishes, given back when every one of its targets
/// is waiting or done: a post that is only waiting for its platform needs no slot, and a post not
/// yet started still queues for one, so a stopping process hands those back as before.
pub struct PostHold {
    permit: std::sync::Mutex<Option<tokio::sync::SemaphorePermit<'static>>>,
    active: std::sync::atomic::AtomicUsize,
}

impl PostHold {
    pub fn new(permit: tokio::sync::SemaphorePermit<'static>, targets: usize) -> Self {
        Self {
            permit: std::sync::Mutex::new(Some(permit)),
            active: std::sync::atomic::AtomicUsize::new(targets),
        }
    }

    /// One target stopped working (it waits or it ended); the last one gives the slot back.
    fn one_less(&self) {
        if self
            .active
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst)
            == 1
        {
            self.permit.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
    }
}

/// The permits of the shared publish slots a target holds. `idle` hands them back for good once
/// the target only has light calls left, so a platform's processing wait (Threads' 30 seconds, a
/// video's status polls) never keeps a slot from a target that can send.
// shortcut: no slot is taken again after the wait; what follows is a status poll and the final
// publish call, a few small requests. Take them again if a connector ever uploads after a wait
// (a thread whose later parts carry media does: see `idle`'s callers).
pub struct Slots {
    permits: std::sync::Mutex<Vec<tokio::sync::OwnedSemaphorePermit>>,
    post: std::sync::Arc<PostHold>,
    counted: std::sync::atomic::AtomicBool,
}

impl Slots {
    pub fn new(
        permits: Vec<tokio::sync::OwnedSemaphorePermit>,
        post: std::sync::Arc<PostHold>,
    ) -> Self {
        Self {
            permits: std::sync::Mutex::new(permits),
            post,
            counted: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn release(&self) {
        self.permits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.count_out();
    }

    /// This target no longer counts as working for its post, once.
    fn count_out(&self) {
        if !self.counted.swap(true, std::sync::atomic::Ordering::SeqCst) {
            self.post.one_less();
        }
    }
}

impl Drop for Slots {
    fn drop(&mut self) {
        self.count_out();
    }
}

/// Waits `time` for the platform to process what was sent, holding no publish slot while it does
/// (see `Slots`). Only for waits after which no large transfer follows. Outside a publish it is
/// a plain sleep. The wait is never followed by a repeat of a call that may have been sent: this
/// changes who holds a slot, nothing about what is sent or when (decision 0008).
pub async fn idle(time: std::time::Duration) {
    if let Ok(slots) = SLOTS.try_with(std::sync::Arc::clone) {
        slots.release();
    }
    tokio::time::sleep(time).await;
}

tokio::task_local! {
    /// Extra processing checks for the publish running in this task: `publish::send` sets it
    /// from the size of the post's videos, so no connector call has to carry it.
    pub static PATIENCE: u32;
}

tokio::task_local! {
    /// What the publish running in this task could not do for the post's images' alt text, one
    /// line each: `publish::send` collects them for the target's `alt_error`. An alt text step
    /// that fails never fails the post.
    pub static ALT_NOTES: std::cell::RefCell<Vec<String>>;
}

/// Records that the alt text of image `n` (1-based) was not sent, and `why`.
pub fn alt_note(n: usize, why: &bluesky::PostError) {
    let why = match why {
        bluesky::PostError::Failed(msg) => msg.replace(&format!(" {UNSENT}"), ""),
        bluesky::PostError::Reconnect => "the account needs to be reconnected.".to_owned(),
        bluesky::PostError::Denied => "the platform denied it.".to_owned(),
    };
    let note = format!("Alt text of image {n} was not added: {why}");
    let _ = ALT_NOTES.try_with(|notes| notes.borrow_mut().push(note));
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

    #[tokio::test]
    async fn idle_hands_the_slots_back_while_it_waits() {
        use std::{sync::Arc, time::Duration};
        use tokio::sync::Semaphore;
        let gate = Arc::new(Semaphore::new(8));
        static POSTS: std::sync::LazyLock<Semaphore> =
            std::sync::LazyLock::new(|| Semaphore::new(8));
        let post_gate = &*POSTS;
        // 40 publishes that each wait 100 ms for a platform, 8 slots: holding a slot while
        // waiting takes 5 rounds, giving it back takes about one.
        let run = |idle_wait: bool| {
            let gate = gate.clone();
            async move {
                let started = std::time::Instant::now();
                let mut all = Vec::new();
                for _ in 0..40 {
                    let permit = gate.clone().acquire_owned().await.unwrap();
                    let post =
                        Arc::new(super::PostHold::new(post_gate.acquire().await.unwrap(), 1));
                    let slots = Arc::new(super::Slots::new(vec![permit], post));
                    all.push(tokio::spawn(super::SLOTS.scope(
                        slots.clone(),
                        async move {
                            if idle_wait {
                                super::idle(Duration::from_millis(100)).await;
                            } else {
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                            drop(slots);
                        },
                    )));
                }
                for t in all {
                    t.await.unwrap();
                }
                started.elapsed()
            }
        };
        let held = run(false).await;
        let given = run(true).await;
        assert!(held >= Duration::from_millis(450), "{held:?}");
        assert!(given < Duration::from_millis(400), "{given:?}");
        assert_eq!(post_gate.available_permits(), 8);
        assert_eq!(gate.available_permits(), 8);
        // Outside a publish it is a plain sleep.
        super::idle(Duration::from_millis(1)).await;
    }
}
