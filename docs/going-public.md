# Release checklist

This file was written for the Herdr 1 plugin release. On herdr-core `hp` is
built from a checkout and nothing is published, so most of the list below has no
counterpart yet; it is kept as the shape a release would take.

## Every release

- [ ] `version` agrees in `Cargo.toml`, `herdr-plugin.toml` and `Cargo.lock`.
- [ ] `cargo build --release --locked` and `cargo test` pass on macOS and on Linux.
- [ ] Walk through the acceptance list. There is no herdr-core one yet: the
      Herdr 1 list is history (`docs/history/manual-test-herdr1.md`) and its
      client-witnessed checks describe a sidebar and popups that no longer exist.
- [ ] Tag `v<version>` on `port/herdr-core`, push the tag, and create the GitHub release with notes in user terms.
- [ ] The `Release binaries` workflow passes: it attaches the four binaries and `SHA256SUMS` to the release, and fails when one is missing.
- [ ] On every machine that runs `hp` from a checkout: `git pull`, `cargo build --release --locked`, `hp doctor --fix`, then `hp ticker stop && hp ticker start`.

## Done

- [x] Public repository with the `herdr-plugin` topic, description and homepage set.
- [x] No "private repository" wording in `README.md` or `docs/getting-started.md`.
- [x] `v0.2.0`: the Herdr-native redesign.

## Open

- [ ] There is no install path to verify any more: `herdr plugin install
      eliasstravik/herdr-projects` belonged to the Herdr 1 plugin and `hp` is a
      cargo build of this checkout. `hp update` says so; decide whether to
      publish the build at all.
- [ ] `docs/history/herdr-notes-herdr1.md` and `docs/history/manual-test-herdr1.md`
      name the author's machines and home paths. Generalise them before
      publishing the history.
