//! `SIGPIPE` back to its default disposition, first thing in `main`.
//!
//! Rust's runtime ignores `SIGPIPE` for the whole process, so a write to a pipe
//! whose reader has gone — `hp skill | head -1`, `hp context p | grep -m1` —
//! comes back as `EPIPE` and every `println!` turns it into a panic on stderr
//! with exit code 101. A truncated pipe is ordinary Unix usage, and `head`
//! closing it is not an error: with the default disposition the write ends the
//! program with `SIGPIPE` instead, silently, as `cat`, `grep` and `git` do.
//!
//! Both binaries call [`restore`] before they read a single argument. A
//! subprocess started afterwards inherits the default too, which is the
//! disposition a shell would have given it, so this also stops an agent started
//! by `hp-agent` from being handed an ignored `SIGPIPE`.

/// Restore the default `SIGPIPE` disposition. Unix only; elsewhere a no-op,
/// because Windows has no `SIGPIPE`.
pub fn restore() {
    #[cfg(unix)]
    // SAFETY: `signal(2)` with the default disposition, called from `main`
    // before any thread exists and before any signal handler can race it.
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

#[cfg(unix)]
const SIGPIPE: i32 = 13;

#[cfg(unix)]
const SIG_DFL: usize = 0;

#[cfg(unix)]
unsafe extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}
