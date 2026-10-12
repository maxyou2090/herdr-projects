//! Share the primary checkout's complete `.agents` directory with local worktrees.

use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// Link the primary checkout's `.agents` directory into a newly placed worktree.
pub fn prepare(repo: &Path, worktree: &Path) -> Result<()> {
    let primary = match primary_checkout(repo) {
        Ok(primary) => primary,
        Err(_error) if is_absent(&repo.join(".agents")) => return Ok(()),
        Err(error) => return Err(error),
    };
    let primary = fs::canonicalize(&primary).context("could not normalize primary checkout")?;
    let source = primary.join(".agents");
    match fs::symlink_metadata(&source) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            bail!(
                "{} must be a real directory inside the primary checkout",
                source.display()
            );
        }
        Ok(_) => {}
    }
    let source = fs::canonicalize(&source)
        .context("could not resolve primary-checkout .agents directory")?;
    if !source.starts_with(&primary) {
        bail!("{} resolves outside the primary checkout", source.display());
    }
    reject_symlink_entries(&source, &source)?;

    let worktree = fs::canonicalize(worktree).context("could not resolve worktree repository")?;
    refuse_tracked_agents(&worktree)?;
    let destination = worktree.join(".agents");
    match fs::symlink_metadata(&destination) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let target = fs::read_link(&destination).with_context(|| {
                format!(
                    "could not inspect worktree .agents link {}",
                    destination.display()
                )
            })?;
            let target = lexical_absolute(destination.parent().unwrap_or(&worktree), &target);
            if target == source {
                return Ok(());
            }
            bail!(
                "conflicting worktree .agents link at {}; it must point directly to {}; broken or external links are not followed",
                destination.display(),
                source.display()
            );
        }
        Ok(meta) if meta.is_dir() => bail!(
            "existing worktree .agents directory conflicts with shared primary-checkout .agents at {}; preserve and review its tracked/local content, move it to a unique backup before retrying; no automatic migration or deletion was attempted",
            destination.display()
        ),
        Ok(_) => bail!(
            "existing worktree .agents path is not a directory link: {}",
            destination.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            std::os::unix::fs::symlink(&source, &destination).with_context(|| {
                format!(
                    "could not create worktree .agents link {}",
                    destination.display()
                )
            })?;
            #[cfg(not(unix))]
            bail!(
                "directory links are not supported on this platform: {}",
                destination.display()
            );
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// Inspect real directories only. Refusing all entries that are symlinks avoids
/// traversing an untrusted path while sharing the whole directory.
fn reject_symlink_entries(root: &Path, directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("could not read {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            bail!(
                "refusing to share .agents containing a symlink at {}; review and resolve it inside the primary checkout first",
                path.display()
            );
        }
        if kind.is_dir() {
            let canonical = fs::canonicalize(&path)?;
            if !canonical.starts_with(root) {
                bail!(
                    "{} resolves outside the primary checkout's .agents directory",
                    path.display()
                );
            }
            reject_symlink_entries(root, &canonical)?;
        }
    }
    Ok(())
}

fn refuse_tracked_agents(worktree: &Path) -> Result<()> {
    let output = Command::new("git")
        .args([
            "-C",
            worktree.to_str().context("worktree path is not UTF-8")?,
            "ls-files",
            "-z",
            "--",
            ".agents",
        ])
        .output()
        .context("could not inspect tracked worktree .agents content")?;
    if !output.status.success() {
        bail!("git ls-files failed in {}", worktree.display());
    }
    if output
        .stdout
        .split(|byte| *byte == 0)
        .any(|path| !path.is_empty())
    {
        bail!(
            "tracked .agents content conflicts with sharing the primary-checkout directory in {}; preserve and review it before retrying",
            worktree.display()
        );
    }
    Ok(())
}

