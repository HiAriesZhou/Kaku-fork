#!/usr/bin/env bash
# Refuses an app bundle, and optionally its DMG, that carries the private web
# terminal (the `remote` feature). Every public publish path runs this, so a
# CARGO_FEATURES left in the shell or a crate-qualified feature such as
# kaku/remote cannot ship it.
#
# Usage: scripts/check_no_web_terminal.sh <Kaku.app> [Kaku.dmg]

set -euo pipefail

# Both strings live only in crates/kaku-remote: the GUI logs the first, the
# CLI's `kaku remote` reads the second.
MARKERS='kaku-remote: starting on|kaku-remote\.json'

check_app() {
	local app="$1" bin found=0
	[[ -d "$app/Contents/MacOS" ]] || {
		echo "No app bundle at $app" >&2
		return 1
	}
	for bin in "$app"/Contents/MacOS/*; do
		[[ -f "$bin" ]] || continue
		if grep -aqE "$MARKERS" "$bin"; then
			echo "$bin contains the web terminal; refusing to publish it." >&2
			found=1
		fi
	done
	return "$found"
}

app="${1:?usage: $0 <Kaku.app> [Kaku.dmg]}"
dmg="${2:-}"

check_app "$app"

if [[ -n "$dmg" ]]; then
	mount_dir="$(mktemp -d /tmp/kaku-dmg-check.XXXXXX)"
	if ! hdiutil attach -nobrowse -readonly -mountpoint "$mount_dir" "$dmg" >/dev/null; then
		rmdir "$mount_dir" 2>/dev/null || true
		echo "Could not mount $dmg to check it before publishing." >&2
		exit 1
	fi
	status=0
	check_app "$mount_dir/Kaku.app" || status=1
	hdiutil detach "$mount_dir" >/dev/null
	rmdir "$mount_dir" 2>/dev/null || true
	exit "$status"
fi
