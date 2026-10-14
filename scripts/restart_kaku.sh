#!/usr/bin/env bash
# Restarts the running Kaku the way an in-app update does: TERM its GUI
# processes, force-kill any left after 20s, then open the app again. Tabs and
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

# The GUI processes of this bundle only; pattern-free, since paths may hold
# characters pgrep would read as a regex.
gui_pids() {
	ps -axww -o pid= -o args= | while read -r pid args; do
		case "$args" in
		"$gui" | "$gui "*) printf '%s\n' "$pid" ;;
		esac
	done
}

sleep "$delay"
before="$(gui_pids | tr '\n' ' ')"
note "restart $app; running: ${before:-none}"

for _ in $(seq 1 20); do
	pids="$(gui_pids)"
	[[ -z "$pids" ]] && break
	for pid in $pids; do kill -TERM "$pid" 2>/dev/null || true; done
	sleep 1
done
for pid in $(gui_pids); do
	note "force-killing $pid"
	kill -KILL "$pid" 2>/dev/null || true
done

sleep 1
/usr/bin/open "$app"
for _ in $(seq 1 30); do
	after="$(gui_pids | tr '\n' ' ')"
	if [[ -n "$after" ]]; then
		note "running again: $after"
		exit 0
	fi
	sleep 0.5
done
note "Kaku did not come back after open; open $app by hand"
exit 1
