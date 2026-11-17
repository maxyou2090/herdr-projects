#!/bin/sh
# Builds hp and hp-agent from this checkout and puts them on PATH.
# herdr-core has no plugin install: this is an ordinary program that calls `hc`.
set -eu

here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"

say() { printf 'hp install: %s\n' "$*" >&2; }

command -v cargo >/dev/null 2>&1 || { say "cargo is not on PATH (install Rust: https://rustup.rs)"; exit 1; }
say "building hp and hp-agent"
cargo build --release --locked || { say "cargo build failed"; exit 1; }

bin_dir=${XDG_BIN_HOME:-$HOME/.local/bin}
mkdir -p "$bin_dir"
for name in hp hp-agent; do
  link=$bin_dir/$name
  if [ -L "$link" ] || [ ! -e "$link" ]; then
    rm -f "$link"
    ln -s "$here/target/release/$name" "$link"
    say "$name -> $here/target/release/$name"
  else
    say "left $link alone: it is not a link"
  fi
done

say "done. run \`hp doctor\` to check the setup"
