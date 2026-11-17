//! Getting a thread's brief line to its agent, once, and only when it is
//! really ready. Every sender (the ticker's passes, `thread brief`, adoption)
//! goes through `deliver`.
//!
//! - Ready means idle or done, the input box readable and empty, and (for the
//!   ticker) the state and screen unchanged for `SETTLE_SECS`: a start-up
//!   screen that reads idle for a moment does not get the text.
//! - Sent means herdr saw the agent start working (`--wait --until working`),
//!   not that `agent prompt` exited 0.
//! - After any try that may have typed, the screen decides: the line in the
//!   conversation is delivered, the line left in the box gets Enter, an empty
//!   box with no line is typed again. Anything else waits. Text is never
//!   typed on top of a copy.
//! - `MAX_ATTEMPTS` tries, then one inbox item; `thread brief` still sends it.

use anyhow::Result;

use crate::core::{Agent, Core};
use crate::project::{self, Project};
use crate::prompt_box;
use crate::thread::{self, Thread};
use crate::inbox;

pub const SETTLE_SECS: i64 = 3;
pub const MAX_ATTEMPTS: u32 = 3;
/// A claim older than this belongs to a sender that died.
pub const CLAIM_SECS: i64 = 60;

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// herdr saw the agent start on it, or the screen shows it was taken.
    Delivered,
    /// Not yet; the reason, for `thread brief`.
    Waiting(String),
    /// Another sender is at it right now, or it was delivered already.
    Taken,
    /// The tries ran out: an inbox item was written.
    Stuck,
    /// The pane shows a trust screen (this line of it): nothing is typed, as
    /// its Enter would accept the screen.
    TrustScreen(&'static str),
}

/// Who is sending: the ticker waits for a settled screen and stops after
/// `MAX_ATTEMPTS`; a person (`thread brief`) or adoption does neither.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sender {
    Ticker,
    Manual,
}

/// The part of the launch line that is always on one screen line.
fn marker(slug: &str, id: &str) -> String {
    format!(".herdr-project/{slug}-{id}/brief.md")
}

