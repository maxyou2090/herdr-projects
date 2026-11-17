//! Root, config directory, herdr binary and session resolution.
//!
//! Nothing here reads the process environment directly: callers pass an `Env`,
//! so resolution order is testable and never depends on plugin variables.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::runner::Runner;

#[derive(Debug, Clone)]
pub struct Env {
    vars: BTreeMap<String, String>,
    pub home: PathBuf,
}

impl Env {
    pub fn from_process() -> Result<Self> {
        let vars: BTreeMap<String, String> = std::env::vars().collect();
        let home = vars
            .get("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .context("HOME is not set")?;
        Ok(Env { vars, home })
    }

    #[cfg(test)]
    pub fn for_test(home: &Path, vars: &[(&str, &str)]) -> Self {
        Env {
            vars: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            home: home.to_path_buf(),
        }
    }

    /// A variable's value; an empty value counts as unset.
    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str).filter(|v| !v.is_empty())
    }

    /// `HERDR_APP_BIN` when set, else `herdr-app` on `PATH`: the window that
    /// shows a core's terminals in spaces and tabs.
    pub fn herdr_app_bin(&self) -> String {
        self.var("HERDR_APP_BIN").unwrap_or("herdr-app").to_string()
    }

    /// The fixed user-level config directory, `~/.config/herdr-projects`.
    pub fn config_dir(&self) -> PathBuf {
        self.home.join(".config").join("herdr-projects")
    }

    /// `HC_BIN_PATH` when set, else `hc` on `PATH`: herdr-core's own CLI, the
    /// one binary every engine module reaches core through.
    pub fn hc_bin(&self) -> String {
        self.var("HC_BIN_PATH").unwrap_or("hc").to_string()
    }

    /// The core terminal this process runs in: `HC_TERMINAL_ID`, the address
    /// core gives every terminal it starts. `None` outside one — the ticker,
    /// the app, a test.
    ///
    /// This is the only answer to "which pane am I?" after the port. Herdr 1
    /// exported the pane and tab ids as `HERDR_PANE_ID` / `HERDR_TAB_ID`, and
    /// herdr-core never sets them: every check that read them was permanently
    /// false, which is how `open --here`, the lane a `context` reads and
    /// `thread resolve`'s coordinator check silently stopped working.
    pub fn this_terminal(&self) -> Option<&str> {
        self.var("HC_TERMINAL_ID")
    }

    /// The core session this process runs in: `HC_SESSION`, empty for the
    /// default core. The same name [`crate::core::Core::cmd`] hands its
    /// children, so a command and the `hc` it runs always agree on the session.
    pub fn this_session(&self) -> &str {
        self.var("HC_SESSION").unwrap_or("")
    }

    fn expand_tilde(&self, path: &str) -> PathBuf {
        match path.strip_prefix("~/") {
            Some(rest) => self.home.join(rest),
            None if path == "~" => self.home.clone(),
            None => PathBuf::from(path),
        }
    }
}

/// This binary's own path with symbolic links resolved, so a path written into
/// hooks, AGENTS.md or the tab bar survives `~/.local/bin` links changing.
pub fn binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("could not find this binary's own path")?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

/// What every subcommand works from: the environment, the resolved root and
/// config directory, and the runner all external commands go through.
pub struct Ctx<'a> {
    pub env: &'a Env,
    pub root: PathBuf,
    pub config_dir: PathBuf,
    pub runner: &'a dyn Runner,
    /// False in tests, so commands that ensure a ticker never spawn a process.
    pub detached_ticker: bool,
}

/// The part of `config.toml` that resolution needs. Safety tables are read by
/// the `project` module from the same file.
#[derive(Debug, Default, Deserialize)]
struct RootConfig {
    root: Option<String>,
}

/// Projects root: `--root`, then `HERDR_PROJECTS_ROOT`, then `root` in
/// `<config_dir>/config.toml`, then `~/.herdr-projects`.
pub fn resolve_root(flag: Option<&Path>, env: &Env, config_dir: &Path) -> Result<PathBuf> {
    if let Some(flag) = flag {
        return absolute(flag);
    }
    if let Some(var) = env.var("HERDR_PROJECTS_ROOT") {
        return absolute(&env.expand_tilde(var));
    }
    let config_file = config_dir.join("config.toml");
    if let Ok(text) = std::fs::read_to_string(&config_file) {
        let config: RootConfig = toml::from_str(&text)
            .with_context(|| format!("{} does not parse", config_file.display()))?;
        if let Some(root) = config.root.filter(|r| !r.is_empty()) {
            return absolute(&env.expand_tilde(&root));
        }
    }
    Ok(env.home.join(".herdr-projects"))
}

fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).with_context(|| format!("bad path {}", path.display()))
}

