#!/bin/bash
# macOS: what the system asks of Radiotrope.app, tried the way a user's
# clicks reach it (Launch Services, through `open`), and Start at Login
# through launchd. For CI (ci-macos.yml with a bundle around the debug
# build, package-macos.yml with the one from the DMG) and for a Mac by hand:
#
#   scripts/macos-integration-check.sh /Applications/Radiotrope.app [EVIDENCE_DIR]
#
# What it checks:
#   1. a playlist opened while the app is NOT running starts the app, which
#      plays the playlist's address (`open -a Radiotrope.app "Check Cold.m3u"`);
#   2. a second launch (`open -a`, and the program itself as from Terminal)
#      starts no second process, and the running app is told to show its
#      window (what a click on the Dock icon sends);
#   3. a .pls and a plain address opened while it runs play too, and a
#      playlist with no station in it is refused with a message;
#   4. SIGTERM still quits it;
#   5. `radiotrope --start-at-login on` writes the LaunchAgent file that
#      View > System Tray > Start at Login writes, launchd accepts the file
#      and starts the app from it, and `--start-at-login off` removes it.
#
# It reads the app's log (~/Library/Logs/radiotrope/radiotrope.log), which
# says what the app did with each request. The addresses are of nothing
# (127.0.0.1:9), so no station plays.
#
# By hand, note: this runs in YOUR profile (Launch Services starts the app
# with your home folder, not one of the script's), so the last station
# shown afterwards is the check's dead address. Quit Radiotrope first; a
# Start at Login that was on is put back on at the end.
set -u

