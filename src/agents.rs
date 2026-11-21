//! What the binary knows about launching agents: the 24 harness CLIs it types
//! into a terminal, and the flag each of them needs to resume a native session.
//!
//! `KINDS` is this binary's own list. herdr-core has no notion of an agent
//! kind — it keeps a terminal and reads a program's own status (OSC 7501) — and
//! `hp-agent <kind>` is what turns a kind into a program, so this list is the
//! set `agent start` can write into a pane.
//!
//! The resume table in [`resume_args`] is **maintained by hand**. It came from
//! Herdr 1's session-state page (herdr.dev, "Native agent session restore",
//! 0.9.1) when this binary was a Herdr 1 plugin. In the herdr-core port the
//! core supplied the native session id (the `hp_session` label, read back as
//! `Agent::agent_session`) but it carries no resume table, so nothing upstream
//! keeps these flags current: when a harness renames its resume flag, edit the
//! table here and the test at the bottom of this file. A kind that is not in
//! the table resumes nothing — `open` starts that conversation anew instead of
//! guessing a flag.
//!
//! [`fresh_session_args`] is the other half and is why a coordinator survives a
//! closed Space. Core never writes `hp_session`, so after the herdr-core port no
//! record ever had a session id to resume (`docs/herdr-core-port.md`, t-0082).
//! The fix is to stop waiting for core to report one: `hp` names the session
//! itself, writes the id into the coordinator record **before** the agent starts,
//! and passes it on every launch. A harness that answers "open the session with
//! this id, create it when it does not exist" can then be resumed by id alone —
//! and that is one flag, not two: the `--session-id <id>` of [`fresh_session_args`]
//! opens an existing session or creates it, so the first `open` and every later
//! one use the same argument.
//!
//! Each entry of that table is a flag verified against the harness's own
//! `--help` on this machine, never from memory: pi's `--session-id <id>` does
//! exactly the "existing or new" thing. Harnesses verified to have no such flag
//! (codex, agy) and harnesses not checked at all stay out of it, which keeps
//! today's behaviour for them: no id recorded, a fresh conversation each time.
//!
//! pi needs one further distinction, in [`resume_args`]: **what the recorded
//! value is** picks the flag, because the two are not interchangeable. An id
//! `hp` minted is opened with `--session-id` (existing or new); a path a record
//! from before the port holds — pi's sessions are
//! `/Users/…/2026-11-06T03-59-43-694Z_01a1af03.jsonl` — is opened with
//! `--session`. `pi --session hp-<12 hex>` is not a resume at all: pi answers
//! "No session found matching" and exits when that session file does not exist
//! yet, which `hp-agent` had already reported as started (2026-11-21, my-net).

pub const KINDS: [&str; 24] = [
    "pi", "claude", "codex", "gemini", "cursor", "devin", "agy", "cline", "omp", "mastracode", "opencode", "copilot", "kimi", "kiro", "droid",
    "amp", "grok", "hermes", "kilo", "qodercli", "qwen", "letta", "maki", "muse",
];

pub fn is_kind(kind: &str) -> bool {
    KINDS.contains(&kind)
}

/// The arguments that resume a native session `id` for `kind`, or `None` for
/// a kind whose resume flag this table does not name (see the module comment:
/// these are each harness CLI's own flag, kept by hand, not a core-provided
/// table).
///
/// pi is the one kind whose flag depends on the value: an id `hp` minted (see
/// [`is_minted_session`]) is opened with `--session-id`, existing or new, and a
/// path from a record made before the port with `--session`.
pub fn resume_args(kind: &str, id: &str) -> Option<Vec<String>> {
    if id.is_empty() || id.starts_with('-') {
        return None;
    }
    let args: Vec<String> = match kind {
        "claude" | "cursor" | "grok" | "devin" | "droid" | "qodercli" | "qwen" | "hermes" => vec!["--resume".into(), id.into()],
        "codex" => vec!["resume".into(), id.into()],
        "omp" | "copilot" => vec![format!("--resume={id}")],
        "opencode" | "kimi" | "kilo" => vec!["--session".into(), id.into()],
        // `pi --session hp-<12 hex>` says "No session found matching" while the
        // session file does not exist yet (pi writes it after the first turn),
        // exits, and leaves the pane at a shell — while `hp-agent` had already
        // reported a status, so `open` believed it started (2026-11-21).
        "pi" => vec![if is_minted_session(id) { "--session-id".into() } else { "--session".into() }, id.into()],
        "agy" | "letta" => vec!["--conversation".into(), id.into()],
        "mastracode" => vec!["--thread".into(), id.into()],
        _ => return None,
    };
    Some(args)
}

