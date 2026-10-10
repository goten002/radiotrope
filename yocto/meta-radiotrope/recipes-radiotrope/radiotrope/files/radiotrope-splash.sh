#!/bin/sh
# Boot splash: writes the splash image to the framebuffer until
# radiotrope.service stops it. The framebuffer changes format on the way
# (the firmware's is 32 bpp, vc4's is 16 bpp), so each pass checks the
# depth and picks the matching image.

FB=/dev/fb0
DIR=/usr/share/radiotrope

# Wait up to 15 seconds for the framebuffer
i=0
while [ ! -e "$FB" ] && [ $i -lt 150 ]; do
    usleep 100000
    i=$((i + 1))
done

[ ! -e "$FB" ] && exit 0

# Write repeatedly to survive the reinitialisation when vc4 takes over
while true; do
    bpp=$(cat /sys/class/graphics/fb0/bits_per_pixel 2>/dev/null)
    case "$bpp" in
        16) cat "$DIR/splash-16.fb" > "$FB" 2>/dev/null ;;
        *)  cat "$DIR/splash-32.fb" > "$FB" 2>/dev/null ;;
    esac
    sleep 1
done
