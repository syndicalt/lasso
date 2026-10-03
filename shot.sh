#!/bin/bash
export WAYLAND_DISPLAY=wayland-1 XDG_RUNTIME_DIR=/run/user/1000
GEO=$(hyprctl -j clients | python3 -c "
import json,sys
for c in json.load(sys.stdin):
    if c.get('class')=='lasso': print(f\"{c['at'][0]},{c['at'][1]} {c['size'][0]}x{c['size'][1]}\")")
hyprctl dispatch focuswindow class:lasso >/dev/null 2>&1
sleep 0.3
timeout 10 grim -g "$GEO" "$1"
