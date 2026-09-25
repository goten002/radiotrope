#!/bin/sh
# Boot splash — continuously write splash image to framebuffer
# Keeps refreshing until killed by radiotrope.service startup

SPLASH=/usr/share/radiotrope/splash.fb
FB=/dev/fb0

# Wait up to 15 seconds for framebuffer
i=0
while [ ! -e "$FB" ] && [ $i -lt 150 ]; do
    usleep 100000
    i=$((i + 1))
done

[ ! -e "$FB" ] && exit 0

# Write splash repeatedly to survive fbdev reinitializations
while true; do
    cat "$SPLASH" > "$FB" 2>/dev/null
    sleep 1
done
