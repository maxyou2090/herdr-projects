//! Root, config directory, herdr binary and socket resolution.
//!
//! Nothing here reads the process environment directly: callers pass an `Env`,
//! so resolution order is testable and never depends on plugin variables.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
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

    /// The fixed user-level config directory, `~/.config/herdr-projects`.
    pub fn config_dir(&self) -> PathBuf {
        self.home.join(".config").join("herdr-projects")
    }

    /// `HC_BIN_PATH` when set, else `hc` on `PATH`.
    ///
    /// The port keeps the caller-facing name `herdr_bin` so the engine modules
    /// change in one place; the binary it returns is herdr-core's `hc`.
    pub fn herdr_bin(&self) -> String {
        self.var("HC_BIN_PATH").unwrap_or("hc").to_string()
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
    /// Herdr 1's `--socket`; kept so old scripts still parse.
    pub socket: Option<PathBuf>,
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

/// `--session`, then `HC_SESSION`, then `HERDR_SESSION`. herdr-core has one core
/// per session and no session registry, so a name is used as given and there is
/// nothing to ask.
///
/// `--socket` and `HERDR_SOCKET_PATH` were Herdr 1's way to pick a session; both
/// now mean "the default core", which is the only such session that exists
/// unless a name is given.
pub fn resolve_session(flags: &SessionFlags, env: &Env, _runner: &dyn Runner) -> Result<Session> {
    if flags.session.is_some() && flags.socket.is_some() {
        bail!("pass --session or --socket, not both");
    }
    if let Some(name) = &flags.session {
        return Ok(Session {
            socket: name.clone(),
            name: Some(name.clone()),
        });
    }
    if let Some(name) = env.var("HC_SESSION").or_else(|| env.var("HERDR_SESSION")) {
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
        assert_eq!(env.herdr_bin(), "hc");
    }

    #[test]
    fn herdr_bin_prefers_the_variable() {
        let env = Env::for_test(Path::new("/h"), &[("HC_BIN_PATH", "/opt/hc")]);
        assert_eq!(env.herdr_bin(), "/opt/hc");
    }

    #[test]
    fn session_order_flag_then_hc_session_then_default() {
        let runner = FakeRunner::new();
        let by_flag = SessionFlags { session: Some("explicit".into()), socket: None };
        let env = Env::for_test(Path::new("/h"), &[("HC_SESSION", "hp-dev")]);
        let got = resolve_session(&by_flag, &env, &runner).unwrap();
        assert_eq!(got, Session { socket: "explicit".into(), name: Some("explicit".into()) });

        let got = resolve_session(&SessionFlags::default(), &env, &runner).unwrap();
        assert_eq!(got, Session { socket: "hp-dev".into(), name: Some("hp-dev".into()) });

        // Herdr 1's `--socket` and `HERDR_SESSION` still parse; a socket path
        // means the default core, and a name is a name.
        let by_socket = SessionFlags { session: None, socket: Some("/flag.sock".into()) };
        let env = Env::for_test(Path::new("/h"), &[]);
        let got = resolve_session(&by_socket, &env, &runner).unwrap();
        assert_eq!(got, Session { socket: String::new(), name: None });

        let env = Env::for_test(Path::new("/h"), &[("HERDR_SESSION", "hp-dev")]);
        assert_eq!(resolve_session(&SessionFlags::default(), &env, &runner).unwrap().name.as_deref(), Some("hp-dev"));

        // Nothing set: the default core, which has no name to pass on.
        let env = Env::for_test(Path::new("/h"), &[]);
        assert_eq!(resolve_session(&SessionFlags::default(), &env, &runner).unwrap(), Session { socket: String::new(), name: None });
    }

    #[test]
    fn session_and_socket_together_are_refused() {
        let runner = FakeRunner::new();
        let env = Env::for_test(Path::new("/h"), &[]);
        let flags = SessionFlags {
            session: Some("a".into()),
            socket: Some("/b".into()),
        };
        assert!(resolve_session(&flags, &env, &runner).is_err());
    }
}
