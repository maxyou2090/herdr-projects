#!/usr/bin/env python3
"""Turn the runtime state a Herdr 1 backup restored into a projects root into
state herdr-core can use, keeping every report and every note.

What it touches (see docs/herdr-core-port.md §7):

  .state/lanes.json        keeps the default lane, clears its recorded pane
  .state/coordinator*.json deletes them; the next `hp open` writes a fresh one
  threads/t-*.toml         clears the pane, tab and Space ids, resolves what was
                           still open, keeps reports, memory and tasks
  .state/ticker.json       moved into .state.pre-core/ (stale pane ids, nudge hash)
  .state/lock              moved too: the process that held it is gone
  .ticker.lock             moved too

Everything else — PROJECT.md, TASKS.md, MEMORY.md, memory/, routines/,
threads/*.md — is left byte for byte. Run it on a copy first:

  cp -R ~/backup/root /tmp/root-try && scripts/migrate-from-herdr1-state.py /tmp/root-try
  scripts/migrate-from-herdr1-state.py ~/.herdr-projects
"""

import json
import pathlib
import re
import shutil
import sys


def move_aside(state: pathlib.Path, name: str) -> bool:
    source = state / name
    if not source.exists():
        return False
    aside = state / ".state.pre-core"
    aside.mkdir(exist_ok=True)
    shutil.move(str(source), str(aside / name))
    return True


def rewrite_lanes(state: pathlib.Path) -> int:
    lanes_path = state / "lanes.json"
    if not lanes_path.exists():
        return 0
    registry = json.loads(lanes_path.read_text())
    lanes = registry.get("lanes", [])
    default = next((lane for lane in lanes if lane.get("name") == "default"), None)
    if default is None:
        lanes_path.unlink()
        return 0
    default["pane_id"] = ""
    default["status"] = "open"
    lanes_path.write_text(json.dumps({"lanes": [default]}, ensure_ascii=False, indent=2) + "\n")
    return 1


def rewrite_threads(project: pathlib.Path) -> tuple[int, int]:
    seen = resolved = 0
    for path in sorted((project / "threads").glob("*.toml")):
        if not re.fullmatch(r"t-[0-9]+\.toml", path.name):
            continue
        seen += 1
        lines = []
        for line in path.read_text().splitlines():
            key = line.split("=", 1)[0].strip()
            if key in {"workspace_id", "repo_workspace", "own_workspace", "tab_id", "pane_id"}:
                lines.append(f'{key} = ""')
                continue
            if key == "status":
                value = line.split("=", 1)[1].strip().strip('"')
                if value not in {"resolved", "failed"}:
                    lines.append('status = "resolved"')
                    resolved += 1
                    continue
            lines.append(line)
        path.write_text("\n".join(lines) + "\n")
    return seen, resolved


def main(root: pathlib.Path) -> int:
    if not root.is_dir():
        print(f"{root}: not a folder", file=sys.stderr)
        return 1
    projects = [p for p in sorted(root.iterdir()) if p.is_dir() and (p / "PROJECT.md").is_file()]
    if not projects:
        print(f"{root}: no project (a folder with PROJECT.md) to migrate", file=sys.stderr)
        return 1
    print(f"{'project':24} {'lanes':>5} {'threads':>7} {'resolved':>8}")
    for project in projects:
        state = project / ".state"
        state.mkdir(exist_ok=True)
        lanes = rewrite_lanes(state)
        for name in ("coordinator.json", "coordinators.json", "ticker.json", "lock"):
            move_aside(state, name)
        threads, resolved = rewrite_threads(project)
        print(f"{project.name:24} {lanes:5} {threads:7} {resolved:8}")
    move_aside(root / ".state", "nothing")  # no-op guard for a stray .state at the root
    ticker_lock = root / ".ticker.lock"
    if ticker_lock.exists():
        shutil.move(str(ticker_lock), str(root / ".ticker.lock.pre-core"))
    print("\nrun `hp doctor` and then `hp open <project>`; every report and note is untouched")
    return 0


if __name__ == "__main__":
    sys.exit(main(pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "~/.herdr-projects").expanduser()))
