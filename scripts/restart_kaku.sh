#!/usr/bin/env bash
# Restarts Kaku the way an in-app update does: TERM every running Kaku GUI
# process, force-kill any left after 20s, then open the given app. Tabs and
# working directories come back from the session snapshot Kaku saves while it
# runs. A normal quit is not used because it stops at the quit confirmation
# whenever a program such as Claude Code is running.
#
# The script detaches first, so it survives the terminal it was started from
# closing, including when that terminal is Kaku itself, and it waits a few
# seconds so whoever asked from inside Kaku can finish their reply.
#
# Usage: scripts/restart_kaku.sh [/path/to/Kaku.app]
#   default app            /Applications/Kaku.app
#   KAKU_RESTART_DELAY     seconds before quitting (default 5)

set -euo pipefail

app="${1:-/Applications/Kaku.app}"
app="${app%/}"
gui="$app/Contents/MacOS/kaku-gui"
log="$HOME/Library/Logs/kaku-restart.log"
delay="${KAKU_RESTART_DELAY:-5}"

[[ -x "$gui" ]] || {
	echo "No Kaku app at $app" >&2
	exit 1
}

if [[ "${KAKU_RESTART_DETACHED:-}" != 1 ]]; then
	self="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
	# A new session, so closing Kaku's terminals (SIGHUP to their sessions)
	# does not reach this script.
	KAKU_RESTART_DETACHED=1 /usr/bin/nohup /usr/bin/perl -MPOSIX -e 'POSIX::setsid(); exec @ARGV' \
		/bin/bash "$self" "$app" >>"$log" 2>&1 </dev/null &
	echo "Restarting $app in ${delay}s; log: $log"
	exit 0
fi

note() { printf '[%s] %s\n' "$(date '+%F %T')" "$*"; }

# Every running Kaku GUI, whichever bundle it came from: a copy opened from
# dist/ is still the Kaku being restarted, and leaving it running would end
# with two of them. Matched on the executable alone, never the arguments, so
# a shell whose command line mentions the path is not taken for Kaku.
gui_pids() {
	ps -axww -o pid= -o comm= | while read -r pid exe; do
		case "$exe" in
		*/Contents/MacOS/kaku-gui) printf '%s\n' "$pid" ;;
		esac
	done
}

# Only the target bundle, to confirm it came back.
target_pids() {
	ps -axww -o pid= -o args= | while read -r pid args; do
		case "$args" in
		"$gui" | "$gui "*) printf '%s\n' "$pid" ;;
		esac
	done
}

sleep "$delay"
before="$(ps -o pid=,args= -p "$(gui_pids | paste -sd, -)" 2>/dev/null | awk '{print $1, $2}' | paste -sd ';' -)"
note "restart into $app; running: ${before:-none}"

# Every Kaku under this HOME saves the same session file when it quits, so
# only the one that owns the snapshot (its pid names the snapshot's content
# directory) gets TERM and saves; the others are killed after it, unsaved,
# so a stray copy cannot replace the tabs. Without an owner, all get TERM.
session="${XDG_CONFIG_HOME:-$HOME/.config}/kaku/last_session.json"
owner="$(sed -n 's/.*"content_dir"[[:space:]]*:[[:space:]]*"session-\([0-9][0-9]*\)-.*/\1/p' "$session" 2>/dev/null | head -1)"
running="$(gui_pids)"
# Not `gui_pids | grep -q`: grep stops at the first match, gui_pids then dies
# of SIGPIPE, and pipefail would read the match as a failure.
if [[ -n "$owner" ]] && grep -qx "$owner" <<<"$running"; then
	quit_first="$owner"
else
	quit_first="$running"
fi
for _ in $(seq 1 20); do
	alive=""
	for pid in $quit_first; do
		kill -0 "$pid" 2>/dev/null && alive="$alive $pid"
	done
	[[ -z "$alive" ]] && break
	for pid in $alive; do kill -TERM "$pid" 2>/dev/null || true; done
	sleep 1
done
for pid in $(gui_pids); do
	note "force-killing $pid"
	kill -KILL "$pid" 2>/dev/null || true
done

sleep 1
/usr/bin/open "$app"
for _ in $(seq 1 30); do
	after="$(target_pids | tr '\n' ' ')"
	if [[ -n "$after" ]]; then
		note "running again: $after"
		exit 0
	fi
	sleep 0.5
done
note "Kaku did not come back after open; open $app by hand"
exit 1
