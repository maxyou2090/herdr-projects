//! `hp update` and the "newer release" line `doctor` prints.
//!
//! herdr-core has no plugin releases to install: `hp` is a checkout built with
//! cargo, and moving core is `hc update`'s job. So there is no release feed to
//! fetch from and nothing this command could download — it says what the build
//! is and how to move it, and `--check` says the same without changing anything.
//! Both hooks stay so the CLI and `doctor` keep their shape.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// `hp update`: nothing to fetch; say which build this is and how it moves.
pub fn run(check_only: bool) -> Result<()> {
    let checkout = own_root().map(|path| path.display().to_string()).unwrap_or_else(|| "an unknown checkout".to_string());
    if check_only {
        println!("version:  {} (built from a checkout, not a release)", crate::VERSION);
        println!("checkout: {checkout}");
        println!("nothing to check: there is no release feed for this build");
        println!("to update it: `git pull`, `cargo build --release --locked`, then `ticker stop && ticker start`");
        return Ok(());
    }
    println!("hp {} comes from a checkout ({checkout}); there is nothing to fetch", crate::VERSION);
    println!("to update it: `git pull`, `cargo build --release --locked`, then `ticker stop && ticker start`");
    Ok(())
}

/// The checkout this binary was built in, when it still exists.
pub fn own_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let target = exe.parent()?.parent()?;
    (target.file_name()? == "target").then(|| target.parent().map(Path::to_path_buf))?
}