fn is_absent(path: &Path) -> bool {
    matches!(fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

/// Resolve a link spelling lexically without following any filesystem links.
fn lexical_absolute(parent: &Path, target: &Path) -> PathBuf {
    let absolute = if target.is_absolute() {
        target.to_path_buf()
    } else {
        parent.join(target)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                }
            }
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn primary_checkout(repo: &Path) -> Result<PathBuf> {
    let repo = fs::canonicalize(repo)
        .with_context(|| format!("could not resolve repository {}", repo.display()))?;
    let repo_arg = repo.to_str().context("repository path is not UTF-8")?;
    let bare = git_value(repo_arg, "--is-bare-repository")?;
    if bare == "true" {
        bail!(
            "{} is a bare repository, not a primary checkout",
            repo.display()
        );
    }
    let common = PathBuf::from(git_value_with(
        repo_arg,
        "--path-format=absolute",
        "--git-common-dir",
    )?);
    let common = fs::canonicalize(&common).with_context(|| {
        format!(
            "could not resolve git common directory {}",
            common.display()
        )
    })?;
    if common.file_name().is_none_or(|name| name != ".git") {
        bail!(
            "git common directory {} is external to a standard primary checkout",
            common.display()
        );
    }
    let primary = common
        .parent()
        .context("git common directory has no parent")?;
    let primary_git = primary.join(".git");
    let meta = fs::symlink_metadata(&primary_git)
        .with_context(|| format!("{} is not an ordinary primary checkout", primary.display()))?;
    if !meta.is_dir() || fs::canonicalize(&primary_git)? != common {
        bail!(
            "git common directory {} is not the .git directory of a primary checkout",
            primary.display()
        );
    }
    let check = git_value(
        primary
            .to_str()
            .context("primary checkout path is not UTF-8")?,
        "--show-toplevel",
    )?;
    let primary = fs::canonicalize(primary)?;
    if fs::canonicalize(check)? != primary {
        bail!("could not verify primary checkout at {}", primary.display());
    }
    Ok(primary)
}

fn git_value_with(repo: &str, arg: &str, extra: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["-C", repo, "rev-parse", arg, extra])
        .output()
        .context("could not run git to locate primary checkout")?;
    if !output.status.success() {
        bail!("git rev-parse {arg} failed in {repo}");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn git_value(repo: &str, arg: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["-C", repo, "rev-parse", arg])
        .output()
        .context("could not run git to locate primary checkout")?;
    if !output.status.success() {
        bail!("git rev-parse {arg} failed in {repo}");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    struct Fixture {
        _tmp: TempDir,
        primary: PathBuf,
        worktree: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let primary = tmp.path().join("primary checkout");
        let worktree = tmp.path().join("worktree with spaces");
        fs::create_dir_all(&primary).unwrap();
        git(&primary, &["init", "-q"]);
        git(&primary, &["config", "user.email", "test@example.invalid"]);
        git(&primary, &["config", "user.name", "Test"]);
        fs::write(primary.join("README.md"), "base").unwrap();
        git(&primary, &["add", "README.md"]);
        git(&primary, &["commit", "-qm", "base"]);
        let agents = primary.join(".agents");
        let skill = agents.join("skills/sample");
        fs::create_dir_all(skill.join("references")).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: sample\ndescription: Sample rules.\n---\n",
        )
        .unwrap();
        fs::write(skill.join("references/note.md"), "reference text").unwrap();
        git(
            &primary,
            &[
                "worktree",
                "add",
                "--detach",
                "--no-checkout",
                worktree.to_str().unwrap(),
            ],
        );
        Fixture {
            _tmp: tmp,
            primary,
            worktree,
        }
    }

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(["-C", repo.to_str().unwrap()])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn source(f: &Fixture) -> PathBuf {
        fs::canonicalize(f.primary.join(".agents")).unwrap()
    }

    #[test]
    fn shares_the_whole_agents_directory_idempotently_and_sees_later_skills() {
        let f = fixture();
        let agents = f.primary.join(".agents");
        let ordinary = agents.join("commands/NOT_A_SKILL.md");
        fs::create_dir_all(ordinary.parent().unwrap()).unwrap();
        fs::write(&ordinary, "shared non-skill file").unwrap();
        let script = agents.join("scripts/must-not-run.sh");
        fs::create_dir_all(script.parent().unwrap()).unwrap();
        fs::write(&script, "#!/bin/sh\ntouch should-not-exist\n").unwrap();

        prepare(&f.worktree, &f.worktree).unwrap();
        let link = f.worktree.join(".agents");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(&link).unwrap(), source(&f));
        assert_eq!(
            fs::read_to_string(link.join("commands/NOT_A_SKILL.md")).unwrap(),
            "shared non-skill file"
        );
        assert_eq!(
            fs::read_to_string(link.join("skills/sample/references/note.md")).unwrap(),
            "reference text"
        );
        assert_eq!(
            fs::read_to_string(link.join("scripts/must-not-run.sh")).unwrap(),
            "#!/bin/sh\ntouch should-not-exist\n"
        );
        assert!(!f.worktree.join("should-not-exist").exists());

        prepare(&f.worktree, &f.worktree).unwrap();
        let later = agents.join("skills/added-after-link/SKILL.md");
        fs::create_dir_all(later.parent().unwrap()).unwrap();
        fs::write(&later, "new Skill").unwrap();
        assert_eq!(
            fs::read_to_string(link.join("skills/added-after-link/SKILL.md")).unwrap(),
            "new Skill"
        );
        assert_eq!(fs::canonicalize(&link).unwrap(), source(&f));
    }

    #[test]
    fn missing_agents_source_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("plain directory");
        fs::create_dir_all(&repo).unwrap();
        prepare(&repo, &repo).unwrap();
        assert!(!repo.join(".agents").exists());

        let f = fixture();
        fs::remove_dir_all(f.primary.join(".agents")).unwrap();
        prepare(&f.worktree, &f.worktree).unwrap();
        assert!(!f.worktree.join(".agents").exists());
    }

    #[test]
    fn rejects_non_directory_or_symlink_primary_agents_without_creating_a_link() {
        let f = fixture();
        fs::remove_dir_all(f.primary.join(".agents")).unwrap();
        fs::write(f.primary.join(".agents"), "not a directory").unwrap();
        assert!(
            prepare(&f.worktree, &f.worktree)
                .unwrap_err()
                .to_string()
                .contains("real directory")
        );
        assert!(!f.worktree.join(".agents").exists());

        let f = fixture();
        let external = f._tmp.path().join("external agents");
        fs::create_dir_all(&external).unwrap();
        fs::remove_dir_all(f.primary.join(".agents")).unwrap();
        symlink(&external, f.primary.join(".agents")).unwrap();
        let error = prepare(&f.worktree, &f.worktree).unwrap_err().to_string();
        assert!(error.contains("real directory"), "{error}");
        assert!(!f.worktree.join(".agents").exists());
    }

    #[test]
    fn rejects_symlinks_inside_shared_agents_without_following_them() {
        let f = fixture();
        let external = f._tmp.path().join("external file");
        fs::write(&external, "outside").unwrap();
        symlink(&external, f.primary.join(".agents/external-link")).unwrap();
        let error = prepare(&f.worktree, &f.worktree).unwrap_err().to_string();
        assert!(
            error.contains("refusing to share .agents containing a symlink"),
            "{error}"
        );
        assert!(!f.worktree.join(".agents").exists());
        assert_eq!(fs::read_to_string(external).unwrap(), "outside");
    }

    #[test]
    fn refuses_existing_real_agents_directory_including_tracked_content() {
        let f = fixture();
        let local = f.worktree.join(".agents/local.md");
        fs::create_dir_all(local.parent().unwrap()).unwrap();
        fs::write(&local, "preserve me").unwrap();
        git(&f.worktree, &["add", "-f", ".agents/local.md"]);

        let error = prepare(&f.worktree, &f.worktree).unwrap_err().to_string();
        assert!(
            error.contains("tracked .agents content conflicts"),
            "{error}"
        );
        assert!(
            !fs::symlink_metadata(f.worktree.join(".agents"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(local).unwrap(), "preserve me");
        assert!(
            git_output(
                &f.worktree,
                &["ls-files", "--error-unmatch", ".agents/local.md"]
            )
            .status
            .success()
        );
    }

    #[test]
    fn tracked_agents_content_conflicts_even_with_an_exact_directory_link() {
        let f = fixture();
        let destination = f.worktree.join(".agents");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("tracked.md"), "tracked local content").unwrap();
        git(&f.worktree, &["add", "-f", ".agents/tracked.md"]);
        let backup = f.worktree.with_file_name(".agents.tracked-backup");
        fs::rename(&destination, &backup).unwrap();
        symlink(source(&f), &destination).unwrap();

        let error = prepare(&f.worktree, &f.worktree).unwrap_err().to_string();
        assert!(
            error.contains("tracked .agents content conflicts"),
            "{error}"
        );
        assert_eq!(fs::read_link(destination).unwrap(), source(&f));
        assert_eq!(
            fs::read_to_string(backup.join("tracked.md")).unwrap(),
            "tracked local content"
        );
    }

    #[test]
    fn refuses_existing_file_at_agents_target_without_replacing_it() {
        let f = fixture();
        let destination = f.worktree.join(".agents");
        fs::write(&destination, "keep this file").unwrap();
        let error = prepare(&f.worktree, &f.worktree).unwrap_err().to_string();
        assert!(
            error.contains("existing worktree .agents path is not a directory link"),
            "{error}"
        );
        assert_eq!(fs::read_to_string(destination).unwrap(), "keep this file");
    }

    #[test]
    fn rejects_wrong_and_broken_destination_links_without_following_or_replacing_them() {
        let f = fixture();
        let destination = f.worktree.join(".agents");
        let external = f._tmp.path().join("external target");
        fs::create_dir_all(&external).unwrap();
        symlink(&external, &destination).unwrap();
        let error = prepare(&f.worktree, &f.worktree).unwrap_err().to_string();
        assert!(
            error.contains("conflicting worktree .agents link"),
            "{error}"
        );
        assert_eq!(fs::read_link(&destination).unwrap(), external);

        let f = fixture();
        let destination = f.worktree.join(".agents");
        let missing = f._tmp.path().join("does-not-exist");
        symlink(&missing, &destination).unwrap();
        let error = prepare(&f.worktree, &f.worktree).unwrap_err().to_string();
        assert!(
            error.contains("conflicting worktree .agents link"),
            "{error}"
        );
        assert_eq!(fs::read_link(destination).unwrap(), missing);
    }

    #[test]
    fn finds_primary_checkout_from_a_linked_worktree() {
        let f = fixture();
        let nested = f._tmp.path().join("nested worktree");
        git(
            &f.primary,
            &["worktree", "add", "--detach", nested.to_str().unwrap()],
        );
        assert_eq!(
            primary_checkout(&nested).unwrap(),
            fs::canonicalize(&f.primary).unwrap()
        );
        prepare(&nested, &f.worktree).unwrap();
        assert_eq!(
            fs::canonicalize(f.worktree.join(".agents")).unwrap(),
            source(&f)
        );
    }

    #[test]
    fn rejects_bare_and_external_git_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("bare.git");
        assert!(
            Command::new("git")
                .args(["init", "-q", "--bare", bare.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            primary_checkout(&bare)
                .unwrap_err()
                .to_string()
                .contains("bare repository")
        );

        let external = tmp.path().join("external.git");
        let checkout = tmp.path().join("external checkout");
        assert!(
            Command::new("git")
                .args([
                    "init",
                    "-q",
                    "--separate-git-dir",
                    external.to_str().unwrap(),
                    checkout.to_str().unwrap()
                ])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            primary_checkout(&checkout)
                .unwrap_err()
                .to_string()
                .contains("external")
        );
    }

    fn git_output(repo: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(["-C", repo.to_str().unwrap()])
            .args(args)
            .output()
            .unwrap()
    }
}