/// Which herdr session a command should talk to, as given on the command line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionFlags {
    pub session: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// The core session a project lives in: an `HC_SESSION` name, empty for the
    /// default core. Herdr 1 recorded a socket path here; a path reads as the
    /// default session, so records written before the port keep working.
    pub socket: String,
    /// Known only when the session was chosen by name.
    pub name: Option<String>,
}

/// `--session`, then `HC_SESSION`. herdr-core has one core per session and no
/// session registry, so a name is used as given (a name that was never started
/// is started on first use) and there is nothing to ask.
///
/// Herdr 1's `--socket` and `HERDR_SESSION` are gone rather than translated:
/// core sets `HC_SESSION` for every command it starts and never sets either of
/// those, so reading them could only ever name a session the command is not in
/// (a recorded socket path still reads as the default session, see [`Session`]).
pub fn resolve_session(flags: &SessionFlags, env: &Env, _runner: &dyn Runner) -> Result<Session> {
    if let Some(name) = &flags.session {
        return Ok(Session {
            socket: name.clone(),
            name: Some(name.clone()),
        });
    }
    if let Some(name) = env.var("HC_SESSION") {
        return Ok(Session {
            socket: name.to_string(),
            name: Some(name.to_string()),
        });
    }
    Ok(Session {
        socket: String::new(),
        name: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::fake::FakeRunner;

    #[test]
    fn root_order_flag_env_config_default() {
        let home = tempfile::tempdir().unwrap();
        let config_dir = home.path().join("cfg");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("config.toml"), "root = \"~/from-config\"\n").unwrap();

        let env = Env::for_test(home.path(), &[("HERDR_PROJECTS_ROOT", "/from-env")]);
        let flag = PathBuf::from("/from-flag");
        assert_eq!(resolve_root(Some(&flag), &env, &config_dir).unwrap(), flag);
        assert_eq!(
            resolve_root(None, &env, &config_dir).unwrap(),
            PathBuf::from("/from-env")
        );

        let env = Env::for_test(home.path(), &[]);
        assert_eq!(
            resolve_root(None, &env, &config_dir).unwrap(),
            home.path().join("from-config")
        );
        assert_eq!(
            resolve_root(None, &env, &home.path().join("missing")).unwrap(),
            home.path().join(".herdr-projects")
        );
    }

    #[test]
    fn root_config_that_does_not_parse_is_an_error() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("config.toml"), "root = [").unwrap();
        let env = Env::for_test(home.path(), &[]);
        assert!(resolve_root(None, &env, home.path()).is_err());
    }

    #[test]
    fn empty_variable_counts_as_unset() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[("HERDR_PROJECTS_ROOT", ""), ("HC_BIN_PATH", "")]);
        assert_eq!(
            resolve_root(None, &env, &home.path().join("none")).unwrap(),
            home.path().join(".herdr-projects")
        );
        assert_eq!(env.hc_bin(), "hc");
    }

    #[test]
    fn hc_bin_prefers_the_variable() {
        let env = Env::for_test(Path::new("/h"), &[("HC_BIN_PATH", "/opt/hc")]);
        assert_eq!(env.hc_bin(), "/opt/hc");
    }

    #[test]
    fn session_order_flag_then_hc_session_then_default() {
        let runner = FakeRunner::new();
        let by_flag = SessionFlags { session: Some("explicit".into()) };
        let env = Env::for_test(Path::new("/h"), &[("HC_SESSION", "hp-dev")]);
        let got = resolve_session(&by_flag, &env, &runner).unwrap();
        assert_eq!(got, Session { socket: "explicit".into(), name: Some("explicit".into()) });

        let got = resolve_session(&SessionFlags::default(), &env, &runner).unwrap();
        assert_eq!(got, Session { socket: "hp-dev".into(), name: Some("hp-dev".into()) });

        // Nothing set: the default core, which has no name to pass on.
        let env = Env::for_test(Path::new("/h"), &[]);
        assert_eq!(resolve_session(&SessionFlags::default(), &env, &runner).unwrap(), Session { socket: String::new(), name: None });

        // Herdr 1's variable no longer selects a session: core never sets it.
        let env = Env::for_test(Path::new("/h"), &[("HERDR_SESSION", "hp-dev")]);
        assert_eq!(resolve_session(&SessionFlags::default(), &env, &runner).unwrap(), Session { socket: String::new(), name: None });
    }

    #[test]
    fn the_calling_terminal_and_session_are_read_from_core_only() {
        let env = Env::for_test(Path::new("/h"), &[("HC_TERMINAL_ID", "t44"), ("HERDR_PANE_ID", "w1:p1")]);
        assert_eq!(env.this_terminal(), Some("t44"));
        assert_eq!(env.this_session(), "");
        let env = Env::for_test(Path::new("/h"), &[("HC_TERMINAL_ID", ""), ("HC_SESSION", "hp-dev")]);
        assert_eq!(env.this_terminal(), None);
        assert_eq!(env.this_session(), "hp-dev");
    }
}
