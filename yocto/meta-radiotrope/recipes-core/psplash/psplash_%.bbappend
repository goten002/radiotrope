FILESEXTRAPATHS:prepend := "${THISDIR}/files:"

# Replace default Yocto splash with Radiotrope branding
SPLASH_IMAGES:forcevariable = "file://radiotrope-splash.png;outsuffix=default"
