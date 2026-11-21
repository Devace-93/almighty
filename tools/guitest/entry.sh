#!/bin/sh
# Starts the virtual display, then the app given as arguments, and stays up
# so the host can drive it with `docker exec … xdotool/import`.
Xvfb :99 -screen 0 1280x800x24 -nolisten tcp >/tmp/xvfb.log 2>&1 &
sleep 1
exec dbus-run-session -- "$@"
