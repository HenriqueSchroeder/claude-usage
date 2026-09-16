/// A stderr diagnostic that repeats only when the message changes.
///
/// Every boundary in this daemon is read on a timer, so a permanently broken
/// one — a missing credentials file, an endpoint answering nonsense, a
/// transcript nobody can read — would otherwise write one identical line per
/// poll, 1440 a day, and bury anything worth seeing.
#[derive(Default)]
pub struct Notes {
    last: Option<String>,
}

impl Notes {
    pub fn say(&mut self, msg: String) {
        if self.last.as_deref() != Some(msg.as_str()) {
            eprintln!("claude-usage: {msg}");
            self.last = Some(msg);
        }
    }

    /// Forgets the last message, so a problem that comes back after a
    /// recovery is reported again instead of being mistaken for the old one.
    pub fn clear(&mut self) {
        self.last = None;
    }

    /// What was last said, if anything. Lets a test assert that a boundary
    /// reported at all, which is the bug this type exists to prevent.
    #[cfg(test)]
    pub fn last_message(&self) -> Option<&str> {
        self.last.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_repeats_only_after_something_else_was_said() {
        let mut n = Notes::default();
        n.say("network down".into());
        assert_eq!(n.last.as_deref(), Some("network down"));

        // Same problem next poll: remembered, not re-printed.
        n.say("network down".into());
        assert_eq!(n.last.as_deref(), Some("network down"));

        n.say("endpoint nonsense".into());
        assert_eq!(n.last.as_deref(), Some("endpoint nonsense"));

        // A recovery clears the memory, so a relapse is heard again.
        n.clear();
        assert_eq!(n.last, None);
    }
}
