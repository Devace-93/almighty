#!/bin/sh
# Drive the desktop client inside the `almgui` container.
#   gui.sh start            build image if needed and start the app
#   gui.sh click X Y [mod]  click (optionally holding ctrl/alt/super)
#   gui.sh type "text"      type text; gui.sh key Return
#   gui.sh shot out.png     screenshot of the virtual display
set -e
here=$(cd "$(dirname "$0")" && pwd)
app="$here/../../crates/ui/target/dx/almighty-ui/debug/linux/app"
case "$1" in
  start)
    docker image inspect almighty-guitest >/dev/null 2>&1 || docker build -q -t almighty-guitest "$here"
    docker rm -f almgui >/dev/null 2>&1 || true
    docker run -d --name almgui --network host -e ALMIGHTY_URL -v "$app:/app:ro" almighty-guitest /app/almighty-ui >/dev/null
    sleep 8
    docker exec almgui sh -c 'xdotool search --name "^almighty$" windowmove 0 0 windowsize 1280 800' ;;
  click)
    if [ -n "$4" ]; then docker exec almgui xdotool mousemove "$2" "$3" keydown "$4" click 1 keyup "$4"
    else docker exec almgui xdotool mousemove "$2" "$3" click 1; fi ;;
  type) docker exec almgui xdotool type --delay 30 "$2" ;;
  key) docker exec almgui xdotool key "$2" ;;
  shot) docker exec almgui import -window root /tmp/shot.png && docker cp almgui:/tmp/shot.png "$2" >/dev/null ;;
  stop) docker rm -f almgui >/dev/null ;;
  *) echo "usage: gui.sh start|click|type|key|shot|stop"; exit 2 ;;
esac
