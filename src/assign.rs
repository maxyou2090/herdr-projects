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
pub fn load(ctx: &Ctx, project: &Project, _refresh: bool) -> Result<Assignable> {
    let config = crate::profiles::load(&ctx.config_dir)?;
    let safety = project.safety(&ctx.config_dir)?;
    let (settings, _) = project.read_project_md()?;
    let detected = crate::profiles::detect(ctx.env);
    let local = crate::profiles::usable(&config, &safety, crate::profiles::Role::Thread, &detected, &settings.thread_profile).into_iter().map(|p| p.name).collect();
    Ok(Assignable { local })
}

/// The one `Assignable:` line of `context`.
pub fn context_line(ctx: &Ctx, project: &Project) -> String {
    match load(ctx, project, false) {
        Ok(assignable) => format!("Assignable (agent owners): {}\n", assignable.line()),
        Err(error) => format!("config-error: assignable: {error:#}\n"),
    }
}

/// `assignable <slug> [--refresh] [--check OWNER]`. `--refresh` is accepted and
/// does nothing: it used to re-look up other machines' profiles.
pub fn run(ctx: &Ctx, slug: &str, refresh: bool, check: Option<&str>) -> Result<()> {
    let project = Project::load(&ctx.root, slug)?;
    let assignable = load(ctx, &project, refresh)?;
    if let Some(owner) = check {
        let text = owner.trim().trim_start_matches('(').trim_end_matches(')');
        assignable.check(&Owner::parse(text))?;
        println!("`{text}` is a valid owner");
        return Ok(());
    }
    let mut out = String::new();
    let _ = writeln!(out, "me: the user");
    let _ = writeln!(out, "Profiles here: {}", if assignable.local.is_empty() { "(none allowed)".to_string() } else { assignable.local.join(", ") });
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
}
