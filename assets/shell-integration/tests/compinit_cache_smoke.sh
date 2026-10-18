#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
source "$SCRIPT_DIR/common.sh"

# kaku.zsh rebuilds the completion dump only when it is older than a day and
# loads it with `compinit -C` otherwise. The age test is a `(#q...)` glob
# qualifier, which zsh only honors with EXTENDED_GLOB; without it the test was
# always true and every shell paid for a full compinit.

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/kaku-compinit-cache.XXXXXX")"
cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

test_home="$tmp_dir/home"
mkdir -p "$test_home"

vendor_dir="$tmp_dir/vendor"
create_stub_vendor_dir "$vendor_dir"

setup_out=""
setup_status=0
setup_out="$(
  HOME="$test_home" \
  ZDOTDIR="$test_home" \
  KAKU_INIT_INTERNAL=1 \
  KAKU_SKIP_TOOL_BOOTSTRAP=1 \
  KAKU_SKIP_TERMINFO_BOOTSTRAP=1 \
  KAKU_VENDOR_DIR="$vendor_dir" \
  bash "$REPO_ROOT/assets/shell-integration/setup_zsh.sh" --update-only 2>&1
)" || setup_status=$?
if [[ "$setup_status" -ne 0 ]]; then
  echo "compinit_cache: setup_zsh.sh failed (exit $setup_status):" >&2
  echo "$setup_out" >&2
  exit 1
fi

# Print the arguments kaku.zsh passes to compinit, and whether it leaked
# EXTENDED_GLOB into the user's shell.
compinit_call() {
  TERM=xterm-256color HOME="$test_home" ZDOTDIR="$test_home" zsh -f -c '
compinit() { print -r -- "__KAKU_COMPINIT__:[$*]" }
source "$HOME/.config/kaku/zsh/kaku.zsh"
[[ -o extendedglob ]] && print -r -- "__KAKU_EXTGLOB__:on" || print -r -- "__KAKU_EXTGLOB__:off"
' 2>&1
}

expect() {
  local label="$1" out="$2" want="$3"
  case "$out" in
    *"$want"*) ;;
    *)
      echo "compinit_cache: $label: expected $want in:" >&2
      echo "$out" >&2
      exit 1
      ;;
  esac
}

dump="$test_home/.zcompdump"

# A fresh dump is loaded from cache.
touch "$dump"
out="$(compinit_call)"
expect "fresh dump" "$out" "__KAKU_COMPINIT__:[-C]"
expect "fresh dump" "$out" "__KAKU_EXTGLOB__:off"

# A dump older than a day is rebuilt.
touch -t 202001010000 "$dump"
out="$(compinit_call)"
expect "stale dump" "$out" "__KAKU_COMPINIT__:[]"

# No dump yet: compinit -C writes one, the same as the original intent.
rm -f "$dump"
out="$(compinit_call)"
expect "missing dump" "$out" "__KAKU_COMPINIT__:[-C]"

# Exercise the real compinit: an unchanged dump does not get rewritten by
# a full check. After that check, the following shell must use the cache.
real_compinit_call() {
  TERM=xterm-256color HOME="$test_home" ZDOTDIR="$test_home" zsh -f -c '
autoload -Uz compinit
autoload +X compinit
functions[_kaku_test_compinit]=$functions[compinit]
compinit() {
  print -r -- "__KAKU_COMPINIT__:[$*]"
  _kaku_test_compinit "$@"
}
source "$HOME/.config/kaku/zsh/kaku.zsh"
[[ -o extendedglob ]] && print -r -- "__KAKU_EXTGLOB__:on" || print -r -- "__KAKU_EXTGLOB__:off"
' 2>&1
}

out="$(real_compinit_call)"
expect "real missing dump" "$out" "__KAKU_COMPINIT__:[-C]"
[[ -s "$dump" ]] || { echo "compinit_cache: real compinit did not create a dump" >&2; exit 1; }
touch -t 202001010000 "$dump"
out="$(real_compinit_call)"
expect "real stale dump" "$out" "__KAKU_COMPINIT__:[]"
expect "real stale dump" "$out" "__KAKU_EXTGLOB__:off"
out="$(real_compinit_call)"
expect "next shell after full check" "$out" "__KAKU_COMPINIT__:[-C]"

# A rejected full check must remain stale so the next shell retries it.
touch -t 202001010000 "$dump"
out="$(TERM=xterm-256color HOME="$test_home" ZDOTDIR="$test_home" zsh -f -c '
compinit() { return 1 }
source "$HOME/.config/kaku/zsh/kaku.zsh"
_kaku_zcompdump_stale && print -r -- "__KAKU_DUMP__:stale"
' 2>&1)"
expect "failed full check" "$out" "__KAKU_DUMP__:stale"

echo "compinit_cache smoke test passed"
