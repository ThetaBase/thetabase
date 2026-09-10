//! What the review screen does, with no terminal anywhere near it.
//!
//! The Safety Layer's correctness rests on somebody actually *reading* what
//! they confirm. Reviewing five changes through the non-interactive commands
//! costs twenty invocations and three identifier copy-pastes, and a person
//! stops reading carefully somewhere around the third — so this is a safety
//! feature wearing a user-interface's clothes, and it is tested like one.
//!
//! The rules below carry that, and each has a test:
//!
//! * **One keystroke can confirm at most one change.** There is no bulk
//!   confirm, at any tier, ever.
//! * **The strongest gate costs more than a keypress.** Confirming a
//!   `shadow_validate` change means typing its id.
//! * **The queue is `triage`'s order and this never re-sorts it.**
//! * **Somebody without the role sees the queue and cannot answer it**, and the
//!   refusal names the role they would need.

use theta_proto::wire::{ChangeDiffWire, GateWire, ReviewBatchWire};

/// What the screen wants the caller to do.
///
/// Returned rather than performed, so every decision this module makes is
/// inspectable without a socket. A test asserting "this keystroke confirms
/// nothing" is asserting about a value, not about what did or did not reach a
/// server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Confirm(String),
    Promote(String),
    Reject { change_id: String, reason: String },
    Refresh,
    Quit,
}

/// Whether this reviewer may answer a gate.
///
/// Not a bool. "Can act" and "which role would let me" are different questions
/// and a refusal that cannot answer the second is a refusal somebody has to go
/// and research.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authority {
    MayReview,
    /// Read-only, and the role that would change that.
    Observer {
        needs_role: String,
    },
}

impl Authority {
    pub fn may_review(&self) -> bool {
        matches!(self, Authority::MayReview)
    }
}

/// What the screen is waiting for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Browsing,
    /// A destructive change on the strongest gate: the reviewer is typing its
    /// id. Not a modal asking "are you sure" — those train people to dismiss
    /// modals — but an act proportionate to dropping a column.
    ConfirmingById {
        change_id: String,
        typed: String,
    },
    /// Rejecting, and saying why. A reason is required: a refusal with no
    /// reason is one the agent cannot act on, and it will propose the same
    /// thing again.
    RejectingWithReason {
        change_id: String,
        typed: String,
    },
    /// Polling has stopped because nobody has touched the keyboard.
    ///
    /// An open review screen keeps an instance awake — answering a poll
    /// requires the instance to be running, so there is no server-side way to
    /// treat polling as idle. This is the client-side half: a terminal left
    /// open in a pane for a week stops asking.
    Dormant,
}

pub struct ReviewApp {
    batches: Vec<ReviewBatchWire>,
    /// Index into the flattened change list, in the order `triage` produced.
    selected: usize,
    mode: Mode,
    authority: Authority,
    message: Option<String>,
    /// Milliseconds since the last keypress, for [`Mode::Dormant`].
    idle_ms: u64,
}

/// How long a review screen keeps polling with nobody touching it.
///
/// Fifteen minutes. Long enough that somebody reading a large diff is not
/// interrupted, short enough that a terminal forgotten in a tmux pane stops
/// paying for an instance overnight.
pub const DORMANT_AFTER_MS: u64 = 15 * 60 * 1_000;

impl ReviewApp {
    pub fn new(batches: Vec<ReviewBatchWire>, authority: Authority) -> Self {
        Self {
            batches,
            selected: 0,
            mode: Mode::Browsing,
            authority,
            message: None,
            idle_ms: 0,
        }
    }

    /// Every pending change, in the order the server sent them.
    ///
    /// **Deliberately not sorted here.** `safety::triage` already produces a
    /// total order, and a second opinion about urgency rendered on top of the
    /// first is how a reviewer ends up reading them in an order nobody
    /// intended.
    pub fn changes(&self) -> Vec<&ChangeDiffWire> {
        self.batches
            .iter()
            .flat_map(|batch| batch.changes.iter())
            .collect()
    }

