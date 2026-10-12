//! Prepare local worktrees with per-skill links from the repository's primary checkout.

use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Link valid primary-checkout skills into a newly placed local worktree.
pub fn prepare(repo: &Path, worktree: &Path) -> Result<()> {
    let primary = match primary_checkout(repo) {
        Ok(primary) => primary,
        Err(_error) if !repo.join(".agents/skills").exists() => return Ok(()),
        Err(error) => return Err(error),
    };
    let source = primary.join(".agents/skills");
    match fs::symlink_metadata(&source) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            bail!(
                "{} must be a real .agents/skills directory",
                source.display()
            );
        }
        Ok(_) => {}
    }

    let destination = worktree.join(".agents/skills");
    let parent = worktree.join(".agents");
    check_directory(&parent)?;
    check_directory(&destination)?;

    let repo_root = fs::canonicalize(worktree).context("could not resolve worktree repository")?;
    let tracked = tracked_paths(&repo_root)?;
    let mut plan = Vec::new();
    let entries =
        fs::read_dir(&source).with_context(|| format!("could not read {}", source.display()))?;
    for entry in entries {
        let entry = entry?;
        let from = entry.path();
        let name = entry.file_name();
        let kind = entry.file_type()?;
        if kind.is_symlink() && from.join("SKILL.md").is_file() {
            bail!(
                "skill source directory must not be a symlink: {}",
                from.display()
            );
        }
        if !kind.is_dir() || !from.join("SKILL.md").is_file() {
            continue;
        }
        validate_tree(&from)?;
        let to = destination.join(&name);
        let relative = PathBuf::from(".agents/skills").join(&name);
        if tracked.contains(&relative) || tracked.iter().any(|p| p.starts_with(&relative)) {
            bail!(
                "refusing to replace tracked skill path {}",
                relative.display()
            );
        }
        match fs::symlink_metadata(&to) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let actual = fs::canonicalize(&to)
                    .with_context(|| format!("broken skill link {}", to.display()))?;
                let expected = fs::canonicalize(&from)?;
                if actual != expected {
                    bail!(
                        "skill link conflict at {}; expected {}",
                        to.display(),
                        expected.display()
                    );
                }
                plan.push((from, to, false));
            }
            Ok(_) => bail!(
                "skill path already exists; refusing to replace {}",
                to.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                plan.push((from, to, true))
            }
            Err(error) => return Err(error.into()),
        }
    }

    // No filesystem writes occur until every source and destination has passed preflight.
    if plan.iter().any(|(_, _, create)| *create) {
        fs::create_dir_all(&destination)
            .with_context(|| format!("could not create {}", destination.display()))?;
    }
    for (from, to, create) in plan {
        if create {
            let target = fs::canonicalize(&from)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &to)
                .with_context(|| format!("could not create skill link {}", to.display()))?;
            #[cfg(not(unix))]
            bail!(
                "skill links are not supported on this platform: {}",
                to.display()
            );
        }
    }
    Ok(())
}

fn primary_checkout(repo: &Path) -> Result<PathBuf> {
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
            common.display()
        );
    }
    let check = git_value(
        primary
            .to_str()
            .context("primary checkout path is not UTF-8")?,
        "--show-toplevel",
    )?;
    if fs::canonicalize(check)? != fs::canonicalize(primary)? {
        bail!("could not verify primary checkout at {}", primary.display());
    }
    Ok(primary.to_path_buf())
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

fn check_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            bail!("refusing to use non-directory {}", path.display());
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn tracked_paths(repo_root: &Path) -> Result<std::collections::HashSet<PathBuf>> {
    let output = Command::new("git")
        .args([
            "-C",
            repo_root.to_str().context("repository path is not UTF-8")?,
            "ls-files",
            "-z",
            "--",
            ".agents/skills",
        ])
        .output()
        .context("could not inspect tracked skill paths")?;
    if !output.status.success() {
        bail!("git ls-files failed in {}", repo_root.display());
    }
    Ok(output
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(String::from_utf8_lossy(p).into_owned()))
        .collect())
}

