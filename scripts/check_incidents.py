#!/usr/bin/env python3
"""Every row of docs/incidents.md names real commits, a known layer and a catcher this repository defines.

    python3 scripts/check_incidents.py [--ledger PATH]

The incident table is the eval set for the review layers (docs/incidents.md
says why and what each layer means). A row fails when its ID repeats, its
Reported cell is not a date, its Fixed commit is missing or does not resolve,
its Introduced commit is neither `unknown` nor a commit, its layer is not one
of LAYERS, or a backticked catcher is defined nowhere: not a Rust #[test]
function, a Python test_* function, a tracked file or a workflow step name.

In a shallow clone the commits cannot be resolved and are only shape-checked,
which the output says. The Scripts and Release Checks job checks out full
history, so CI resolves them. Read only: it never writes a file.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
LEDGER = ROOT / "docs" / "incidents.md"
LAYERS = ("write-time", "unit", "guard", "ci", "review", "build-validation",
          "release-preflight", "hand-test", "field")
COLUMNS = ["ID", "Reported", "Symptom", "Introduced", "Fixed", "Layer", "Caught by"]
SHA = re.compile(r"^[0-9a-f]{7,40}$")
# Vendored upstream crates are not Kaku's catchers.
SKIP_PREFIXES = ("deps/",)
RUST_TEST_ATTR = re.compile(r"^\s*#\[(?:[A-Za-z_][A-Za-z0-9_]*::)*test\b")
RUST_ATTR = re.compile(r"^\s*#\[")
RUST_FN = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)\s*[(<]")
PY_TEST = re.compile(r"^\s*def\s+(test_[A-Za-z0-9_]*)\s*\(", re.MULTILINE)
STEP_NAME = re.compile(r"^\s*-?\s*name:\s*(.+?)\s*$")


def git(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["git", "-C", str(ROOT), *args], capture_output=True, text=True)


def tracked_files() -> list[str]:
    out = git("ls-files")
    return [p for p in out.stdout.splitlines() if p and not p.startswith(SKIP_PREFIXES)]


def rust_tests(text: str) -> set[str]:
    """Functions whose attribute block includes #[test] or a path ending in ::test."""
    names, is_test = set(), False
    for line in text.splitlines():
        if RUST_TEST_ATTR.match(line):
            is_test = True
            continue
        if RUST_ATTR.match(line) or not line.strip() or line.strip().startswith("//"):
            continue
        match = RUST_FN.match(line)
        if match and is_test:
            names.add(match.group(1))
        is_test = False
    return names


def workflow_steps(text: str) -> set[str]:
    steps = set()
    for line in text.splitlines():
        match = STEP_NAME.match(line)
        if match:
            steps.add(match.group(1).strip("'\""))
    return steps


def defined_catchers(files: list[str]) -> set[str]:
    names: set[str] = set(files)
    for path in files:
        if path.endswith(".rs"):
            names.update(rust_tests((ROOT / path).read_text("utf-8", errors="replace")))
        elif path.endswith(".py"):
            names.update(PY_TEST.findall((ROOT / path).read_text("utf-8", errors="replace")))
        elif path.startswith(".github/workflows/") and path.endswith((".yml", ".yaml")):
            names.update(workflow_steps((ROOT / path).read_text("utf-8", errors="replace")))
    return names


def rows(text: str):
    """The table under '## Incidents' as (line number, cells), header and rule excluded."""
    section = False
    for number, line in enumerate(text.splitlines(), 1):
        if line.startswith("## "):
            section = line.strip() == "## Incidents"
            continue
        if section and line.startswith("|"):
            cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
            if cells == COLUMNS or set("".join(cells)) <= set("-: "):
                continue
            yield number, cells


def is_shallow() -> bool:
    return git("rev-parse", "--is-shallow-repository").stdout.strip() == "true"


def resolves(sha: str) -> bool:
    return git("cat-file", "-e", sha + "^{commit}").returncode == 0


def problems(text: str, label: str, defined: set[str], check_commits: bool) -> list[str]:
    found: list[str] = []
    seen: set[str] = set()
    for number, cells in rows(text):
        where = f"{label}:{number}"
        if len(cells) != len(COLUMNS):
            found.append(f"{where}: expected {len(COLUMNS)} cells, found {len(cells)}")
            continue
        row = dict(zip(COLUMNS, cells))
        if row["ID"] in seen:
            found.append(f"{where}: {row['ID']} repeats")
        seen.add(row["ID"])
        if not re.fullmatch(r"\d{4}-\d{2}-\d{2}", row["Reported"]):
            found.append(f"{where}: Reported is not a date: {row['Reported']}")
        for column, optional in (("Fixed", False), ("Introduced", True)):
            value = row[column]
            if optional and value == "unknown":
                continue
            if not SHA.match(value):
                found.append(f"{where}: {column} is not a commit: {value}")
            elif check_commits and not resolves(value):
                found.append(f"{where}: {column} {value} does not resolve")
        if row["Layer"] not in LAYERS:
            found.append(f"{where}: layer {row['Layer']} is not one of {', '.join(LAYERS)}")
        catchers = re.findall(r"`([^`]+)`", row["Caught by"])
        if not catchers:
            found.append(f"{where}: no catcher named")
        found.extend(
            f"{where}: catcher `{name}` is not a test, tracked file or workflow step"
            for name in catchers
            if name not in defined
        )
    return found


def main(argv: list[str]) -> int:
    ledger = LEDGER
    if len(argv) == 2 and argv[0] == "--ledger":
        ledger = Path(argv[1]).resolve()
    elif argv:
        print("usage: check_incidents.py [--ledger PATH]", file=sys.stderr)
        return 2
    text = ledger.read_text("utf-8")
    try:
        label = str(ledger.relative_to(ROOT))
    except ValueError:
        label = str(ledger)
    shallow = is_shallow()
    found = problems(text, label, defined_catchers(tracked_files()), check_commits=not shallow)
    for line in found:
        print(line)
    if found:
        return 1
    note = ", commits shape-checked only (shallow clone)" if shallow else ""
    print(f"incidents ok: {sum(1 for _ in rows(text))} rows{note}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
