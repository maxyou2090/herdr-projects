# Release checklist

Nothing is published: `hp` is built from a checkout, and that is the whole
installation path ([`../README.md`](../README.md) `## Install`,
[`getting-started.md`](getting-started.md) `## 1. Build and install`). The list
below is the shape a release would take if prebuilt binaries were ever
published; it was written for the Herdr 1 plugin release, so its Herdr 1 notes
are history.

## Every release

- [ ] `version` agrees in `Cargo.toml` and `Cargo.lock`. (`hp --version` is the
      package version plus the git commit and build time, from `build.rs`.)
- [ ] `cargo build --release --locked` and `cargo test` pass on macOS and on Linux.
- [ ] Walk through the acceptance list. There is no herdr-core one yet: the
      Herdr 1 list is history (`docs/history/manual-test-herdr1.md`) and its
      client-witnessed checks describe a sidebar and popups that no longer exist.
- [ ] Tag `v<version>` on `port/herdr-core`, push the tag, and create the GitHub release with notes in user terms.
- [ ] The `Release binaries` workflow is gone: it built the Herdr 1 plugin
      binary (`herdr-plugin.toml`, `herdr-projects`) for four targets and could
      not run any more. Publishing means writing a new one for `hp` and
      `hp-agent`.
- [ ] On every machine that runs `hp` from a checkout: `git pull`, `cargo build --release --locked`, `hp doctor --fix`, then `hp ticker stop && hp ticker start`.

## Done

- [x] Public repository with the `herdr-plugin` topic, description and homepage set.
- [x] No "private repository" wording in `README.md` or `docs/getting-started.md`.
- [x] `v0.2.0`: the Herdr-native redesign.
- [x] **No prebuilt binaries.** Decided 2026-11-17: install only by building from
      source, as `README.md` and `docs/getting-started.md` already described. One
      person uses this fork and the code changes most days; `hp --version` is the
      package version plus the git commit and build time, so publishing binaries
      would mean maintaining a release version on top, packaging, uploading and
      keeping `scripts/install.sh` able to pick the right asset. Revisit when a
      second person wants to run it.

## Open

- [ ] `docs/history/herdr-notes-herdr1.md` and `docs/history/manual-test-herdr1.md`
      name the author's machines and home paths. Generalise them before
      publishing the history.
