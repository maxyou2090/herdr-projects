//! Who a TASKS.md task may be assigned to: `me` and the thread profiles this
//! project allows on this machine. Nothing else — there are no other machines
//! to reach any more, so an `@machine` owner is refused with a clear error
//! rather than silently read as a person.

use std::fmt::Write as _;

use anyhow::{Result, bail};

use crate::paths::Ctx;
use crate::project::Project;
use crate::tasks::Owner;

#[derive(Debug, Default, PartialEq)]
pub struct Assignable {
    /// The thread profiles this project allows on this machine.
    pub local: Vec<String>,
}

impl Assignable {
    /// Refuses an owner that is not `me` or one of the project's profiles
    /// here. A machine owner is refused on purpose: it names something this
    /// binary can no longer start a thread on.
    pub fn check(&self, owner: &Owner) -> Result<()> {
        match owner {
            Owner::Unassigned | Owner::Me | Owner::Agent { profile: None } => Ok(()),
            Owner::Person(text) => bail!("`{text}` is not a profile; write a person's name only when the user names them. Assignable: {}", self.line()),
            Owner::Machine(text) => bail!(
                "`{text}` names a machine, and threads only start on this machine any more (a profile name belongs here, not `@machine`); assignable: {}",
                self.line()
            ),
            Owner::Agent { profile: Some(profile) } => {
                if self.local.contains(profile) {
                    return Ok(());
                }
                bail!("there is no profile `{profile}` for threads here (write it as a person only when the user names them); assignable: {}", self.line())
            }
        }
    }

    /// `claude, codex-fast`.
    pub fn line(&self) -> String {
        if self.local.is_empty() { "(none; only me)".into() } else { self.local.join(", ") }
    }
}

/// Everything a task in `project` may be assigned to.
pub fn load(ctx: &Ctx, project: &Project) -> Result<Assignable> {
    let config = crate::profiles::load(&ctx.config_dir)?;
    let safety = project.safety(&ctx.config_dir)?;
    let (settings, _) = project.read_project_md()?;
    let detected = crate::profiles::detect(ctx.env);
    let local = crate::profiles::usable(&config, &safety, crate::profiles::Role::Thread, &detected, &settings.thread_profile).into_iter().map(|p| p.name).collect();
    Ok(Assignable { local })
}

/// The one `Assignable:` line of `context`.
pub fn context_line(ctx: &Ctx, project: &Project) -> String {
    match load(ctx, project) {
        Ok(assignable) => format!("Assignable (agent owners): {}\n", assignable.line()),
        Err(error) => format!("config-error: assignable: {error:#}\n"),
    }
}

/// An owner is who is responsible, so it never becomes a way around the
/// user's naming: an on-request profile cannot own a task at all
/// (`hp task add --owner`, `hp task thread --owner`). It is still refused
/// later, at launch, by the allow-list that `Assignable` reads.
pub fn check_owner(ctx: &Ctx, project: &Project, owner: &str) -> Result<()> {
    let config = crate::profiles::load(&ctx.config_dir)?;
    refuse_on_request(&config, &Owner::parse(owner)).map_err(|error| {
        anyhow::anyhow!("{error} Assignable: {}", load(ctx, project).map(|a| a.line()).unwrap_or_else(|e| format!("config-error: {e:#}")))
    })
}

/// The pure part of [`check_owner`].
pub fn refuse_on_request(config: &crate::profiles::Config, owner: &Owner) -> Result<()> {
    let Owner::Agent { profile: Some(profile) } = owner else {
        return Ok(());
    };
    if !config.profiles.get(profile).is_some_and(|entry| entry.on_request) {
        return Ok(());
    }
    bail!(
        "`{profile}` is on request only: it can never own a task. Start a thread with it directly — `hp thread start <slug> --profile {profile}` — once the user names that profile; no `profile allow` is needed."
    )
}

/// The `On request …` line of `assignable`, naming every on-request profile;
/// `None` when this machine has none. Separate from the owner line because
/// these are exactly the profiles that are never owners and never need an
/// allow: the user naming one is enough for `--profile`.
pub fn on_request_line(config: &crate::profiles::Config) -> Option<String> {
    let names: Vec<String> = config.on_request().into_iter().map(|p| p.name).collect();
    if names.is_empty() {
        return None;
    }
    Some(format!("On request (never an owner; `--profile` works once the user names it): {}", names.join(", ")))
}

