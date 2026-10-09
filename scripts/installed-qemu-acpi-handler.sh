#!/bin/sh
# This installed test fixture accepts only the real ACPI power button action.
case "$1:$2:$3:$4" in
    button/power:PWRF:*|button/power:PBTN:*)
        echo VEYNODE_INSTALLED_ACPI_POWER_BUTTON >/dev/console
        /sbin/poweroff
        ;;
esac
