//! The `hp` command on `PATH`: a link in `~/.local/bin` (or `$XDG_BIN_HOME`)
//! to this binary, made by `doctor --fix`, so a rebuild or a moved checkout
//! never leaves a broken command. A file, or anyone else's link, is left alone.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::paths::Env;

pub const NAME: &str = "hp";

/// The folder the link goes in: `$XDG_BIN_HOME`, else `~/.local/bin`.
pub fn bin_dir(env: &Env) -> PathBuf {
    env.var("XDG_BIN_HOME").map(PathBuf::from).unwrap_or_else(|| env.home.join(".local/bin"))
}

pub fn link_path(env: &Env) -> PathBuf {
    bin_dir(env).join(NAME)
}

/// Whether `binary` is an installed build (`target/release/hp`), not a test or
/// debug binary that must never become the user's command.
pub fn installable(binary: &Path) -> bool {
    binary.ends_with(Path::new("release").join(NAME))
}

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    /// A link that resolves to `binary`.
    Ours,
    Missing,
    /// A link that is dangling: a candidate for replacement.
    Stale(PathBuf),
    /// A file, or a link somewhere else (a checkout of the user's own).
    Foreign(String),
}

pub fn state(env: &Env, binary: &Path) -> State {
    let link = link_path(env);
    let Ok(meta) = std::fs::symlink_metadata(&link) else {
        return State::Missing;
    };
    if !meta.file_type().is_symlink() {
        return State::Foreign("a file, not a link".into());
    }
    let Ok(target) = std::fs::read_link(&link) else {
        return State::Foreign("an unreadable link".into());
    };
    let target = link.parent().map(|dir| dir.join(&target)).unwrap_or(target);
    let binary = std::fs::canonicalize(binary).unwrap_or_else(|_| binary.to_path_buf());
    match std::fs::canonicalize(&target) {
        Ok(resolved) if resolved == binary => State::Ours,
        Err(_) => State::Stale(target),
        Ok(_) => State::Foreign(format!("a link to {}", target.display())),
    }
}

/// Makes the link when it is missing or stale; returns the state found.
pub fn ensure(env: &Env, binary: &Path) -> Result<State> {
    let found = state(env, binary);
    if matches!(&found, State::Missing | State::Stale(_)) {
        let link = link_path(env);
        let dir = bin_dir(env);
        std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
        // Link under a temporary name, then rename over: never a moment without a command.
        let tmp = dir.join(format!(".{NAME}.{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(binary, &tmp).with_context(|| format!("could not link {}", link.display()))?;
        if let Err(error) = std::fs::rename(&tmp, &link) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error).with_context(|| format!("could not link {}", link.display()));
        }
    }
    Ok(found)
}

/// What a shell with `path_var` runs for `hp`, if anything.
pub fn resolves_to(path_var: &str) -> Option<PathBuf> {
    std::env::split_paths(path_var).map(|dir| dir.join(NAME)).find(|candidate| candidate.is_file())
}

/// The `doctor` line: `(ok, detail)` where `None` is a warning.
pub fn check(env: &Env, binary: &Path, path_var: &str, fix: bool) -> (Option<bool>, String) {
    let link = link_path(env);
    if !installable(binary) {
        return (None, format!("{} is not an installed build, so {} was not checked", binary.display(), link.display()));
    }
    let (found, fixed) = if fix {
        match ensure(env, binary) {
            Ok(found) => {
                let fixed = matches!(found, State::Missing | State::Stale(_));
                (state(env, binary), fixed)
            }
            Err(error) => return (Some(false), format!("could not fix: {error:#}")),
        }
    } else {
        (state(env, binary), false)
    };
    let mut detail = match &found {
        State::Foreign(what) => return (None, format!("{} is {what}, so it was left alone; move it away and run `doctor --fix` to link this binary there", link.display())),
        State::Missing if !fix => return (None, format!("{} is missing; `doctor --fix` links it", link.display())),
        State::Stale(old) if !fix => return (None, format!("{} links {}, not this binary; `doctor --fix` relinks it", link.display(), old.display())),
        _ if fixed => format!("fixed: {} now links this binary", link.display()),
        _ => format!("{} links this binary", link.display()),
    };
    let dir = bin_dir(env);
    if !std::env::split_paths(path_var).any(|d| d == dir) {
        detail.push_str(&format!("; but {} is not on your PATH: add `export PATH=\"{}:$PATH\"` to your shell profile", dir.display(), dir.display()));
        return (None, detail);
    }
    match resolves_to(path_var) {
        Some(first) if std::fs::canonicalize(&first).ok() != std::fs::canonicalize(binary).ok() => {
            detail.push_str(&format!("; but your PATH finds {} first, which is another binary", first.display()));
            (None, detail)
        }
        _ => (Some(true), detail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Env, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let binary = home.path().join("checkout/target/release/hp");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, "#!/bin/sh\n").unwrap();
        (home, env, binary)
    }

    #[test]
    fn a_missing_link_is_made_and_a_file_is_left_alone() {
        let (home, env, binary) = setup();
        assert_eq!(state(&env, &binary), State::Missing);
        assert_eq!(ensure(&env, &binary).unwrap(), State::Missing);
        assert_eq!(state(&env, &binary), State::Ours);
        assert_eq!(ensure(&env, &binary).unwrap(), State::Ours);

        let link = link_path(&env);
        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, "not a link").unwrap();
        assert!(matches!(state(&env, &binary), State::Foreign(_)));
        assert_eq!(ensure(&env, &binary).unwrap(), State::Foreign("a file, not a link".into()));
        assert!(link.exists());
        let _ = home;
    }

    #[test]
    fn a_dangling_link_is_replaced() {
        let (home, env, binary) = setup();
        let dir = bin_dir(&env);
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(home.path().join("gone"), link_path(&env)).unwrap();
        assert!(matches!(state(&env, &binary), State::Stale(_)));
        ensure(&env, &binary).unwrap();
        assert_eq!(state(&env, &binary), State::Ours);
    }

    #[test]
    fn only_an_installed_build_is_linked() {
        assert!(installable(Path::new("/x/target/release/hp")));
        assert!(!installable(Path::new("/x/target/debug/hp")));
    }
}
