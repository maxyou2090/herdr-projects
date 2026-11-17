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
//! core supplies the native session id (the `hp_session` label, read back as
//! `Agent::agent_session`) but it carries no resume table, so nothing upstream
//! keeps these flags current: when a harness renames its resume flag, edit the
//! table here and the test that pins it at the bottom of this file. A kind that
//! is not in the table resumes nothing — `open` starts that conversation anew
//! instead of guessing a flag.

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
pub fn resume_args(kind: &str, id: &str) -> Option<Vec<String>> {
    if id.is_empty() || id.starts_with('-') {
        return None;
    }
    let args: Vec<String> = match kind {
        "claude" | "cursor" | "grok" | "devin" | "droid" | "qodercli" | "qwen" | "hermes" => vec!["--resume".into(), id.into()],
        "codex" => vec!["resume".into(), id.into()],
        "omp" | "copilot" => vec![format!("--resume={id}")],
        "pi" | "opencode" | "kimi" | "kilo" => vec!["--session".into(), id.into()],
        "agy" | "letta" => vec!["--conversation".into(), id.into()],
        "mastracode" => vec!["--thread".into(), id.into()],
        _ => return None,
    };
    Some(args)
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
}
