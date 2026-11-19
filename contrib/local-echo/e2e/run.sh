#!/bin/bash
# usage: run.sh <gui-binary> <rtt-ms> <label>
set -u
GUI=$1; LAG=$2; LABEL=$3
E=$(cd "$(dirname "$0")" && pwd)
RUN=/dev/shm/wt-$LABEL
rm -rf "$RUN"; mkdir -p "$RUN/xdg"; chmod 700 "$RUN" "$RUN/xdg"
export LAG_TRACE=$RUN/trace.txt XDG_RUNTIME_DIR=$RUN/xdg TEST_SOCK=$RUN/sock LAGPROXY=$E/lagproxy.py LAG_MS=$LAG LAG_JITTER_MS=${LAG_JITTER_MS:-0} \
       SERVER_CFG=$E/server.lua TEST_LOG=$RUN/log.txt
unset WEZTERM_UNIX_SOCKET WEZTERM_PANE TMUX TMUX_PANE
if ! pgrep -f "Xvnc :77" >/dev/null; then
  rm -f /tmp/.X11-unix/X77 /tmp/.X77-lock
  Xvnc :77 -geometry 1280x800 -depth 24 -SecurityTypes None -rfbport -1 -rfbunixpath "$E/vnc.sock" -nolisten tcp >"$E/xvnc.log" 2>&1 &
  sleep 2
fi
/usr/bin/wezterm-mux-server --config-file "$E/server.lua" --daemonize >"$RUN/mux.log" 2>&1
sleep 2
DISPLAY=:77 timeout 120 "$GUI" --config-file "$E/client.lua" >"$RUN/gui.log" 2>&1
echo "gui exit=$?"
pkill -f "wezterm-mux-server.*--config-file $E/server.lua"
grep -c . "$RUN/log.txt" 2>/dev/null
