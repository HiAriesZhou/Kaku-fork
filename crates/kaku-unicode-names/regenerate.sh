#!/usr/bin/env bash
# Rebuild src/names.tsv.zst from the Unicode Character Database.
#
#   crates/kaku-unicode-names/regenerate.sh <ucd-dir>
#
# <ucd-dir> is an unpacked UCD.zip for the target Unicode version. The table
# used to ship as ~140k lines of Rust (`ucd-generate names`), which compiled
# into ~6.8 MB per architecture; the compressed table is ~0.25 MB and is
# decoded the first time the character selector opens.
set -euo pipefail

if [[ $# -ne 1 ]]; then
	echo "usage: $0 <ucd-dir>" >&2
	exit 1
fi

out="$(cd "$(dirname "$0")" && pwd)/src/names.tsv.zst"

# ucd-generate 0.3.1 prints `    ("NAME", 1234),` per entry, sorted by name.
ucd-generate names "$1" |
	awk -F'"' '/^    \("/ { split($3, a, /[ ,)]+/); printf "%s\t%X\n", $2, a[2] }' |
	zstd -q -19 -f -o "$out"

echo "wrote $out ($(zstd -lv "$out" 2>/dev/null | awk '/Decompressed Size/ {print $3, $4}'))"