/// Whether a recorded session value is an id `hp` named itself
/// ([`session_id`], `hp-<12 hex>`) rather than a path from a record made before
/// the port (pi's are `/Users/…/2026-11-06T03-59-43-694Z_01a1af03.jsonl`). Only
/// the shape answers: an id never holds a separator, a path always does.
pub fn is_minted_session(value: &str) -> bool {
    value.starts_with("hp-") && !value.contains('/') && !value.contains('\\')
}

/// The arguments that open the session `id` for `kind`, creating it when it does
/// not exist yet — the flag a harness uses to accept an id from its caller.
/// `None` for every kind whose CLI has no such flag (see the module comment:
/// each entry is verified by hand).
///
/// "Open, creating it when it is missing" is also what makes it the resume flag
/// for such an id ([`resume_args`]): one id, one argument, first launch or
/// hundredth.
pub fn fresh_session_args(kind: &str, id: &str) -> Option<Vec<String>> {
    if id.is_empty() || id.starts_with('-') {
        return None;
    }
    let args: Vec<String> = match kind {
        // `pi --session-id <id>`: the current directory's session with that id,
        // created when it is not there yet. Needs letters, digits, `.`, `_`,
        // `-`, and a letter or digit at both ends — [`session_id`] satisfies it.
        "pi" => vec!["--session-id".into(), id.into()],
        _ => return None,
    };
    Some(args)
}

/// The session id `hp` hands a coordinator whose harness accepts one:
/// `hp-<12 hex>`. Made from the nanosecond clock, this process's pid and a
/// counter through a digest, so two coordinators started in the same second —
/// or in the same process — differ, and no crate is added for it.
pub fn session_id() -> String {
    static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
    let calls = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mixed = format!("{}:{nanos}:{calls}", std::process::id());
    format!("hp-{}", &crate::thread::sha256_hex(mixed.as_bytes())[..12])
}

/// Whether `value` looks like a model name: `opus`, `gpt-5.5`,
/// `anthropic/claude-sonnet-5`, `claude-opus-5-5[1m]`. Never a flag.
fn is_model_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.starts_with('-')
        && value.chars().all(|c| c.is_ascii_alphanumeric() || "._-/:@+[]".contains(c))
}

