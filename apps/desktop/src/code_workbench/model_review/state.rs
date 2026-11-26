use std::collections::BTreeSet;

#[derive(Default)]
pub(super) struct DiffSelection {
    rows: Vec<(String, bool)>,
    checked: BTreeSet<usize>,
    pub(super) has_untracked: bool,
}

pub(super) enum FileSides<'a> {
    Tracked {
        path: &'a str,
        staged: bool,
        unstaged: bool,
    },
    Untracked,
}

impl DiffSelection {
    pub(super) fn new<'a>(changes: impl Iterator<Item = FileSides<'a>>) -> Self {
        let mut selection = Self::default();
        for change in changes {
            let FileSides::Tracked {
                path,
                staged,
                unstaged,
            } = change
            else {
                selection.has_untracked = true;
                continue;
            };
            if staged {
                selection.rows.push((path.to_owned(), true));
            }
            if unstaged {
                selection.rows.push((path.to_owned(), false));
            }
        }
        selection
    }

    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(super) fn row(&self, index: usize) -> Option<(&str, bool, bool)> {
        self.rows
            .get(index)
            .map(|(path, staged)| (path.as_str(), *staged, self.checked.contains(&index)))
    }

    pub(super) fn check(&mut self, index: usize, checked: bool) {
        if checked && index < self.rows.len() {
            self.checked.insert(index);
        } else {
            self.checked.remove(&index);
        }
    }

    pub(super) fn selected(&self) -> impl Iterator<Item = &(String, bool)> {
        self.checked.iter().map(|index| &self.rows[*index])
    }
}

