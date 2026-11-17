//! `hp update` and the "newer release" line `doctor` prints.
//!
//! herdr-core has no plugin releases to install: `hp` is a checkout built with
//! cargo, and moving core is `hc update`'s job. Both hooks stay so the CLI and
//! `doctor` keep their shape.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::paths::Ctx;
use crate::runner::Runner;

/// `hp update`: nothing to fetch; say how this binary is built instead.
pub fn run(_ctx: &Ctx, _check_only: bool) -> Result<()> {
    println!("hp is built from a checkout: rebuild with `cargo build --release --locked` in it");
    Ok(())
}

/// A newer release than this binary, when one is known. Nothing is published
/// for this build, so there is never one to report.
pub fn newer_release(_runner: &dyn Runner, _root: Option<&Path>) -> Option<String> {
    None
}

/// The checkout this binary was built in, when it still exists.
pub fn own_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let target = exe.parent()?.parent()?;
    (target.file_name()? == "target").then(|| target.parent().map(Path::to_path_buf))?
}
