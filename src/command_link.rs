//! The `herdr-projects` command on `PATH`: a link in `~/.local/bin` (or
//! `$XDG_BIN_HOME`) to the plugin's binary, made at every plugin start and by
//! `doctor --fix`, so a reinstall into another folder never leaves it broken.
//! Only a missing entry, or a link that is dangling or points into Herdr's
//! plugin folder, is ever considered for a rewrite; a file or anyone else's
//! link is left alone.
//! Like the hooks, a write consults the journal: only the installation recorded
//! there may relink, so a throwaway release build cannot take the link over.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::paths::Env;

pub const NAME: &str = "herdr-projects";

/// The folder the link goes in: `$XDG_BIN_HOME`, else `~/.local/bin`.
pub fn bin_dir(env: &Env) -> PathBuf {
    env.var("XDG_BIN_HOME").map(PathBuf::from).unwrap_or_else(|| env.home.join(".local/bin"))
}

pub fn link_path(env: &Env) -> PathBuf {
    bin_dir(env).join(NAME)
}

/// Whether `binary` is an installed build (`target/release/herdr-projects`),
/// not a test or debug binary that must never become the user's command.
pub fn installable(binary: &Path) -> bool {
    binary.ends_with(Path::new("release").join(NAME))
}

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    /// A link that resolves to `binary`.
    Ours,
    Missing,
    /// A link that is dangling or points to another build inside Herdr's
    /// plugin folder: a candidate for replacement, if the journal allows it.
    Stale(PathBuf),
    /// A file, or a link somewhere else (a checkout of the user's own): never touched.
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
        Ok(_) if target.starts_with(plugins_dir(env)) => State::Stale(target),
        Ok(_) => State::Foreign(format!("a link to {}", target.display())),
    }
}

/// Herdr's plugin installs live under `<herdr config dir>/plugins`.
fn plugins_dir(env: &Env) -> PathBuf {
    let config = crate::setup::herdr_config_path(env);
    config.parent().unwrap_or(Path::new("/")).join("plugins")
}

/// The journal's installation, when it is not `binary` at `root`. The journal
/// is the record of what the installation is, so the link follows the same
/// rule as the hooks: a build at another root says so instead of relinking.
fn refusal(env: &Env, binary: &Path, root: &Path) -> Option<String> {
    let journal = crate::setup::load_journal(&env.config_dir());
    let here = crate::setup::Installation { binary: binary.to_path_buf(), root: root.to_path_buf() };
    crate::setup::may_repair(&journal, &here).err()
}

