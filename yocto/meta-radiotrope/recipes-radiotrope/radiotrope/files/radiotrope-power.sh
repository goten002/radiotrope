#!/bin/sh
# Power button monitor — toggles radiotrope between active and light sleep
# Uses gpiomon -n 1 (single event) to avoid bounce buffering
# LED on GPIO 22 indicates running state
#
# Bluetooth, USB/Ethernet, and HDMI are permanently disabled
# Light sleep additionally disables: WiFi, 3 CPU cores, LEDs

LED=534  # GPIO 22 = 512 + 22

# Setup LED GPIO
echo $LED > /sys/class/gpio/export 2>/dev/null
sleep 0.1
echo out > /sys/class/gpio/gpio${LED}/direction 2>/dev/null

led_on() {
    echo 1 > /sys/class/gpio/gpio${LED}/value 2>/dev/null
}

led_off() {
    echo 0 > /sys/class/gpio/gpio${LED}/value 2>/dev/null
}

is_running() {
    systemctl is-active --quiet radiotrope
}

enter_sleep() {
    # Backlight off immediately (instant visual feedback)
    echo 0 > /sys/class/backlight/10-0045/brightness 2>/dev/null

    # Stop the app
    systemctl stop radiotrope

    # Clear framebuffer to black (prevents corrupted image showing when backlight leaks)
    # 800x480x4 bytes = 1,536,000 bytes
    dd if=/dev/zero of=/dev/fb0 bs=4096 count=375 2>/dev/null

    # WiFi stays on during sleep for instant wake with network ready

    # CPU to minimum frequency
    echo powersave > /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null

    # Disable 3 CPU cores (~200mW)
    echo 0 > /sys/devices/system/cpu/cpu1/online 2>/dev/null
    echo 0 > /sys/devices/system/cpu/cpu2/online 2>/dev/null
    echo 0 > /sys/devices/system/cpu/cpu3/online 2>/dev/null

    # Turn off board LEDs
    echo 0 > /sys/class/leds/led0/brightness 2>/dev/null
    echo none > /sys/class/leds/led0/trigger 2>/dev/null
    echo 0 > /sys/class/leds/led1/brightness 2>/dev/null
    echo none > /sys/class/leds/led1/trigger 2>/dev/null

    # Power LED off last
    led_off
}

exit_sleep() {
    # Power LED on first (instant visual feedback)
    led_on

    # Re-enable CPU cores
    echo 1 > /sys/devices/system/cpu/cpu1/online 2>/dev/null
    echo 1 > /sys/devices/system/cpu/cpu2/online 2>/dev/null
    echo 1 > /sys/devices/system/cpu/cpu3/online 2>/dev/null

    # CPU to ondemand
    echo ondemand > /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null

    # Backlight on
    echo 255 > /sys/class/backlight/10-0045/brightness 2>/dev/null

    # Restore board LEDs
    echo mmc0 > /sys/class/leds/led0/trigger 2>/dev/null
    echo default-on > /sys/class/leds/led1/trigger 2>/dev/null


    # Start the app
    systemctl start radiotrope
}

# Wait for system to settle, then set initial LED state
sleep 3
if is_running; then
    led_on
else
    led_off
fi

# Main loop — wait for single press, handle, debounce, repeat
while true; do
    gpiomon -e falling -n 1 -c gpiochip0 4 2>/dev/null

    if is_running; then
        enter_sleep
    else
        exit_sleep
    fi

    # Debounce
    sleep 1
done
