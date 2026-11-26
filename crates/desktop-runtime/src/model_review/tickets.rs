use std::collections::HashMap;
use std::time::{Duration, Instant};

const MAX_TICKETS: usize = 16;
const PREVIEW_LIFETIME: Duration = Duration::from_secs(600);

enum State<Draft, Cancel> {
    Ready(Instant, Draft),
    Sending(Cancel),
}

pub(crate) struct Tickets<Owner, Draft, Cancel> {
    entries: HashMap<String, (Owner, State<Draft, Cancel>)>,
}

impl<Owner, Draft, Cancel> Default for Tickets<Owner, Draft, Cancel> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
}

impl<Owner: PartialEq, Draft, Cancel> Tickets<Owner, Draft, Cancel> {
    pub(super) fn issue(
        &mut self,
        ticket: String,
        owner: Owner,
        draft: Draft,
        now: Instant,
    ) -> bool {
        self.entries.retain(|_, (_, state)| !matches!(state, State::Ready(created, _) if now.duration_since(*created) >= PREVIEW_LIFETIME));
        if self.entries.len() >= MAX_TICKETS || self.entries.contains_key(&ticket) {
            return false;
        }
        self.entries
            .insert(ticket, (owner, State::Ready(now, draft)));
        true
    }

    pub(super) fn begin(
        &mut self,
        ticket: &str,
        owner: &Owner,
        cancel: Cancel,
        now: Instant,
    ) -> Option<Draft> {
        let (expected, state) = self.entries.get_mut(ticket)?;
        if expected != owner {
            return None;
        }
        match state {
            State::Sending(_) => return None,
            State::Ready(created, _) if now.duration_since(*created) >= PREVIEW_LIFETIME => {
                self.entries.remove(ticket);
                return None;
            }
            State::Ready(_, _) => {}
        }
        match std::mem::replace(state, State::Sending(cancel)) {
            State::Ready(_, draft) => Some(draft),
            State::Sending(_) => unreachable!(),
        }
    }

    pub(super) fn remove(&mut self, ticket: &str, owner: &Owner) -> bool {
        if self
            .entries
            .get(ticket)
            .is_some_and(|(expected, _)| expected == owner)
        {
            self.entries.remove(ticket);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn preview_is_owned_immutable_and_single_use() {
        let mut tickets = Tickets::default();
        let now = Instant::now();
        let owner = ("session-a", "workspace-a");
        let mut working = String::from("original selected patch");
        assert!(tickets.issue("ticket".into(), owner, working.clone(), now));
        working.clear();
        for wrong in [("session-b", "workspace-a"), ("session-a", "workspace-b")] {
            assert!(!tickets.remove("ticket", &wrong));
            assert!(tickets.begin("ticket", &wrong, (), now).is_none());
        }
        assert_eq!(
            tickets.begin("ticket", &owner, (), now).as_deref(),
            Some("original selected patch")
        );
        assert!(tickets.begin("ticket", &owner, (), now).is_none());
        assert!(tickets.remove("ticket", &owner));
        assert!(tickets.begin("ticket", &owner, (), now).is_none());
    }

    #[test]
    fn cancellation_drops_only_its_own_sender_and_invalidates_result() {
        let mut tickets = Tickets::default();
        let now = Instant::now();
        let (sender, cancelled) = mpsc::channel::<()>();
        assert!(tickets.issue("ticket".into(), 1, "text", now));
        assert_eq!(tickets.begin("ticket", &1, sender, now), Some("text"));
        assert!(!tickets.remove("ticket", &2));
        assert_eq!(cancelled.try_recv(), Err(mpsc::TryRecvError::Empty));
        assert!(tickets.remove("ticket", &1));
        assert_eq!(cancelled.try_recv(), Err(mpsc::TryRecvError::Disconnected));
        assert!(!tickets.remove("ticket", &1));
    }

    #[test]
    fn expired_cancelled_and_over_capacity_drafts_do_not_send() {
        let mut tickets = Tickets::default();
        let now = Instant::now();
        for index in 0..MAX_TICKETS {
            assert!(tickets.issue(index.to_string(), 1, "text", now));
        }
        assert!(!tickets.issue("extra".into(), 1, "text", now));
        assert!(tickets.remove("0", &1));
        assert!(tickets.begin("0", &1, (), now).is_none());
        assert!(tickets.begin("1", &1, (), now + PREVIEW_LIFETIME).is_none());
        assert!(tickets.issue("extra".into(), 1, "text", now + PREVIEW_LIFETIME));
    }
}
