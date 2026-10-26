#!/usr/bin/env python3
"""Name the verification a change needs from the paths it touches, never from who wrote it.

    python3 scripts/risk_tier.py [path ...]

With no paths it reads the change not yet upstream: commits since the merge
base with @{upstream} (origin/main when there is none) plus staged, unstaged
and untracked files. Prints one line per tier the change reaches, highest
first, and nothing when the default checks for the change type (AGENTS.md
Verification) are enough. Advice only: it always exits 0 and never writes a
file, since most of the gates it names need a built app, a release run or the
maintainer, which a script cannot prove ran.
"""

from __future__ import annotations

import fnmatch
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

# Highest first. A path takes every tier it matches. fnmatch's `*` also
# matches `/`, so a directory pattern covers its whole subtree.
TIERS = [
    ("release",
     "make fmt && make check && make test, then make app; Build Validation green on HEAD; "
     "the pre-release smoke checklist in .agents/skills/release/SKILL.md; the maintainer's review of the diff", [
         "scripts/release.sh", "scripts/notarize.sh", "scripts/prep_release.sh", "scripts/nightly.sh",
         "scripts/build.sh", "scripts/check_release_notes.sh", "scripts/check_release_config.sh",
         "scripts/check_config_release_readiness.sh", "kaku/Cargo.toml", "kaku-gui/Cargo.toml",
         "rust-toolchain.toml", ".github/RELEASE_NOTES.md",
     ]),
    ("update",
     "sweep every update entry path (menu Check for Updates, Restart to Update, toast, CLI direct ZIP, brew cask) "
     "and keep user_facing_update_events_route_through_confirm and brew_and_direct_paths_confirm_before_replace green; "
     "update a built app from the previous release by hand", [
         "scripts/update_helper.sh", "kaku/src/update.rs", "kaku-gui/src/update.rs",
         "crates/wezterm-toast-notification/src/macos.rs",
     ]),
    ("workflow",
     "watch the pushed run with gh run watch <id> --exit-status; keep checks.yml at five macOS jobs", [
         ".github/workflows/*",
     ]),
    ("config",
     "./scripts/check_config_release_readiness.sh and ./scripts/check_release_config.sh; migration code only "
     "for keys the previous release had (docs/config-versions.md)", [
         "assets/shell-integration/config_version.txt", "assets/shell-integration/config_update_highlights.tsv",
         "assets/shell-integration/check_config_version.sh", "assets/macos/Kaku.app/Contents/Resources/kaku.lua",
     ]),
    ("dotfiles",
     "the affected assets/shell-integration/tests/*_smoke.sh, then the list checks.yml runs; shellcheck "
     "--severity=error; write_through under a real write failure in every copy; reset keeps user-owned lines", [
         "assets/shell-integration/setup_zsh.sh", "assets/shell-integration/setup_fish.sh",
         "assets/shell-integration/install_cli_tools.sh", "kaku/src/init.rs", "kaku/src/reset.rs",
         "kaku/src/doctor.rs",
     ]),
]


def git(*args: str) -> list[str]:
    out = subprocess.run(["git", "-C", str(ROOT), *args], capture_output=True, text=True)
    return out.stdout.splitlines() if out.returncode == 0 else []


def pending_paths() -> list[str]:
    base = (git("merge-base", "HEAD", "@{upstream}") or git("merge-base", "HEAD", "origin/main") or [""])[0]
    paths = set(git("diff", "--name-only", base)) if base else set()
    paths.update(git("ls-files", "--others", "--exclude-standard"))
    return sorted(p for p in paths if p)


def tiers_for(paths: list[str]) -> list[tuple[str, str, list[str]]]:
    hits = []
    for name, gates, patterns in TIERS:
        matched = [p for p in paths if any(fnmatch.fnmatchcase(p, pattern) for pattern in patterns)]
        if matched:
            hits.append((name, gates, matched))
    return hits


def main(argv: list[str]) -> int:
    paths = argv or pending_paths()
    for name, gates, matched in tiers_for(paths):
        more = f" and {len(matched) - 1} more" if len(matched) > 1 else ""
        print(f"risk: {name} ({matched[0]}{more}) needs {gates}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