/// Splits a thread's or coordinator's `--agent-arg` values into the model
/// flags they may carry and everything else, read as pairs: `--model NAME`
/// and `--model=NAME` for every kind, plus `-m NAME` for Codex. Any other
/// launch flag belongs in the user's own `*_agent_args` safety settings, so
/// an agent can never widen another agent's powers.
pub fn split_model_args(kind: &str, args: &[String]) -> (Vec<String>, Vec<String>) {
    let (mut allowed, mut refused) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let is_flag = arg == "--model" || (kind == "codex" && arg == "-m");
        if is_flag && args.get(i + 1).is_some_and(|v| is_model_name(v)) {
            allowed.extend([arg.clone(), args[i + 1].clone()]);
            i += 2;
            continue;
        }
        if arg.strip_prefix("--model=").is_some_and(is_model_name) {
            allowed.push(arg.clone());
        } else {
            refused.push(arg.clone());
        }
        i += 1;
    }
    (allowed, refused)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn agent_args_carry_only_a_model_flag() {
        for kind in ["claude", "codex", "gemini", "opencode", "cursor", "copilot"] {
            for args in [&["--model", "opus"][..], &["--model=gpt-5.5"], &["--model", "anthropic/claude-sonnet-5"], &["--model", "claude-opus-5-5[1m]"], &[]] {
                assert_eq!(split_model_args(kind, &strings(args)), (strings(args), vec![]), "{kind} {args:?}");
            }
        }
        assert_eq!(split_model_args("codex", &strings(&["-m", "gpt-5.5"])), (strings(&["-m", "gpt-5.5"]), vec![]));
        assert_eq!(split_model_args("claude", &strings(&["-m", "opus"])), (vec![], strings(&["-m", "opus"])));

        let refused = |args: &[&str]| split_model_args("claude", &strings(args));
        assert_eq!(refused(&["--dangerously-skip-permissions"]), (vec![], strings(&["--dangerously-skip-permissions"])));
        assert_eq!(split_model_args("codex", &strings(&["--yolo"])), (vec![], strings(&["--yolo"])));
        assert_eq!(refused(&["--model"]), (vec![], strings(&["--model"])));
        assert_eq!(refused(&["--model", "--foo"]), (vec![], strings(&["--model", "--foo"])));
        assert_eq!(refused(&["--model", "x", "--extra"]), (strings(&["--model", "x"]), strings(&["--extra"])));
        assert_eq!(refused(&["--model="]), (vec![], strings(&["--model="])));
        assert_eq!(refused(&["--model=-x"]), (vec![], strings(&["--model=-x"])));
        assert_eq!(refused(&["--model", "a b"]), (vec![], strings(&["--model", "a b"])));
        assert_eq!(refused(&["--model", "$(id)"]), (vec![], strings(&["--model", "$(id)"])));
        assert_eq!(refused(&["opus"]), (vec![], strings(&["opus"])));
    }

    #[test]
    fn a_fresh_session_flag_is_only_the_kinds_this_table_holds() {
        assert_eq!(fresh_session_args("pi", "hp-0123456789ab").unwrap(), ["--session-id", "hp-0123456789ab"]);
        // Verified to have no caller-chosen id on start: they must stay out.
        for kind in ["codex", "agy", "claude", "gemini", "opencode"] {
            assert_eq!(fresh_session_args(kind, "hp-0123456789ab"), None, "{kind}");
        }
        assert_eq!(fresh_session_args("pi", ""), None);
        assert_eq!(fresh_session_args("pi", "--dangerous"), None);
    }

    /// pi accepts letters, digits, `.`, `_`, `-` and demands a letter or digit at
    /// both ends: an id outside that rule would make `open` fail to start at all.
    #[test]
    fn a_generated_session_id_is_one_pi_accepts_and_differs_each_time() {
        let first = session_id();
        let second = session_id();
        assert!(first.starts_with("hp-"), "{first}");
        assert_eq!(first.len(), "hp-".len() + 12, "{first}");
        assert!(first.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'), "{first}");
        assert!(first.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()), "{first}");
        assert!(first.chars().last().is_some_and(|c| c.is_ascii_alphanumeric()), "{first}");
        assert_ne!(first, second, "two coordinators in the same second must not share an id");
    }

    #[test]
    fn resume_arguments_are_the_harness_flags_this_table_holds() {
        assert_eq!(resume_args("claude", "abc").unwrap(), ["--resume", "abc"]);
        assert_eq!(resume_args("codex", "abc").unwrap(), ["resume", "abc"]);
        assert_eq!(resume_args("opencode", "abc").unwrap(), ["--session", "abc"]);
        assert_eq!(resume_args("copilot", "abc").unwrap(), ["--resume=abc"]);
        assert_eq!(resume_args("gemini", "abc"), None);
        assert_eq!(resume_args("claude", ""), None);
        assert_eq!(resume_args("claude", "--dangerous"), None);
        assert!(is_kind("claude") && !is_kind("chatgpt"));
        assert_eq!(KINDS.len(), 24);
    }

    /// pi's two shapes, and only pi's: an id `hp` minted is `--session-id` (open
    /// it, create it when missing), the path a record from before the port holds
    /// is `--session`. Getting this backwards is the 2026-11-21 my-net failure:
    /// `pi --session hp-<12 hex>` finds no session, exits, and the pane is back
    /// at a shell while `open` reports a coordinator that started.
    #[test]
    fn pi_resumes_a_minted_id_by_id_and_an_old_path_by_path() {
        assert_eq!(resume_args("pi", "hp-0123456789ab").unwrap(), ["--session-id", "hp-0123456789ab"]);
        let path = "/Users/maxyou/.pi/agent/sessions/--Users-maxyou-.herdr-projects-my-net--/2026-11-06T03-59-43-694Z_01a1af03.jsonl";
        assert_eq!(resume_args("pi", path).unwrap(), ["--session", path]);
        assert_eq!(resume_args("pi", "old.jsonl").unwrap(), ["--session", "old.jsonl"], "no separator, but a file name is a path");
        assert!(!is_minted_session(path) && !is_minted_session("old.jsonl"), "a path is never an id");
        assert!(is_minted_session("hp-0123456789ab") && is_minted_session(&session_id()), "and an id is what hp makes");
        // The kinds that share pi's flag stay on it: their ids are not hp's.
        for kind in ["opencode", "kimi", "kilo"] {
            assert_eq!(resume_args(kind, "hp-0123456789ab").unwrap(), ["--session", "hp-0123456789ab"], "{kind}");
        }
        assert_eq!(resume_args("pi", "hp-0123456789ab"), fresh_session_args("pi", "hp-0123456789ab"), "one flag, first launch or later one");
    }
}
