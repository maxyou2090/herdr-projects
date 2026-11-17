//! What an agent is doing, read from the screen core already keeps, so
//! `hp-agent` can report the middle of a turn and not only its ends.
//!
//! herdr-core knows a program's state only from the OSC 7501 records it prints,
//! and most harnesses print none: such a pane reads `idle` from the moment the
//! wrapper starts it until the moment it exits, whatever the agent is doing.
//! The harnesses' screens, however, are in core already (`hc read`), and this
//! binary can read them (`prompt_box`, `trust_screen`): the wrapper polls its
//! own pane and reports what it can prove. See `docs/herdr-core-port.md` §8.1
//! for why this is a read of core's screen rather than a PTY relay inside
//! `hp-agent`.
//!
//! The rule is confidence, never coverage. A state the user is sent to look at
//! and does not find is worse than a state that was never reported:
//!
//! - **`blocked`** needs a line only a trust or review screen draws, on a
//!   screen whose input box is not empty (a transcript quoting the dialog sits
//!   above an empty box, so a quote never counts), on two reads in a row.
//! - **`working`** needs the screen to have changed since the previous read,
//!   and a layout this binary can read.
//! - **`idle`** needs an empty input box and three unchanged reads. A turn that
//!   is running either prints output or redraws its own timer, so silence at an
//!   empty prompt is a finished turn.
//! - Everything else concludes nothing and reports nothing.
//!
//! This module has no dependency beyond `prompt_box` and `trust_screen`, so the
//! `hp-agent` binary compiles it without the rest of the crate (it is declared
//! there with `#[path]`).

use crate::prompt_box::{self, Draft};
use crate::trust_screen;

/// Seconds between two reads of a pane's screen.
pub const POLL_SECS: u64 = 2;

/// Reads a `blocked` or `working` conclusion must hold before it is reported.
/// Two reads stop a half-drawn screen from reporting a dialog that is not
/// there yet.
const CONFIRM_READS: u32 = 2;

/// Unchanged reads at an empty input box before the turn counts as over.
/// `QUIET_READS * POLL_SECS` is the silence a running harness never shows: it
/// is either printing or redrawing its elapsed-time line.
const QUIET_READS: u32 = 3;

/// Reads after a start during which nothing is concluded: the harness is still
/// drawing its first screen, and that drawing is not a turn.
const WARMUP_READS: u32 = 3;

/// A state the wrapper can report from a screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// At an empty prompt: the turn is over, a prompt can be sent.
    Idle,
    /// A turn is running.
    Working,
    /// A dialog waits for a person.
    Blocked,
}

impl State {
    /// The word core uses for this state in a 7501 record.
    pub fn name(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Working => "working",
            State::Blocked => "blocked",
        }
    }
}

/// Reads a pane's screens in order and says when its state changed.
#[derive(Debug, Clone)]
pub struct Watcher {
    kind: String,
    /// The previous screen's text, styling dropped: the activity test.
    last: String,
    reads: u32,
    quiet: u32,
    blocked: u32,
    state: State,
}

impl Watcher {
    /// A watcher for an agent of `kind`, started as `idle` — what the wrapper
    /// reports at start, before any turn.
    pub fn new(kind: &str) -> Self {
        Watcher { kind: kind.to_string(), last: String::new(), reads: 0, quiet: 0, blocked: 0, state: State::Idle }
    }

    /// The state this watcher last reported.
    #[cfg(test)]
    pub fn state(&self) -> State {
        self.state
    }

