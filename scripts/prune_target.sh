#!/usr/bin/env bash
# Keeps a cargo target directory from growing without bound. cargo never
# removes old artifacts, and this repo builds the same code under several
# profiles, architectures and feature sets (remote on and off), so target/
# reached 92G. Past the cap the whole directory goes: deleting part of it by
# age can leave cargo with fingerprints that point at missing files, while a
# full clean only costs the next build its cache.
#
# Measuring a large target/ takes seconds, so this runs from the build entry
# points but measures at most once a day.
#
# Usage: scripts/prune_target.sh [target-dir]
#   KAKU_TARGET_CAP_GB  size that triggers a clean (default 40)

set -euo pipefail

dir="${1:-${CARGO_TARGET_DIR:-target}}"
cap_gb="${KAKU_TARGET_CAP_GB:-40}"

[[ -d "$dir" ]] || exit 0

stamp="$dir/.kaku-size-checked"
if [[ -f "$stamp" ]] && [[ -z "$(find "$stamp" -mmin +1440 2>/dev/null)" ]]; then
	exit 0
fi

size_kb="$(du -sk "$dir" 2>/dev/null | cut -f1)"
size_gb=$((size_kb / 1024 / 1024))

if ((size_gb >= cap_gb)); then
	echo "target: ${size_gb}G is over the ${cap_gb}G cap, cleaning it; this build starts from scratch."
	# cargo clean waits for any build holding the directory lock.
	cargo clean --target-dir "$dir" >/dev/null 2>&1 || rm -rf "$dir"
	mkdir -p "$dir"
fi

touch "$stamp"