/// Makes the link when it is missing or stale; returns the state found. A
/// journal naming another installation refuses the write, as it does for the
/// hooks; `configure` is the deliberate way to point it here.
pub fn ensure(env: &Env, binary: &Path, root: &Path) -> Result<State> {
    let found = state(env, binary);
    if matches!(&found, State::Missing | State::Stale(_)) {
        if let Some(refusal) = refusal(env, binary, root) {
            bail!("{refusal}");
        }
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

/// What a shell with `path_var` runs for `herdr-projects`, if anything.
pub fn resolves_to(path_var: &str) -> Option<PathBuf> {
    std::env::split_paths(path_var).map(|dir| dir.join(NAME)).find(|candidate| candidate.is_file())
}

/// The `doctor` line: `(ok, detail)` where `None` is a warning.
pub fn check(env: &Env, binary: &Path, root: &Path, path_var: &str, fix: bool) -> (Option<bool>, String) {
    let link = link_path(env);
    if !installable(binary) {
        return (None, format!("{} is not an installed build, so {} was not checked", binary.display(), link.display()));
    }
    let found = state(env, binary);
    if matches!(&found, State::Missing | State::Stale(_))
        && let Some(refusal) = refusal(env, binary, root)
    {
        let detail = match &found {
            State::Missing => format!("{} is missing; {refusal}", link.display()),
            State::Stale(old) => format!("{} links {}, not this binary; {refusal}", link.display(), old.display()),
            _ => unreachable!(),
        };
        return (None, detail);
    }
    let (found, fixed) = if fix {
        match ensure(env, binary, root) {
            Ok(found) => {
                let fixed = matches!(found, State::Missing | State::Stale(_));
                (found, fixed)
            }
            Err(error) => return (Some(false), format!("could not fix: {error:#}")),
        }
    } else {
        (state(env, binary), false)
    };
    let mut detail = match &found {
        State::Foreign(what) => return (None, format!("{} is {what}, so it was left alone; move it away and run `doctor --fix` to link this binary there", link.display())),
        State::Missing if !fix => return (None, format!("{} is missing; `doctor --fix` (or restarting Herdr) links it", link.display())),
        State::Stale(old) if !fix => return (None, format!("{} links {}, not this binary; `doctor --fix` (or restarting Herdr) relinks it", link.display(), old.display())),
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
        let binary = home.path().join(".config/herdr/plugins/github/herdr-projects-abc/target/release/herdr-projects");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, "#!/bin/sh\n").unwrap();
        (home, env, binary)
    }

    fn path_with(env: &Env) -> String {
        bin_dir(env).display().to_string() + ":/usr/bin"
    }

    /// A journal entry recording `binary` at `root` as the installation, as
    /// `configure` writes one.
    fn journal_recording(env: &Env, binary: &Path, root: &Path) {
        let entry = crate::setup::Owned::new(Some(String::new()), String::new(), "hooks", Some("hook".into()), binary, root);
        crate::setup::save_journal(&env.config_dir(), &[("settings.json".to_string(), entry)].into()).unwrap();
    }

    #[test]
    fn a_missing_link_is_made_and_then_ours() {
        let (home, env, binary) = setup();
        assert_eq!(ensure(&env, &binary, home.path()).unwrap(), State::Missing);
        assert_eq!(std::fs::read_link(link_path(&env)).unwrap(), binary);
        assert_eq!(ensure(&env, &binary, home.path()).unwrap(), State::Ours);
        let (ok, detail) = check(&env, &binary, home.path(), &path_with(&env), false);
        assert_eq!(ok, Some(true), "{detail}");
    }

    #[test]
    fn a_dangling_link_or_one_into_another_plugin_install_is_replaced() {
        let (home, env, binary) = setup();
        std::fs::create_dir_all(bin_dir(&env)).unwrap();
        std::os::unix::fs::symlink(home.path().join("gone/herdr-projects"), link_path(&env)).unwrap();
        assert!(matches!(ensure(&env, &binary, home.path()).unwrap(), State::Stale(_)));
        assert_eq!(std::fs::read_link(link_path(&env)).unwrap(), binary);

        let other = home.path().join(".config/herdr/plugins/github/herdr-projects-old/target/release/herdr-projects");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, "").unwrap();
        std::fs::remove_file(link_path(&env)).unwrap();
        std::os::unix::fs::symlink(&other, link_path(&env)).unwrap();
        let (ok, detail) = check(&env, &binary, home.path(), &path_with(&env), true);
        assert_eq!(ok, Some(true), "{detail}");
        assert!(detail.starts_with("fixed:"), "{detail}");
        assert_eq!(std::fs::read_link(link_path(&env)).unwrap(), binary);
    }

    /// The incident PR #13 closed for the hooks, for the command link: a
    /// managed install's link is dangling, a throwaway release build at another
    /// root runs the repair path, and the journal says the installation is the
    /// managed one. The link must stay where it was, with a visible reason.
    #[test]
    fn a_fix_from_another_release_build_does_not_take_the_command_link() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[("XDG_BIN_HOME", home.path().join("bin").to_str().unwrap())]);
        let installed = home.path().join(".config/herdr/plugins/github/herdr-projects-abc/target/release/herdr-projects");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        std::fs::write(&installed, "#!/bin/sh\n").unwrap();
        let installed_root = home.path().join("projects");
        std::fs::create_dir_all(&installed_root).unwrap();
        journal_recording(&env, &installed, &installed_root);

        std::fs::create_dir_all(bin_dir(&env)).unwrap();
        let gone = home.path().join(".config/herdr/plugins/github/herdr-projects-old/target/release/herdr-projects");
        std::os::unix::fs::symlink(&gone, link_path(&env)).unwrap();

        let throwaway = home.path().join("tmp/build/target/release/herdr-projects");
        std::fs::create_dir_all(throwaway.parent().unwrap()).unwrap();
        std::fs::write(&throwaway, "#!/bin/sh\n").unwrap();
        let scratch = home.path().join("scratch-root");

        let (ok, detail) = check(&env, &throwaway, &scratch, &path_with(&env), true);
        assert_eq!(std::fs::read_link(link_path(&env)).unwrap(), gone, "a throwaway release build took the link: {detail}");
        assert_eq!(ok, None, "{detail}");
        assert!(detail.contains("left alone: this is not the installation"), "{detail}");
        let recorded = format!("{} --root {}", installed.canonicalize().unwrap().display(), installed_root.canonicalize().unwrap().display());
        assert!(detail.contains(&recorded), "the recorded installation is named: {detail}");
        assert!(detail.contains("`configure`"), "{detail}");

        // The link still points inside Herdr's plugin folder: no different.
        let other = gone.clone();
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, "#!/bin/sh\n").unwrap();
        std::fs::remove_file(link_path(&env)).unwrap();
        std::os::unix::fs::symlink(&other, link_path(&env)).unwrap();
        let (ok, detail) = check(&env, &throwaway, &scratch, &path_with(&env), true);
        assert_eq!(std::fs::read_link(link_path(&env)).unwrap(), other, "a throwaway release build took the link: {detail}");
        assert_eq!(ok, None, "{detail}");
        assert!(detail.contains("left alone: this is not the installation"), "{detail}");

        // Plugin start writes through `ensure`; it must refuse too.
        let refused = ensure(&env, &throwaway, &scratch).unwrap_err().to_string();
        assert!(refused.contains("left alone: this is not the installation"), "{refused}");
        assert_eq!(std::fs::read_link(link_path(&env)).unwrap(), other);
    }

    /// The other half, as for the hooks: the installation recorded in the
    /// journal still repairs its own link, and so does a moved install whose
    /// recorded binary is gone, at the same root.
    #[test]
    fn the_installation_and_a_moved_install_still_refresh_the_link() {
        let (home, env, installed) = setup();
        let root = home.path().join("projects");
        std::fs::create_dir_all(&root).unwrap();
        journal_recording(&env, &installed, &root);
        std::fs::create_dir_all(bin_dir(&env)).unwrap();
        std::os::unix::fs::symlink(home.path().join("gone"), link_path(&env)).unwrap();

        let (ok, detail) = check(&env, &installed, &root, &path_with(&env), true);
        assert_eq!(ok, Some(true), "{detail}");
        assert!(detail.starts_with("fixed:"), "{detail}");
        assert_eq!(std::fs::read_link(link_path(&env)).unwrap(), installed);

        // The recorded binary moved away; the binary now at that root repairs.
        let moved = home.path().join("moved/target/release/herdr-projects");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        std::fs::write(&moved, "#!/bin/sh\n").unwrap();
        std::fs::remove_file(&installed).unwrap();
        let (ok, detail) = check(&env, &moved, &root, &path_with(&env), true);
        assert_eq!(ok, Some(true), "{detail}");
        assert!(detail.starts_with("fixed:"), "{detail}");
        assert_eq!(std::fs::read_link(link_path(&env)).unwrap(), moved);
    }

    #[test]
    fn a_file_or_a_link_to_the_users_own_checkout_is_never_touched() {
        let (home, env, binary) = setup();
        std::fs::create_dir_all(bin_dir(&env)).unwrap();
        std::fs::write(link_path(&env), "mine").unwrap();
        assert!(matches!(ensure(&env, &binary, home.path()).unwrap(), State::Foreign(_)));
        assert_eq!(std::fs::read_to_string(link_path(&env)).unwrap(), "mine");
        let (ok, detail) = check(&env, &binary, home.path(), &path_with(&env), true);
        assert_eq!(ok, None);
        assert!(detail.contains("left alone"), "{detail}");

        std::fs::remove_file(link_path(&env)).unwrap();
        let own = home.path().join("dev/herdr-projects/target/release/herdr-projects");
        std::fs::create_dir_all(own.parent().unwrap()).unwrap();
        std::fs::write(&own, "").unwrap();
        std::os::unix::fs::symlink(&own, link_path(&env)).unwrap();
        assert!(matches!(ensure(&env, &binary, home.path()).unwrap(), State::Foreign(_)));
        assert_eq!(std::fs::read_link(link_path(&env)).unwrap(), own);
    }

    #[test]
    fn a_bin_dir_missing_from_path_is_a_warning_with_the_fix() {
        let (home, env, binary) = setup();
        let (ok, detail) = check(&env, &binary, home.path(), "/usr/bin", true);
        assert_eq!(ok, None);
        assert!(detail.contains("is not on your PATH: add `export PATH="), "{detail}");
        assert!(link_path(&env).is_symlink());
    }

    #[test]
    fn another_binary_earlier_on_path_is_named() {
        let (home, env, binary) = setup();
        let early = home.path().join("early");
        std::fs::create_dir_all(&early).unwrap();
        std::fs::write(early.join(NAME), "").unwrap();
        let path = format!("{}:{}", early.display(), path_with(&env));
        let (ok, detail) = check(&env, &binary, home.path(), &path, true);
        assert_eq!(ok, None);
        assert!(detail.contains("finds") && detail.contains("early"), "{detail}");
    }

    #[test]
    fn xdg_bin_home_wins_and_debug_builds_are_never_linked() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[("XDG_BIN_HOME", "/xdg/bin")]);
        assert_eq!(link_path(&env), PathBuf::from("/xdg/bin/herdr-projects"));
        let (ok, _) = check(&env, Path::new("/src/target/debug/herdr-projects"), home.path(), "", true);
        assert_eq!(ok, None);
        assert!(!Path::new("/xdg/bin").exists());
    }

    /// Runs scripts/link-command.sh for `checkout` with `home` as HOME.
    fn run_script(home: &Path, checkout: &Path) -> String {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/link-command.sh");
        let out = std::process::Command::new("sh")
            .arg(script)
            .arg(checkout)
            .env_clear()
            .env("HOME", home)
            .env("PATH", format!("{}:/usr/bin:/bin", home.join(".local/bin").display()))
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    #[test]
    fn the_install_script_links_where_herdr_will_move_the_checkout() {
        let home = tempfile::tempdir().unwrap();
        let plugins = home.path().join(".config/herdr/plugins");
        let checkout = plugins.join(".tmp-install-12-34/checkout");
        let link = home.path().join(".local/bin/herdr-projects");
        // Herdr's folder for plugin id `herdr-projects`: slug plus sha256's first 12 hex digits.
        let final_binary = plugins.join("github/herdr-projects-b1278ffb803c/target/release/herdr-projects");

        // Anywhere but a Herdr install checkout: nothing.
        run_script(home.path(), &home.path().join("dev/herdr-projects"));
        assert!(std::fs::symlink_metadata(&link).is_err());

        let said = run_script(home.path(), &checkout);
        assert_eq!(std::fs::read_link(&link).unwrap(), final_binary, "{said}");
        assert!(said.contains("linked"), "{said}");

        // A dangling link elsewhere is replaced; a working one is left alone.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(home.path().join("gone"), &link).unwrap();
        run_script(home.path(), &checkout);
        assert_eq!(std::fs::read_link(&link).unwrap(), final_binary);
        let own = home.path().join("own");
        std::fs::write(&own, "").unwrap();
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&own, &link).unwrap();
        assert!(run_script(home.path(), &checkout).contains("left"));
        assert_eq!(std::fs::read_link(&link).unwrap(), own);

        // A file is never touched.
        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, "mine").unwrap();
        assert!(run_script(home.path(), &checkout).contains("not a link"));
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "mine");
    }
}