    /// One read of the pane's screen (`hc read --json`'s `text`): the state to
    /// report now, or `None` when nothing is different or this read cannot
    /// conclude anything.
    pub fn observe(&mut self, screen: &str) -> Option<State> {
        let plain = prompt_box::plain(screen);
        let changed = !self.last.is_empty() && plain != self.last;
        self.last = plain;
        self.reads += 1;

        if trust_screen::detect(&self.kind, screen).is_some() {
            self.blocked += 1;
            self.quiet = 0;
            if self.blocked < CONFIRM_READS || self.state == State::Blocked {
                return None;
            }
            self.state = State::Blocked;
            return Some(State::Blocked);
        }
        self.blocked = 0;

        // A layout this binary cannot read (a scrolled view, an unknown kind, a
        // dialog it has no phrase for) says nothing about the turn, and a fresh
        // pane says nothing either.
        let draft = prompt_box::check(&self.kind, screen);
        if self.reads <= WARMUP_READS || draft == Draft::Unknown {
            self.quiet = 0;
            return None;
        }

        if changed {
            self.quiet = 0;
            if self.state == State::Working {
                return None;
            }
            self.state = State::Working;
            return Some(State::Working);
        }

        self.quiet += 1;
        if self.quiet < QUIET_READS || draft != Draft::Empty || self.state == State::Idle {
            return None;
        }
        self.state = State::Idle;
        Some(State::Idle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/prompt_box/{name}.ansi", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    /// A screen with the agent mid-turn: it is drawing output (and a ticking
    /// elapsed time), and its input box is still empty underneath.
    fn busy(base: &str, step: usize) -> String {
        format!("⏺ Reading src/main.rs ({step}s)\n\n{base}")
    }

    /// Feeds `screens` and collects every state reported, with the read it
    /// came on.
    fn run(kind: &str, screens: &[String]) -> Vec<(usize, State)> {
        let mut watcher = Watcher::new(kind);
        let mut out = Vec::new();
        for (i, screen) in screens.iter().enumerate() {
            if let Some(state) = watcher.observe(screen) {
                out.push((i + 1, state));
            }
        }
        out
    }

    #[test]
    fn a_turn_reports_working_then_idle() {
        // Warm-up reads the empty prompt, then output arrives and runs, then
        // the turn ends and the screen stops changing.
        let empty = fixture("claude-empty");
        let mut screens = vec![empty.clone(); 4];
        screens.extend((1..=3).map(|step| busy(&empty, step)));
        screens.extend(vec![fixture("claude-empty-after-turn"); 4]);
        assert_eq!(run("claude", &screens), vec![(5, State::Working), (11, State::Idle)]);
    }

    #[test]
    fn a_trust_screen_reports_blocked_and_the_answer_resumes_the_turn() {
        let empty = fixture("codex-empty");
        let mut screens = vec![empty.clone(); 4];
        screens.push(busy(&empty, 1));
        screens.extend(vec![fixture("codex-trust"); 2]);
        // The person answers it: the turn runs on, then ends.
        screens.extend((2..=3).map(|step| busy(&empty, step)));
        screens.extend(vec![empty; 4]);
        assert_eq!(run("codex", &screens), vec![(5, State::Working), (7, State::Blocked), (8, State::Working), (13, State::Idle)]);
    }

    #[test]
    fn a_dialog_is_only_reported_once_it_holds_for_two_reads() {
        // A screen drawn halfway through can show the phrase for one read.
        let empty = fixture("codex-empty");
        let mut screens = vec![empty.clone(); 4];
        screens.push(busy(&empty, 1));
        screens.push(fixture("codex-trust"));
        screens.push(busy(&empty, 2));
        assert_eq!(run("codex", &screens), vec![(5, State::Working)]);
    }

    #[test]
    fn a_quiet_pane_stays_silent_and_an_unknown_layout_is_not_guessed() {
        // Nothing is reported while an idle prompt simply sits there: `idle`
        // is what the wrapper said at start.
        let idle = vec![fixture("pi-empty"); 10];
        assert!(run("pi", &idle).is_empty());
        // A layout this binary cannot parse (an unknown kind, a scrolled view)
        // concludes nothing, in either direction.
        let mut screens = vec!["$ hp-agent kimi\n".to_string(); 8];
        screens.push("...\n".to_string());
        assert!(run("kimi", &screens).is_empty());
        // A draft in the box while the turn runs is still a running turn, and a
        // draft alone never reports `idle` (someone is typing).
        let empty = fixture("claude-empty");
        let mut screens = vec![empty.clone(); 4];
        screens.push(busy(&empty, 1));
        screens.extend(vec![fixture("claude-draft"); 6]);
        assert_eq!(run("claude", &screens), vec![(5, State::Working)]);
    }

    #[test]
    fn the_states_hold_their_turn_until_the_screen_says_otherwise() {
        // A single changed read does not report `idle` again: the turn ends
        // only after the quiet window, and `working` is reported once.
        let empty = fixture("codex-empty");
        let mut watcher = Watcher::new("codex");
        for _ in 0..4 {
            assert_eq!(watcher.observe(&empty), None);
        }
        assert_eq!(watcher.state(), State::Idle);
        assert_eq!(watcher.observe(&busy(&empty, 1)), Some(State::Working));
        for step in 2..6 {
            assert_eq!(watcher.observe(&busy(&empty, step)), None, "step {step}");
        }
        for read in 1..=3 {
            assert_eq!(watcher.observe(&empty), None, "quiet read {read}");
        }
        assert_eq!(watcher.observe(&empty), Some(State::Idle));
        assert_eq!(watcher.state(), State::Idle);
        // The same screen, read again: nothing new to say.
        assert_eq!(watcher.observe(&empty), None);
    }

    #[test]
    fn every_state_names_the_word_core_reads() {
        assert_eq!((State::Idle.name(), State::Working.name(), State::Blocked.name()), ("idle", "working", "blocked"));
    }
}
