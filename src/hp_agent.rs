//! `hp-agent <command> [args...]`: run a coding agent and tell herdr-core what
//! it is doing over OSC 7501, the only status channel herdr-core reads.
//!
//! herdr-core draws nothing and keeps no agent registry: a program's status is
//! whatever it prints as `ESC ] 7501 ; state=... ST`. herdr-core itself reports
//! idle, working, done, blocked and error with optional kind, progress, app,
//! title and msg. The agents we run (pi today) print none of it, so this
//! wrapper does: it runs the agent with the terminal it was given and reports
//! the states it can know for certain.
//!
//! Today the wrapper only brackets the agent's life (start, exit). Screen-level
//! states (blocked on a permission prompt, a turn that ended) need the PTY relay
//! that is not written yet; until it is, the ticker's own screen reading covers
//! them, as it does for herdr 1.

use std::process::{Command, ExitCode};
use std::io::Write as _;

/// Writes one OSC 7501 record straight to the terminal, unbuffered.
fn report(fields: &str) {
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]7501;{fields}\x1b\\");
    let _ = out.flush();
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // `hp-agent -- cmd` is what a shell writes; `hp-agent cmd` is what a
    // template writes. Both mean the same thing.
    if args.first().map(String::as_str) == Some("--") {
        args.remove(0);
    }
    let Some(command) = args.first().cloned() else {
        eprintln!("usage: hp-agent <command> [args...]");
        return ExitCode::from(2);
    };

    let app = std::path::Path::new(&command)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| command.clone());
    report(&format!("state=idle:app={app}"));

    let status = Command::new(&command).args(&args[1..]).status();

    match status {
        Ok(status) if status.success() => {
            report(&format!("state=done:app={app}"));
            ExitCode::SUCCESS
        }
        Ok(status) => {
            let code = status.code().unwrap_or(1);
            report(&format!("state=error:app={app}:msg=exit {code}"));
            ExitCode::from(u8::try_from(code).unwrap_or(1))
        }
        Err(error) => {
            report(&format!("state=error:app={app}:msg={}", error.kind()));
            eprintln!("hp-agent: cannot run `{command}`: {error}");
            ExitCode::from(127)
        }
    }
}