fn squeeze(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Claims the brief under the project lock; `None` when it is not pending
/// any more or another sender's claim is fresh.
fn claim(project: &Project, id: &str, now: jiff::Timestamp) -> Result<Option<Thread>> {
    let mut claimed = None;
    thread::update(project, id, |t| {
        let fresh = !t.brief_claimed.is_empty() && thread::seconds_since(&t.brief_claimed, now) < CLAIM_SECS;
        if t.prompt_pending && !fresh {
            t.brief_claimed = project::now();
            claimed = Some(t.clone());
        }
    })?;
    Ok(claimed)
}

/// One delivery step for a thread whose brief is pending, `agent` being its
/// agent as just listed.
pub fn deliver(project: &Project, herdr: &Core, record: &Thread, agent: &Agent, sender: Sender) -> Result<Outcome> {
    let now = jiff::Timestamp::now();
    let Some(mut t) = claim(project, &record.id, now)? else {
        return Ok(Outcome::Taken);
    };
    let outcome = step(project, herdr, &mut t, agent, sender, now);
    let delivered = matches!(outcome, Ok(Outcome::Delivered));
    thread::update(project, &t.id, |r| {
        r.brief_claimed.clear();
        r.brief_attempts = t.brief_attempts;
        r.brief_seen = t.brief_seen.clone();
        r.brief_seen_at = t.brief_seen_at.clone();
        r.brief_stuck = t.brief_stuck;
        if delivered {
            r.prompt_pending = false;
            r.brief_stuck = false;
        }
    })?;
    outcome
}

fn step(project: &Project, herdr: &Core, t: &mut Thread, agent: &Agent, sender: Sender, now: jiff::Timestamp) -> Result<Outcome> {
    let state = agent.agent_status.as_str();
    if matches!(state, "blocked" | "") {
        t.brief_seen.clear();
        return Ok(Outcome::Waiting(format!("the agent is {}", if state.is_empty() { "not detected" } else { state })));
    }
    let kind = if agent.agent.is_empty() { t.agent.as_str() } else { agent.agent.as_str() };
    let ready = agent.ready_for_brief(kind);
    // `unknown` without an open composer, or nothing typed yet and not
    // ready: nothing to look at.
    if (state == "unknown" || t.brief_attempts == 0) && !ready {
        t.brief_seen.clear();
        return Ok(Outcome::Waiting(format!("the agent is {state}")));
    }
    let screen = match herdr.agent_screen(&t.pane_id) {
        Ok(screen) => screen,
        Err(error) => return Ok(Outcome::Waiting(format!("its screen could not be read: {error}"))),
    };
    if let Some(phrase) = crate::trust_screen::detect(kind, &screen) {
        t.brief_seen.clear();
        return Ok(Outcome::TrustScreen(phrase));
    }
    let known = prompt_box::knows(kind);
    let in_box = prompt_box::box_text(kind, &screen);
    let line = squeeze(&marker(&project.slug, &t.id));
    let shown = squeeze(&prompt_box::plain(&screen)).contains(&line);

    if t.brief_attempts > 0 {
        let in_box_has_line = in_box.as_deref().is_some_and(|text| squeeze(text).contains(&line));
        // On screen and not in the box: it was taken. An agent whose box
        // cannot be read counts only once it works.
        let taken = shown && !in_box_has_line && (in_box.as_deref() == Some("") || (!known && !ready));
        match in_box.as_deref() {
            _ if taken => return Ok(Outcome::Delivered),
            // Left in the box: submit it, never type it again.
            Some(_) if in_box_has_line && ready => {
                if let Some(stuck) = out_of_tries(project, t, sender)? {
                    return Ok(stuck);
                }
                bump(project, t)?;
                return Ok(match herdr.agent_send_keys(&t.pane_id, &["enter".to_string()]) {
                    Ok(()) => Outcome::Waiting("pressed Enter on the brief left in its input box".into()),
                    Err(error) => Outcome::Waiting(format!("could not press Enter: {error}")),
                });
            }
            // An empty box and no line anywhere: it was dropped.
            Some("") if ready => {}
            // A box this binary cannot read: only a person decides.
            None if !known => {
                if sender == Sender::Manual {
                    if shown {
                        // They looked; the line is on screen.
                        return Ok(Outcome::Delivered);
                    }
                } else {
                    return give_up(project, t, "its input box cannot be read to check an earlier try");
                }
            }
            _ => return Ok(Outcome::Waiting(format!("the agent is {state} and an earlier try may still be in its input box"))),
        }
    } else if known && in_box.as_deref() != Some("") {
        t.brief_seen.clear();
        let why = if in_box.is_none() { "its input box is not on screen (a menu or start-up screen?)" } else { "its input box holds text" };
        return Ok(Outcome::Waiting(why.into()));
    }

    if let Some(stuck) = out_of_tries(project, t, sender)? {
        return Ok(stuck);
    }
    if sender == Sender::Ticker {
        let key = format!("{}:{:x}", agent.state_change_seq, fnv(&prompt_box::plain(&screen)));
        if t.brief_seen != key {
            t.brief_seen = key;
            t.brief_seen_at = project::now();
            return Ok(Outcome::Waiting("settling".into()));
        }
        if thread::seconds_since(&t.brief_seen_at, now) < SETTLE_SECS {
            return Ok(Outcome::Waiting("settling".into()));
        }
    }
    bump(project, t)?;
    match herdr.agent_prompt_confirmed(&t.pane_id, &thread::launch_prompt(&project.slug, &t.id)) {
        Ok(()) => Ok(Outcome::Delivered),
        Err(error) if crate::core::refused_before_typing(&error) => {
            t.brief_attempts -= 1;
            Ok(Outcome::Waiting(format!("{error}")))
        }
        Err(error) => Ok(Outcome::Waiting(format!("not confirmed ({error}); the screen is checked before any retry"))),
    }
}

/// Counts a try before it is made, so a sender that dies mid-way still
/// leaves the record saying text may be on screen.
/// A retry waits for a fresh settle.
fn bump(project: &Project, t: &mut Thread) -> Result<()> {
    t.brief_attempts += 1;
    t.brief_seen.clear();
    let attempts = t.brief_attempts;
    thread::update(project, &t.id, |r| r.brief_attempts = attempts)?;
    Ok(())
}

fn out_of_tries(project: &Project, t: &mut Thread, sender: Sender) -> Result<Option<Outcome>> {
    if sender == Sender::Ticker && t.brief_attempts >= MAX_ATTEMPTS {
        return give_up(project, t, &format!("{MAX_ATTEMPTS} tries were not confirmed")).map(Some);
    }
    Ok(None)
}

fn give_up(project: &Project, t: &mut Thread, why: &str) -> Result<Outcome> {
    if !t.brief_stuck {
        t.brief_stuck = true;
        let summary = format!(
            "{}: its brief did not get through ({why}). `thread read {} {}` shows the pane; `thread brief` sends it once the input box is empty",
            t.id, project.slug, t.id
        );
        inbox::write_in(project, &inbox::lane_of_thread(project, &t.id), "thread-state", &t.id, "brief not delivered", &summary, "")?;
    }
    Ok(Outcome::Stuck)
}

/// A small stable hash of the screen text (FNV-1a).
fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3))
}
