#!/bin/sh
# Splash screens. "boot" (the default) writes the boot image to the
# framebuffer until radiotrope.service stops it; "shutdown" writes the
# restart or power-off image once, after the player has left the screen.
# The framebuffer changes format on the way (the firmware's is 32 bpp,
# vc4's is 16 bpp), so each write checks the depth and picks the matching
# image.

FB=/dev/fb0
DIR=/usr/share/radiotrope

draw() {
    bpp=$(cat /sys/class/graphics/fb0/bits_per_pixel 2>/dev/null)
    case "$bpp" in
        16) zcat "$DIR/splash-$1-16.fb.gz" > "$FB" 2>/dev/null ;;
        *)  zcat "$DIR/splash-$1-32.fb.gz" > "$FB" 2>/dev/null ;;
    esac
}

case "${1:-boot}" in
    shutdown)
        [ ! -e "$FB" ] && exit 0
        # A reboot queues a job for reboot.target; anything else is going down
        if systemctl list-jobs --no-legend 2>/dev/null | grep -q 'reboot\.target'; then
            draw reboot
        else
            draw poweroff
        fi
        ;;
    *)
        # Wait up to 15 seconds for the framebuffer
        i=0
        while [ ! -e "$FB" ] && [ $i -lt 150 ]; do
            usleep 100000
            i=$((i + 1))
        done

        [ ! -e "$FB" ] && exit 0

        # Write repeatedly to survive the reinitialisation when vc4 takes over
        while true; do
            draw boot
            sleep 1
        done
        ;;
esac