/// Verify all symlinks within a skill resolve within its root and no directory cycle exists.
fn validate_tree(skill: &Path) -> Result<()> {
    let root = fs::canonicalize(skill)
        .with_context(|| format!("could not resolve skill {}", skill.display()))?;
    let mut pending = vec![skill.to_path_buf()];
    let mut visited = std::collections::HashSet::new();
    while let Some(dir) = pending.pop() {
        let canonical = fs::canonicalize(&dir)
            .with_context(|| format!("broken path in skill {}", dir.display()))?;
        if !canonical.starts_with(&root) {
            bail!("skill path escapes its directory: {}", dir.display());
        }
        if !visited.insert(canonical.clone()) {
            bail!("recursive directory link in skill {}", dir.display());
        }
        for entry in fs::read_dir(&dir)
            .with_context(|| format!("could not read skill directory {}", dir.display()))?
        {
            let path = entry?.path();
            let meta = fs::symlink_metadata(&path)?;
            if meta.file_type().is_symlink() {
                let resolved = fs::canonicalize(&path)
                    .with_context(|| format!("broken link in skill {}", path.display()))?;
                if !resolved.starts_with(&root) {
                    bail!("skill link escapes its directory: {}", path.display());
                }
                if resolved.is_dir() {
                    pending.push(path);
                }
            } else if meta.is_dir() {
                pending.push(path);
            }
        }
    }
    Ok(())
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
        Command::new("git")
            .args(["init", "-q", primary.to_str().unwrap()])
            .status()
            .unwrap();
        Command::new("git")
            .args([
                "-C",
                primary.to_str().unwrap(),
                "config",
                "user.email",
                "test@example.invalid",
            ])
            .status()
            .unwrap();
        Command::new("git")
            .args([
                "-C",
                primary.to_str().unwrap(),
                "config",
                "user.name",
                "Test",
            ])
            .status()
            .unwrap();
        fs::write(primary.join("README.md"), "base").unwrap();
        Command::new("git")
            .args(["-C", primary.to_str().unwrap(), "add", "README.md"])
            .status()
            .unwrap();
        Command::new("git")
            .args(["-C", primary.to_str().unwrap(), "commit", "-qm", "base"])
            .status()
            .unwrap();
        fs::create_dir_all(primary.join(".agents/skills/sample/references")).unwrap();
        fs::write(primary.join(".agents/skills/sample/SKILL.md"), "sample").unwrap();
        fs::write(
            primary.join(".agents/skills/sample/references/note.md"),
            "note",
        )
        .unwrap();
        Command::new("git")
            .args([
                "-C",
                primary.to_str().unwrap(),
                "worktree",
                "add",
                "--detach",
                "--no-checkout",
                worktree.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        fs::create_dir_all(worktree.join(".agents/skills/local")).unwrap();
        fs::write(worktree.join(".agents/skills/local/SKILL.md"), "local").unwrap();
        Fixture {
            _tmp: tmp,
            primary,
            worktree,
        }
    }

    #[test]
    fn links_skills_idempotently_and_keeps_local_skills() {
        let f = fixture();
        prepare(&f.worktree, &f.worktree).unwrap();
        prepare(&f.worktree, &f.worktree).unwrap();
        assert_eq!(
            fs::read_to_string(f.worktree.join(".agents/skills/sample/references/note.md"))
                .unwrap(),
            "note"
        );
        assert!(f.worktree.join(".agents/skills/local/SKILL.md").is_file());
        assert_eq!(
            fs::canonicalize(f.worktree.join(".agents/skills/sample")).unwrap(),
            fs::canonicalize(f.primary.join(".agents/skills/sample")).unwrap()
        );
    }

    #[test]
    fn refuses_existing_items_and_broken_or_escaping_source_links() {
        let f = fixture();
        let later = f.primary.join(".agents/skills/z-later");
        fs::create_dir_all(&later).unwrap();
        fs::write(later.join("SKILL.md"), "later").unwrap();
        fs::create_dir_all(f.worktree.join(".agents/skills/z-later")).unwrap();
        assert!(prepare(&f.worktree, &f.worktree).is_err());
        assert!(
            !f.worktree.join(".agents/skills/sample").exists(),
            "preflight must not leave earlier links behind"
        );

        let f = fixture();
        fs::create_dir_all(f.worktree.join(".agents/skills/sample")).unwrap();
        fs::write(f.worktree.join(".agents/skills/sample/SKILL.md"), "keep").unwrap();
        assert!(
            prepare(&f.worktree, &f.worktree)
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        assert_eq!(
            fs::read_to_string(f.worktree.join(".agents/skills/sample/SKILL.md")).unwrap(),
            "keep"
        );

        let f = fixture();
        symlink(
            f.primary.join("missing"),
            f.primary.join(".agents/skills/sample/broken"),
        )
        .unwrap();
        assert!(prepare(&f.worktree, &f.worktree).is_err());
        let f = fixture();
        symlink(
            f.primary.join("README.md"),
            f.primary.join(".agents/skills/sample/outside"),
        )
        .unwrap();
        assert!(
            prepare(&f.worktree, &f.worktree)
                .unwrap_err()
                .to_string()
                .contains("escapes")
        );

        let f = fixture();
        symlink(
            f.primary.join(".agents/skills/sample"),
            f.primary.join(".agents/skills/sample/loop"),
        )
        .unwrap();
        assert!(
            prepare(&f.worktree, &f.worktree)
                .unwrap_err()
                .to_string()
                .contains("recursive")
        );
    }

    #[test]
    fn refuses_tracked_skill_paths_without_creating_or_replacing_files() {
        let f = fixture();
        let mut child = Command::new("git")
            .args([
                "-C",
                f.primary.to_str().unwrap(),
                "hash-object",
                "-w",
                "--stdin",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(b"tracked").unwrap();
        let hash = child.wait_with_output().unwrap();
        let object = String::from_utf8(hash.stdout).unwrap();
        Command::new("git")
            .args([
                "-C",
                f.worktree.to_str().unwrap(),
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{},.agents/skills/sample/SKILL.md", object.trim()),
            ])
            .status()
            .unwrap();
        assert!(
            prepare(&f.worktree, &f.worktree)
                .unwrap_err()
                .to_string()
                .contains("tracked skill path")
        );
        assert!(!f.worktree.join(".agents/skills/sample").exists());
    }

    #[test]
    fn missing_skills_are_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("not a git repo");
        fs::create_dir_all(&repo).unwrap();
        prepare(&repo, &repo).unwrap();
        assert!(!repo.join(".agents").exists());
    }

    #[test]
    fn finds_primary_checkout_when_input_is_a_linked_worktree() {
        let f = fixture();
        let nested = f._tmp.path().join("nested");
        let primary = f.primary.to_string_lossy().into_owned();
        Command::new("git")
            .args([
                "-C",
                &primary,
                "worktree",
                "add",
                "--detach",
                nested.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert_eq!(
            primary_checkout(&nested).unwrap(),
            fs::canonicalize(&f.primary).unwrap()
        );
        prepare(&nested, &f.worktree).unwrap();
        assert_eq!(
            fs::canonicalize(f.worktree.join(".agents/skills/sample")).unwrap(),
            fs::canonicalize(f.primary.join(".agents/skills/sample")).unwrap()
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
}