pub(super) enum Phase<Ticket, Text> {
    Loading,
    Choosing,
    Previewing,
    Ready(Ticket, Text),
    Sending(Ticket),
    Cancelling { ticket: Ticket, sent: bool },
    CancelFailed { ticket: Ticket, sent: bool },
    AwaitingCancel,
    AwaitingSend { cancelled: bool },
    Finished,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ReviewError {
    TicketInvalid(String),
    Other(String),
}

impl ReviewError {
    pub(super) fn message(self) -> String {
        match self {
            Self::TicketInvalid(message) | Self::Other(message) => message,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Notice {
    Recorded,
    Cancelled { sent: bool },
    Error(String),
}

impl<Ticket: Clone, Text> Phase<Ticket, Text> {
    pub(super) fn finish_send(&mut self, result: Result<(), ReviewError>) -> Option<Notice> {
        let notice = match (&*self, result) {
            (Self::Cancelling { sent: true, .. }, Err(ReviewError::TicketInvalid(_))) => {
                *self = Self::AwaitingCancel;
                return None;
            }
            (Self::AwaitingSend { cancelled: true }, Err(ReviewError::TicketInvalid(_))) => {
                Notice::Cancelled { sent: true }
            }
            (
                Self::Sending(_)
                | Self::Cancelling { sent: true, .. }
                | Self::CancelFailed { sent: true, .. }
                | Self::AwaitingSend { .. },
                result,
            ) => match result {
                Ok(()) => Notice::Recorded,
                Err(error) => Notice::Error(error.message()),
            },
            _ => return None,
        };
        *self = Self::Finished;
        Some(notice)
    }

    pub(super) fn finish_cancel(&mut self, result: Result<(), ReviewError>) -> Option<Notice> {
        let (next, notice) = match (&*self, result) {
            (Self::Cancelling { sent: true, .. }, Ok(())) => (
                Self::AwaitingSend { cancelled: true },
                Notice::Cancelled { sent: true },
            ),
            (Self::Cancelling { sent: true, .. }, Err(ReviewError::TicketInvalid(message))) => (
                Self::AwaitingSend { cancelled: false },
                Notice::Error(message),
            ),
            (Self::Cancelling { ticket, sent }, Err(ReviewError::Other(message))) => (
                Self::CancelFailed {
                    ticket: ticket.clone(),
                    sent: *sent,
                },
                Notice::Error(message),
            ),
            (Self::Cancelling { sent: false, .. }, Ok(())) => {
                (Self::Finished, Notice::Cancelled { sent: false })
            }
            (Self::AwaitingCancel, Ok(())) => (Self::Finished, Notice::Cancelled { sent: true }),
            (Self::AwaitingCancel | Self::Cancelling { sent: false, .. }, Err(error)) => {
                (Self::Finished, Notice::Error(error.message()))
            }
            _ => return None,
        };
        *self = next;
        Some(notice)
    }

    pub(super) fn close_only(&self) -> bool {
        matches!(
            self,
            Self::AwaitingCancel | Self::AwaitingSend { .. } | Self::Finished
        )
    }

    pub(super) fn begin_send(&mut self) -> Option<Ticket> {
        let Self::Ready(ticket, _) = self else {
            return None;
        };
        let ticket = ticket.clone();
        *self = Self::Sending(ticket.clone());
        Some(ticket)
    }

    pub(super) fn begin_cancel(&mut self) -> Option<Ticket> {
        let (ticket, sent) = match self {
            Self::Ready(ticket, _) => (ticket.clone(), false),
            Self::Sending(ticket) => (ticket.clone(), true),
            Self::CancelFailed { ticket, sent } => (ticket.clone(), *sent),
            _ => return None,
        };
        *self = Self::Cancelling {
            ticket: ticket.clone(),
            sent,
        };
        Some(ticket)
    }

    pub(super) fn unsent_ticket(&self) -> Option<&Ticket> {
        match self {
            Self::Ready(ticket, _)
            | Self::CancelFailed {
                ticket,
                sent: false,
            } => Some(ticket),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cancelling_send() -> Phase<u8, &'static str> {
        let mut phase = Phase::Ready(1, "preview");
        assert_eq!(phase.begin_send(), Some(1));
        assert_eq!(phase.begin_cancel(), Some(1));
        phase
    }

    fn invalid() -> Result<(), ReviewError> {
        Err(ReviewError::TicketInvalid("invalid ticket".into()))
    }

    #[test]
    fn upstream_failure_survives_racing_invalid_cancel_in_both_orders() {
        for cancel_first in [true, false] {
            let mut phase = cancelling_send();
            if cancel_first {
                assert_eq!(
                    phase.finish_cancel(invalid()),
                    Some(Notice::Error("invalid ticket".into()))
                );
                assert!(phase.close_only());
                assert_eq!(phase.begin_cancel(), None);
            }
            assert_eq!(
                phase.finish_send(Err(ReviewError::Other("HTTP 502".into()))),
                Some(Notice::Error("HTTP 502".into()))
            );
            if !cancel_first {
                assert_eq!(phase.finish_cancel(invalid()), None);
            }
            assert!(matches!(phase, Phase::Finished));
            assert_eq!(phase.begin_cancel(), None);
        }
    }

    #[test]
    fn only_confirmed_cancellation_is_reported_as_cancelled_in_either_order() {
        for cancel_first in [true, false] {
            let mut phase = cancelling_send();
            if cancel_first {
                assert_eq!(
                    phase.finish_cancel(Ok(())),
                    Some(Notice::Cancelled { sent: true })
                );
                assert!(phase.close_only());
                assert_eq!(
                    phase.finish_send(invalid()),
                    Some(Notice::Cancelled { sent: true })
                );
            } else {
                assert_eq!(phase.finish_send(invalid()), None);
                assert!(phase.close_only());
                assert_eq!(
                    phase.finish_cancel(Ok(())),
                    Some(Notice::Cancelled { sent: true })
                );
            }
            assert!(matches!(phase, Phase::Finished));
        }
        let mut phase = cancelling_send();
        assert_eq!(phase.finish_send(invalid()), None);
        assert_eq!(
            phase.finish_cancel(invalid()),
            Some(Notice::Error("invalid ticket".into()))
        );
        assert!(matches!(phase, Phase::Finished));
    }

    #[test]
    fn retry_is_only_available_while_the_ticket_might_still_be_valid() {
        let mut phase = Phase::Ready(4, "preview");
        assert_eq!(phase.begin_cancel(), Some(4));
        assert_eq!(
            phase.finish_cancel(Err(ReviewError::Other("offline".into()))),
            Some(Notice::Error("offline".into()))
        );
        assert_eq!(phase.begin_cancel(), Some(4));
        assert_eq!(
            phase.finish_cancel(invalid()),
            Some(Notice::Error("invalid ticket".into()))
        );
        assert!(phase.close_only());
        assert_eq!(phase.begin_cancel(), None);

        let mut phase = cancelling_send();
        assert_eq!(
            phase.finish_cancel(invalid()),
            Some(Notice::Error("invalid ticket".into()))
        );
        assert_eq!(phase.finish_send(Ok(())), Some(Notice::Recorded));
        assert!(phase.close_only());
    }

    #[test]
    fn partial_staging_is_two_independent_opt_ins() {
        let mut selection = DiffSelection::new(
            [
                FileSides::Tracked {
                    path: "both",
                    staged: true,
                    unstaged: true,
                },
                FileSides::Tracked {
                    path: "other",
                    staged: false,
                    unstaged: true,
                },
            ]
            .into_iter(),
        );
        assert!(!selection.has_untracked);
        assert_eq!(selection.len(), 3);
        assert_eq!(selection.row(0), Some(("both", true, false)));
        assert_eq!(selection.row(1), Some(("both", false, false)));
        assert_eq!(selection.selected().count(), 0);
        selection.check(1, true);
        selection.check(0, true);
        selection.check(1, false);
        selection.check(3, true);
        assert_eq!(
            selection.selected().cloned().collect::<Vec<_>>(),
            [("both".to_owned(), true)]
        );
    }

    #[test]
    fn untracked_files_have_no_selectable_side() {
        let mut selection = DiffSelection::new([FileSides::Untracked].into_iter());
        assert!(selection.has_untracked);
        assert_eq!(selection.len(), 0);
        selection.check(0, true);
        assert_eq!(selection.selected().count(), 0);
    }

    #[test]
    fn only_ready_can_send_and_only_once() {
        for mut phase in [
            Phase::<u8, &str>::Loading,
            Phase::Choosing,
            Phase::Previewing,
            Phase::Finished,
        ] {
            assert_eq!(phase.begin_send(), None::<u8>);
            assert_eq!(phase.begin_cancel(), None);
        }
        let mut phase = Phase::Ready(7, "frozen request");
        assert_eq!(phase.begin_send(), Some(7));
        assert_eq!(phase.begin_send(), None);
        assert_eq!(phase.unsent_ticket(), None);
        assert_eq!(phase.begin_cancel(), Some(7));
        assert!(matches!(
            phase,
            Phase::Cancelling {
                ticket: 7,
                sent: true
            }
        ));
        assert_eq!(phase.begin_cancel(), None);
        assert_eq!(phase.begin_send(), None);
    }

    #[test]
    fn cancelling_a_preview_never_arms_send_even_on_failure() {
        let mut phase = Phase::Ready(3, "frozen request");
        assert_eq!(phase.unsent_ticket(), Some(&3));
        assert_eq!(phase.begin_cancel(), Some(3));
        assert!(matches!(phase, Phase::Cancelling { sent: false, .. }));
        phase = Phase::CancelFailed {
            ticket: 3,
            sent: false,
        };
        assert_eq!(phase.begin_send(), None);
        assert_eq!(phase.begin_cancel(), Some(3));
    }
}
