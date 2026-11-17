//! Talking to the Herdr desktop app, the window over herdr-core.
//!
//! A terminal started with `hc` stays out of that window unless it carries
//! `herdr.space`, and the supported way in is the app's own commands:
//! `herdr-app do space.new` and `herdr-app do tab.new` answer with the ids the
//! app gave the space, the tab and the terminal. This module is where those
//! calls live; a machine without the app falls back to plain `hc` and the
//! terminals are reached with `hc attach`.

use std::time::Duration;

use crate::runner::{Cmd, Runner};

/// The app answers only while its window is open; `status` is that check.
pub fn running(bin: &str, runner: &dyn Runner) -> bool {
    runner
        .run(&Cmd::new(bin, Duration::from_secs(5)).arg("status"))
        .map(|out| out.success())
        .unwrap_or(false)
}
