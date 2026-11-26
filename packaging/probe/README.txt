radiotrope-probe @VERSION@ for @PLATFORM@
=========================================

The station checker from Radiotrope: https://github.com/goten002/radiotrope

It connects to an internet radio station the way the Radiotrope player
does, listens for a few seconds (no sound card needed) and reports whether
the station is on air, its format, its sound level and what is playing.
It is a download of its own: the Radiotrope packages and installers do not
include it.

  radiotrope-probe http://example.com/radio.pls       readable report
  radiotrope-probe --json <URL>...                    JSON for scripts
  radiotrope-probe --ndjson --input stations.txt      one JSON line per station
  radiotrope-probe --quiet --listen 5 <URL>...        one line per station

Run "radiotrope-probe --help" for the timeout, parallel jobs and silence
level.

Exit status: 0 up; 1 degraded (silent, slower than real time, stalling,
many decode errors, or ended); 2 down (with an error code such as dns,
connect, http_status, web_page or timeout); 3 the check can't run.

What is in this archive
-----------------------

  The program (radiotrope-probe, or radiotrope-probe.exe on Windows), this
  README, LICENSE (GNU General Public License, version 3) and
  THIRD-PARTY-LICENSES (the libraries it is built with; both end in .txt
  on Windows). On Linux also its
  man page and bash, zsh and fish completions under share/, laid out so the
  folder can be copied into ~/.local or /usr/local as it is.

Check the download against the SHA256SUMS file on the release page.
