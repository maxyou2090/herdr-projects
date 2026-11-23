//! Lines that keep saying the same thing every tick, said once.
//!
//! `Core::name_space`/`name_tab` answer with a note when a name did not land
//! ("the app does not show sp…: it keeps the name it has"), because a rename
//! that silently fails is worse than a noisy one. The ticker names every
//! coordinator's Space and tab on every tick, so that note was written to
//! `.ticker.log` ten times a minute and buried everything else (2026-11-23,
//! t-0091) — while the retry it describes is the part worth keeping.
//!
//! So: the same note under the same key is printed once, and a *changed* note
//! (or the note coming back after it stopped) is printed again. Keyed by the
//! thing being named, so two panes saying the same thing each say it once.

use std::cell::RefCell;
use std::collections::BTreeMap;

thread_local! {
    /// Key -> the notes last said under it. Thread-local: a test binary runs its
    /// scenarios side by side, and each starts with nothing said.
    static SAID: RefCell<BTreeMap<String, Vec<String>>> = const { RefCell::new(BTreeMap::new()) };
}

/// Prints each line of `lines` this key has not said already, and remembers
/// them. `lines` empty means the condition is gone: the key is forgotten, so the
/// same note said after it comes back is printed again.
pub fn say_once(key: &str, lines: &[String]) {
    if lines.is_empty() {
        SAID.with(|said| said.borrow_mut().remove(key));
        return;
    }
    let fresh: Vec<String> = SAID.with(|said| {
        let said = said.borrow();
        let previous = said.get(key);
        lines
            .iter()
            .filter(|line| previous.is_none_or(|previous| !previous.contains(line)))
            .cloned()
            .collect()
    });
    if fresh.is_empty() {
        return;
    }
    for line in &fresh {
        println!("{line}");
    }
    SAID.with(|said| said.borrow_mut().insert(key.to_string(), lines.to_vec()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_note_repeats_only_when_it_changes_or_comes_back() {
        say_once("a", &["the app does not show sp1".to_string()]);
        // Still true: quiet the second and third time.
        say_once("a", &["the app does not show sp1".to_string()]);
        say_once("a", &["the app does not show sp1".to_string()]);
        // Gone: the key is forgotten, so its return is news again.
        say_once("a", &[]);
        say_once("a", &["the app does not show sp1".to_string()]);
        // A different note under the same key is a different fact.
        let before = SAID.with(|said| said.borrow().get("a").cloned());
        say_once("a", &["the app still shows tab `x` instead of `y`".to_string()]);
        assert_ne!(before, SAID.with(|said| said.borrow().get("a").cloned()));
        // Two panes with the same words are two facts.
        say_once("b", &["the app does not show sp1".to_string()]);
        assert_eq!(SAID.with(|said| said.borrow().len()), 2);
    }
}