[ $# -ge 1 ] || { echo "usage: $0 /path/to/Radiotrope.app [EVIDENCE_DIR]"; exit 2; }
[ -d "$1" ] || { echo "$1 is not an app bundle"; exit 2; }
app=$(cd "$1" && pwd)
program="$app/Contents/MacOS/radiotrope"
[ -x "$program" ] || { echo "$program is missing"; exit 2; }
mkdir -p "${2:-smoke}"
evidence=$(cd "${2:-smoke}" && pwd)

log="$HOME/Library/Logs/radiotrope/radiotrope.log"
agent="$HOME/Library/LaunchAgents/app.radiotrope.Radiotrope.plist"
label=app.radiotrope.Radiotrope
domain="gui/$(id -u)"
report="$evidence/integration.txt"
: > "$report"

failed=0
ok() { echo "ok: $*" | tee -a "$report"; }
fail() { echo "::error::$*"; echo "FAILED: $*" >> "$report"; failed=1; }

# How many of this bundle's player run
running() { pgrep -f "$program" | wc -l | xargs; }
# Wait (up to $2 seconds) until $1 of them run
wait_running() {
  for _ in $(seq "$2"); do [ "$(running)" = "$1" ] && return 0; sleep 1; done
  return 1
}
# The log's lines since the check started
log_start=$(wc -l < "$log" 2>/dev/null | xargs || true)
new_log() { tail -n "+$(( ${log_start:-0} + 1 ))" "$log" 2>/dev/null; }
# Wait (up to $2 seconds) for a line holding $1
wait_log() {
  for _ in $(seq "$2"); do new_log | grep -qF -- "$1" && return 0; sleep 1; done
  return 1
}
quit_app() {
  pkill -TERM -f "$program" 2>/dev/null
  wait_running 0 20 && return 0
  pkill -KILL -f "$program" 2>/dev/null
  return 1
}

if [ "$(running)" != 0 ]; then
  echo "Radiotrope is running: quit it first"
  exit 2
fi
agent_was_on=0
[ -f "$agent" ] && agent_was_on=1

sw_vers 2>/dev/null | tee -a "$report"
echo "app: $app" | tee -a "$report"

# 1. Started by opening a playlist
cold="$evidence/Check Cold.m3u"
printf '#EXTM3U\n#EXTINF:-1,Check\nhttp://127.0.0.1:9/radiotrope-check-cold\n' > "$cold"
open -a "$app" "$cold" || fail "open -a Radiotrope.app file.m3u failed"
if wait_running 1 30; then
  ok "a playlist opened with the app started it"
else
  fail "opening a playlist did not start the app ($(running) running)"
fi
if wait_log "Check Cold.m3u: playing http://127.0.0.1:9/radiotrope-check-cold" 45; then
  ok "the playlist it was started with plays (application:openURLs: at launch)"
else
  fail "the app did not play the playlist it was started with"
fi

# 2. A second launch is the first one, shown
open -a "$app" || fail "open -a Radiotrope.app (second time) failed"
if wait_log "Asked for again" 20; then
  ok "a second launch asks the running app to show its window (reopen)"
else
  fail "the running app was not told of the second launch (applicationShouldHandleReopen)"
fi
[ "$(running)" = 1 ] && ok "open -a a second time: still one process" \
  || fail "open -a a second time: $(running) processes"

# The program itself, as from Terminal: it hands over to the running one
"$program" > "$evidence/integration-second.log" 2>&1 &
second=$!
for _ in $(seq 20); do kill -0 "$second" 2>/dev/null || break; sleep 1; done
if kill -0 "$second" 2>/dev/null; then
  fail "the program started a second player beside the running one"
  kill -KILL "$second"
else
  ok "the program run a second time exits, leaving the first"
fi
sleep 1
[ "$(running)" = 1 ] || fail "after the program was run again: $(running) processes"

# 3. Opened while running: a .pls, an address, and a file with no station
warm="$evidence/Check Warm.pls"
printf '[playlist]\nNumberOfEntries=1\nFile1=http://127.0.0.1:9/radiotrope-check-warm\n' > "$warm"
open -a "$app" "$warm" || fail "open -a Radiotrope.app file.pls failed"
if wait_log "Check Warm.pls: playing http://127.0.0.1:9/radiotrope-check-warm" 20; then
  ok "a .pls opened while the app runs plays"
else
  fail "a .pls opened while the app runs did not reach it"
fi

# (The app registers no address scheme, so only `open -a` sends it one: a
# warning if the system doesn't pass it on, not a failure.)
open -a "$app" "http://127.0.0.1:9/radiotrope-check-url" || echo "::warning::open -a Radiotrope.app URL failed"
if wait_log "playing http://127.0.0.1:9/radiotrope-check-url" 20; then
  ok "an address opened with the app plays"
else
  echo "::warning::an address opened with the app did not reach it"
  echo "warning: an address opened with the app did not reach it" >> "$report"
fi

music="$evidence/Check Music.m3u"
printf '#EXTM3U\n#EXTINF:215,A song\nMusic/song.mp3\n' > "$music"
open -a "$app" "$music" || fail "open -a Radiotrope.app music.m3u failed"
if wait_log "Check Music.m3u: The file has no stream addresses" 20; then
  ok "a playlist of local files is refused with a message"
else
  fail "a playlist of local files was not answered"
fi
[ "$(running)" = 1 ] && ok "after the opens: still one process" \
  || fail "after the opens: $(running) processes"
sleep 2
screencapture -x "$evidence/integration.png" 2>/dev/null || true

# 4. Quit
if quit_app; then ok "SIGTERM quits it"; else fail "the app did not quit on SIGTERM"; fi

# 5. Start at Login
if "$program" --start-at-login on | tee -a "$report" && [ -f "$agent" ]; then
  ok "--start-at-login on wrote $agent"
  cp "$agent" "$evidence/integration-launch-agent.plist"
  plutil -lint "$agent" > /dev/null || fail "the LaunchAgent file is not a property list"
  [ "$(plutil -extract Label raw "$agent")" = "$label" ] || fail "the LaunchAgent's Label is not $label"
  [ "$(plutil -extract ProgramArguments.0 raw "$agent")" = "$program" ] \
    || fail "the LaunchAgent starts '$(plutil -extract ProgramArguments.0 raw "$agent")', not $program"
  [ "$(plutil -extract RunAtLoad raw "$agent")" = true ] || fail "the LaunchAgent has no RunAtLoad"
  [ "$(plutil -extract LimitLoadToSessionType raw "$agent")" = Aqua ] || fail "the LaunchAgent is not limited to Aqua"
  [ "$(plutil -extract ProcessType raw "$agent")" = Interactive ] || fail "the LaunchAgent is not Interactive"

  # What login does: launchd reads the file, and RunAtLoad starts the app
  if launchctl bootstrap "$domain" "$agent" 2>&1 | tee -a "$report"; [ "${PIPESTATUS[0]}" -eq 0 ]; then
    ok "launchd accepts the file"
    if wait_running 1 30; then
      ok "launchd started the app from it (as at login)"
    else
      fail "launchd did not start the app from the LaunchAgent"
      launchctl print "$domain/$label" 2>&1 | head -40
    fi
    launchctl print "$domain/$label" 2>/dev/null | grep -E "state =|program =|last exit" | tee -a "$report"
    sleep 5
    # `--quit` asks the running player to exit and waits for it
    if "$program" --quit && wait_running 0 10; then
      ok "--quit closed the app launchd started"
    else
      fail "--quit did not close the app launchd started"
      quit_app
    fi
    launchctl bootout "$domain/$label" 2>/dev/null || true
  else
    fail "launchd refused the LaunchAgent file"
  fi
else
  fail "--start-at-login on did not write $agent"
fi
if "$program" --start-at-login off | tee -a "$report" && [ ! -e "$agent" ]; then
  ok "--start-at-login off removed the file"
else
  fail "--start-at-login off left $agent"
fi
if [ "$agent_was_on" = 1 ]; then
  "$program" --start-at-login on > /dev/null && echo "Start at Login was on before: on again"
fi

quit_app > /dev/null 2>&1
new_log > "$evidence/integration.log"
echo "--- the app's log during the check"; cat "$evidence/integration.log"
echo "--- the second launch's output"; cat "$evidence/integration-second.log" 2>/dev/null
echo "--- result"; cat "$report"
exit $failed
