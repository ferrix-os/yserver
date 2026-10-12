# A top-level X window becomes a window of the compositor's, with its title
# and class, its own picture, the size the compositor tiles it at, and it
# closes when the compositor closes it (WM_DELETE_WINDOW).
# shellcheck shell=bash
# shellcheck source=lib.sh
. "$SCENARIOS/lib.sh"

probe probe window "rootless toplevel" Rltop 3366cc 300x200
pid=$!
wait_for 30 has_view Rltop || fail "the compositor never had a window of class Rltop"
read -r x y w h < <(view_rect Rltop)
title=$(swaymsg -t get_tree | python3 -c '
import json, sys
def walk(n):
    if n.get("app_id") == "Rltop":
        print(n.get("name")); sys.exit(0)
    for c in n.get("nodes", []) + n.get("floating_nodes", []):
        walk(c)
walk(json.load(sys.stdin))')
[ "$title" = "rootless toplevel" ] || fail "the compositor's window is titled '$title'"
echo "compositor window: ${w}x${h} at ($x, $y), '$title'"

# The compositor tiled it at its own size; X must take that size.
id=$(sed -n 's/^window //p' probe.log)
x_size() { xwininfo -id "$id" | awk '/Width:/ {w = $2} /Height:/ {h = $2} END {print w "x" h}'; }
wait_for 10 grep -q "^configure ${w}x${h}\$" probe.log \
    || fail "X kept the window at $(x_size), not the compositor's ${w}x${h}"

# Its picture, read back from X, on the compositor's screen.
cx=$((x + w / 2)) cy=$((y + h / 2))
wait_for 10 pixel_is "$cx" "$cy" 3366cc \
    || fail "the screen at ($cx, $cy) is $(pixel "$cx" "$cy"), not the window's 3366cc"
grim screen.png 2> /dev/null || true

# Closed by the compositor: the client hears WM_DELETE_WINDOW.
swaymsg '[app_id="Rltop"] kill' > /dev/null
wait_for 10 grep -q '^closed$' probe.log || fail "the client never heard WM_DELETE_WINDOW"
wait_for 10 gone "$pid" || fail "the client did not exit"
wait_for 10 no_view Rltop || fail "the compositor still has the window after it closed"
result pass