/// `assignable <slug> [--check OWNER]`.
pub fn run(ctx: &Ctx, slug: &str, check: Option<&str>) -> Result<()> {
    let project = Project::load(&ctx.root, slug)?;
    let assignable = load(ctx, &project)?;
    let config = crate::profiles::load(&ctx.config_dir)?;
    if let Some(owner) = check {
        let text = owner.trim().trim_start_matches('(').trim_end_matches(')');
        let parsed = Owner::parse(text);
        // An on-request profile is refused as an owner, but it is not a
        // missing profile: saying so sent a coordinator off to `profile
        // allow` for something that needs no allow at all.
        if let Err(error) = refuse_on_request(&config, &parsed) {
            bail!("{error} Assignable: {}", assignable.line());
        }
        assignable.check(&parsed)?;
        println!("`{text}` is a valid owner");
        return Ok(());
    }
    let mut out = String::new();
    let _ = writeln!(out, "me: the user");
    let _ = writeln!(out, "Profiles here: {}", if assignable.local.is_empty() { "(none allowed)".to_string() } else { assignable.local.join(", ") });
    if let Some(line) = on_request_line(&config) {
        let _ = writeln!(out, "{line}");
    }
    print!("{out}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(profile: Option<&str>) -> Owner {
        Owner::Agent { profile: profile.map(str::to_string) }
    }

    #[test]
    fn owners_are_checked_against_this_machines_profiles() {
        let assignable = Assignable { local: vec!["claude".into(), "codex-fast".into()] };
        for good in [Owner::Me, Owner::Unassigned, agent(Some("codex-fast")), agent(None)] {
            assignable.check(&good).unwrap();
        }
        assert!(assignable.check(&agent(Some("pi"))).unwrap_err().to_string().contains("no profile `pi`"));
        assert!(assignable.check(&Owner::Person("Bob Smith".into())).is_err());
        // `@machine` is no longer assignable: it is refused by name, not read
        // as a person.
        let error = assignable.check(&Owner::Machine("@m1".into())).unwrap_err().to_string();
        assert!(error.contains("only start on this machine"), "{error}");
        assert_eq!(assignable.line(), "claude, codex-fast");
        assert_eq!(Assignable::default().line(), "(none; only me)");
    }

    /// An on-request profile is the user's to name, so it is never in the list
    /// an owner is checked against, and never an owner at all even when a
    /// project's allow-list names it (t-0088).
    #[test]
    fn an_on_request_profile_is_never_an_owner() {
        let text = "[profiles.sol]\nagent = \"codex\"\nmodel = \"gpt-6.2-sol\"\non_request = true\n\n[profiles.luna]\nagent = \"omp\"\n";
        let config: crate::profiles::Config = toml::from_str(text).unwrap();
        let error = refuse_on_request(&config, &agent(Some("sol"))).unwrap_err().to_string();
        assert!(error.contains("`sol` is on request only"), "{error}");
        // It says how to use it anyway, so nobody goes looking for an allow.
        assert!(error.contains("--profile sol"), "{error}");
        assert!(error.contains("no `profile allow` is needed"), "{error}");
        assert!(!error.contains("there is no profile"), "{error}");
        assert_eq!(on_request_line(&config).unwrap(), "On request (never an owner; `--profile` works once the user names it): sol");
        assert!(refuse_on_request(&config, &agent(Some("luna"))).is_ok());
        for ok in [Owner::Me, Owner::Unassigned, agent(None), Owner::Person("Priya Rao".into())] {
            refuse_on_request(&config, &ok).unwrap();
        }
        // Even a list that names it does not make it assignable.
        let safety = crate::project::Safety { thread_profiles: Some(vec!["luna".into(), "sol".into()]), ..Default::default() };
        let local: Vec<String> = crate::profiles::usable(&config, &safety, crate::profiles::Role::Thread, &[], "luna").into_iter().map(|p| p.name).collect();
        let assignable = Assignable { local };
        assert_eq!(assignable.line(), "luna");
        assert!(assignable.check(&agent(Some("sol"))).is_err());
    }

    /// `assignable` names the on-request profiles apart from the owners, and
    /// prints nothing about them when there are none.
    #[test]
    fn the_on_request_line_is_there_only_when_there_are_such_profiles() {
        let config: crate::profiles::Config = toml::from_str("[profiles.luna]\nagent = \"omp\"\n").unwrap();
        assert_eq!(on_request_line(&config), None);
    }
}