    pub fn selected(&self) -> Option<&ChangeDiffWire> {
        self.changes().into_iter().nth(self.selected)
    }

    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    pub fn authority(&self) -> &Authority {
        &self.authority
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Replace the queue after a refresh, keeping the reviewer where they were.
    ///
    /// Following the *change id* rather than the index: a change landing above
    /// the cursor would otherwise move the selection under somebody's hands,
    /// and the keystroke they were about to press would answer a different
    /// change from the one they were reading.
    pub fn update(&mut self, batches: Vec<ReviewBatchWire>) {
        let was = self.selected().map(|c| c.change_id.clone());
        self.batches = batches;

        self.selected = was
            .and_then(|id| self.changes().iter().position(|c| c.change_id == id))
            .unwrap_or(0);

        let count = self.changes().len();
        if count > 0 && self.selected >= count {
            self.selected = count - 1;
        }
    }

    /// Time passing with nobody typing.
    pub fn tick(&mut self, elapsed_ms: u64) {
        self.idle_ms = self.idle_ms.saturating_add(elapsed_ms);
        if self.idle_ms >= DORMANT_AFTER_MS && self.mode == Mode::Browsing {
            self.mode = Mode::Dormant;
            self.message = Some("Paused — press any key to resume.".into());
        }
    }

    /// Whether the caller should be polling for new changes.
    pub fn should_poll(&self) -> bool {
        self.mode != Mode::Dormant
    }

    /// Handle one keypress, and say what should happen.
    ///
    /// Returns **at most one** action. That is the structural half of "there is
    /// no bulk confirm": the signature makes a keystroke that answers two gates
    /// unrepresentable, so the rule cannot be broken by somebody adding a
    /// convenience later without changing this type.
    pub fn on_key(&mut self, key: Key) -> Option<Action> {
        self.idle_ms = 0;
        if self.mode == Mode::Dormant {
            // The keypress that wakes it is spent waking it. Otherwise the
            // first thing somebody types after a break also answers a gate.
            self.mode = Mode::Browsing;
            self.message = None;
            return Some(Action::Refresh);
        }

        match std::mem::replace(&mut self.mode, Mode::Browsing) {
            Mode::Dormant => unreachable!("handled above"),

            Mode::ConfirmingById { change_id, typed } => self.typing(key, change_id, typed, true),
            Mode::RejectingWithReason { change_id, typed } => {
                self.typing(key, change_id, typed, false)
            }

            Mode::Browsing => self.browsing(key),
        }
    }

    fn browsing(&mut self, key: Key) -> Option<Action> {
        let count = self.changes().len();
        match key {
            Key::Down | Key::Char('j') => {
                if count > 0 {
                    self.selected = (self.selected + 1).min(count - 1);
                }
                None
            }
            Key::Up | Key::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                None
            }
            Key::Char('q') | Key::Esc => Some(Action::Quit),
            Key::Char('r') if self.selected().is_none() => Some(Action::Refresh),

            Key::Char('c') => self.begin_confirm(),
            Key::Char('p') => self.act(Action::Promote),
            Key::Char('r') => self.begin_reject(),

            _ => None,
        }
    }

    /// Refuse without authority, naming the role that would help.
    ///
    /// Checked in one place rather than at each key, so a new action cannot be
    /// added that forgets to ask.
    fn require_authority(&mut self) -> bool {
        match &self.authority {
            Authority::MayReview => true,
            Authority::Observer { needs_role } => {
                self.message = Some(format!(
                    "You can read this queue but not answer it. Answering a gate \
                     needs the `{needs_role}` role."
                ));
                false
            }
        }
    }

    fn act(&mut self, make: impl Fn(String) -> Action) -> Option<Action> {
        if !self.require_authority() {
            return None;
        }
        let id = self.selected()?.change_id.clone();
        Some(make(id))
    }

    fn begin_confirm(&mut self) -> Option<Action> {
        if !self.require_authority() {
            return None;
        }
        let change = self.selected()?;
        let id = change.change_id.clone();

        match change.gate {
            // The strongest gate is not answerable by a keypress. Four seconds
            // of typing against dropping a column from a protected branch, and
            // it makes the confirmation an act rather than a reflex.
            GateWire::ShadowValidate => {
                self.mode = Mode::ConfirmingById {
                    change_id: id,
                    typed: String::new(),
                };
                self.message = Some("Type the change id to confirm, or Esc.".into());
                None
            }
            GateWire::Confirm => Some(Action::Confirm(id)),
            // Nothing to confirm. Saying so beats a silent no-op, which reads
            // as a broken keyboard.
            GateWire::AutoApply => {
                self.message = Some("That change was applied without a gate.".into());
                None
            }
        }
    }

    fn begin_reject(&mut self) -> Option<Action> {
        if !self.require_authority() {
            return None;
        }
        let id = self.selected()?.change_id.clone();
        self.mode = Mode::RejectingWithReason {
            change_id: id,
            typed: String::new(),
        };
        self.message = Some("Why? The agent will read this. Enter to send, Esc to stop.".into());
        None
    }

    /// Shared by both typing modes.
    fn typing(
        &mut self,
        key: Key,
        change_id: String,
        mut typed: String,
        is_confirm: bool,
    ) -> Option<Action> {
        match key {
            Key::Esc => {
                self.message = None;
                None
            }
            Key::Backspace => {
                typed.pop();
                self.restore_typing(change_id, typed, is_confirm);
                None
            }
            Key::Char(c) => {
                typed.push(c);
                self.restore_typing(change_id, typed, is_confirm);
                None
            }
            Key::Enter if is_confirm => {
                if typed.trim() == change_id {
                    Some(Action::Confirm(change_id))
                } else {
                    self.message = Some("That is not the change id. Nothing was confirmed.".into());
                    None
                }
            }
            Key::Enter => {
                let reason = typed.trim().to_string();
                if reason.is_empty() {
                    // A rejection with no reason is one the agent cannot act
                    // on, so it proposes the same thing again and the reviewer
                    // sees it twice.
                    self.message = Some("A rejection needs a reason.".into());
                    self.restore_typing(change_id, typed, is_confirm);
                    None
                } else {
                    Some(Action::Reject { change_id, reason })
                }
            }
            _ => {
                self.restore_typing(change_id, typed, is_confirm);
                None
            }
        }
    }

    fn restore_typing(&mut self, change_id: String, typed: String, is_confirm: bool) {
        self.mode = match is_confirm {
            true => Mode::ConfirmingById { change_id, typed },
            false => Mode::RejectingWithReason { change_id, typed },
        };
    }

    /// Report the outcome of an action back to the screen.
    pub fn report(&mut self, message: impl Into<String>) {
        self.message = Some(message.into());
    }
}

/// The keys this screen understands.
///
/// Its own type rather than crossterm's, so the state machine can be tested
/// without a terminal crate in the test's dependency closure — and so the set
/// of keys that mean anything is legible in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Up,
    Down,
    Enter,
    Esc,
    Backspace,
}
