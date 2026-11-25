//! What Facebook and Instagram let a message do, as "Test this automation" plays it: the same
//! windows `meta_inbox::send` enforces on a real run, from how old the comment or the person's
//! last message is. A private reply to a comment goes within 7 days of the comment, once per
//! comment. Any other message goes within 24 hours of the person's last one; their answer starts
//! a new 24 hours. An automation never uses the HUMAN_AGENT tag (`META_HUMAN_AGENT`), which only a
//! person's reply may, up to 7 days: the test notes it.
use crate::meta_inbox::{HUMAN_AGENT_DAYS, MESSAGE_HOURS, PRIVATE_REPLY_DAYS};

/// Why Meta would refuse a DM now, and a note for the person testing.
#[derive(Debug, PartialEq, Eq)]
pub struct Refusal {
    /// `outside_window` or `already_replied`, as a run's result words them.
    pub reason: &'static str,
    /// `human_agent_by_hand` (a person could still answer by hand, with the tag) or
    /// `human_agent_off` (they could with Meta's Human Agent feature, `META_HUMAN_AGENT`).
    pub note: Option<&'static str>,
}

/// Time since the thing a message answers, and what was already sent.
#[derive(Debug)]
pub struct Clock {
    /// Whether the run started on a comment (its first DM is a private reply).
    comment: bool,
    /// Minutes since the comment, or since the person's last message.
    elapsed: u64,
    /// The person has answered since: DMs now go in their conversation.
    answered: bool,
    /// The comment's one private reply is used.
    private_used: bool,
}

impl Clock {
    /// A run on `trigger` whose item is `age_minutes` old; `private_used`: a private reply to
    /// the comment already went out.
    pub fn new(trigger: &str, age_minutes: u32, private_used: bool) -> Self {
        Self {
            comment: trigger == "comment_received",
            elapsed: u64::from(age_minutes),
            answered: false,
            private_used,
        }
    }

    /// A `wait`, or a `wait_for_reply` (the test goes on as if the person answered then).
    pub fn wait(&mut self, minutes: u32, for_reply: bool) {
        self.elapsed += u64::from(minutes);
        if for_reply {
            self.elapsed = 0;
            self.answered = true;
        }
    }

    /// A DM now: `Ok`, or what Meta would refuse it for. A private reply uses up the comment's
    /// one.
    pub fn dm(&mut self, human_agent: bool) -> Result<(), Refusal> {
        let (hours, days) = (MESSAGE_HOURS as u64, PRIVATE_REPLY_DAYS as u64);
        if self.comment && !self.answered {
            if self.elapsed >= days * 24 * 60 {
                return Err(Refusal {
                    reason: "outside_window",
                    note: None,
                });
            }
            if self.private_used {
                return Err(Refusal {
                    reason: "already_replied",
                    note: None,
                });
            }
            self.private_used = true;
            return Ok(());
        }
        if self.elapsed >= hours * 60 {
            let by_hand = self.elapsed < HUMAN_AGENT_DAYS as u64 * 24 * 60;
            return Err(Refusal {
                reason: "outside_window",
                note: by_hand.then_some(if human_agent {
                    "human_agent_by_hand"
                } else {
                    "human_agent_off"
                }),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u32 = 24 * 60;

    #[test]
    fn a_comment_takes_one_private_reply_within_seven_days() {
        let mut c = Clock::new("comment_received", 6 * DAY, false);
        assert!(c.dm(false).is_ok());
        assert_eq!(c.dm(false).unwrap_err().reason, "already_replied");
        let mut late = Clock::new("comment_received", 7 * DAY, false);
        assert_eq!(late.dm(false).unwrap_err().reason, "outside_window");
        let mut used = Clock::new("comment_received", 0, true);
        assert_eq!(used.dm(false).unwrap_err().reason, "already_replied");
    }

    #[test]
    fn an_answer_opens_a_new_day_for_messages() {
        let mut c = Clock::new("comment_received", 0, false);
        assert!(c.dm(false).is_ok());
        c.wait(90 * 60 / 60, true);
        assert!(
            c.dm(false).is_ok(),
            "in the answer's conversation, not a second private reply"
        );
        c.wait(DAY, false);
        assert_eq!(c.dm(false).unwrap_err().reason, "outside_window");
    }

    #[test]
    fn a_message_goes_within_24_hours_and_a_person_may_use_the_tag_to_7_days() {
        let mut c = Clock::new("dm_received", 23 * 60, false);
        assert!(c.dm(false).is_ok());
        let mut late = Clock::new("dm_received", 2 * DAY, false);
        let no = late.dm(true).unwrap_err();
        assert_eq!(
            (no.reason, no.note),
            ("outside_window", Some("human_agent_by_hand"))
        );
        let off = Clock::new("story_reply", 2 * DAY, false)
            .dm(false)
            .unwrap_err();
        assert_eq!(off.note, Some("human_agent_off"));
        // Past 7 days not even a person can.
        let gone = Clock::new("dm_received", 8 * DAY, false)
            .dm(true)
            .unwrap_err();
        assert_eq!(gone.note, None);
        // A wait inside a flow eats the window.
        let mut flow = Clock::new("dm_received", 0, false);
        flow.wait(25 * 60, false);
        assert_eq!(flow.dm(false).unwrap_err().reason, "outside_window");
    }
}
