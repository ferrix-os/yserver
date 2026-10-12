# Sourced by each rootless scenario (tools/rootless-suite.sh), in the
# scenario's artifact directory, with DISPLAY (ywayland), WAYLAND_DISPLAY,
# SWAYSOCK and XDG_RUNTIME_DIR (the headless sway), SCENARIOS (this
# directory) and SERVER_LOG (ywayland's log) set.
# shellcheck shell=bash

# Write the verdict and stop: pass, "fail: <why>" or "skip: <why>".
result() {
    echo "$*" > RESULT
    exit 0
}
fail() { result "fail: $*"; }

# Run "$@" every 0.2 s until it succeeds, for at most $1 seconds.
wait_for() {
    local seconds=$1
    shift
    local tries=$((seconds * 5))
    while [ "$tries" -gt 0 ]; do
        if "$@" > /dev/null 2>&1; then
            return 0
        fi
        sleep 0.2
        tries=$((tries - 1))
    done
    return 1
}

# The rectangle "x y width height" of the compositor's window whose app id
# or title is $1, from sway's tree; nothing when there is none.
view_rect() {
    swaymsg -t get_tree | python3 -c '
import json, sys
want = sys.argv[1]
def walk(node):
    if node.get("app_id") == want or (node.get("name") == want and node.get("pid")):
        r = node["rect"]
        print(r["x"], r["y"], r["width"], r["height"])
        sys.exit(0)
    for child in node.get("nodes", []) + node.get("floating_nodes", []):
        walk(child)
walk(json.load(sys.stdin))
sys.exit(1)
' "$1"
}

# Whether the compositor has the window $1 (app id or title), or not.
has_view() { view_rect "$1" > /dev/null; }
no_view() { ! has_view "$1"; }

# Whether process $1 has ended.
gone() { ! kill -0 "$1" 2> /dev/null; }

# The colour "rrggbb" of the compositor's screen at ($1, $2), from grim.
pixel() {
    grim -t ppm - | python3 -c '
import sys
data = sys.stdin.buffer.read()
fields, at = [], 0
while len(fields) < 4:
    while data[at:at + 1].isspace():
        at += 1
    if data[at:at + 1] == b"#":
        at = data.index(b"\n", at)
        continue
    end = at
    while not data[end:end + 1].isspace():
        end += 1
    fields.append(data[at:end])
    at = end
at += 1
width = int(fields[1])
x, y = int(sys.argv[1]), int(sys.argv[2])
offset = at + (y * width + x) * 3
print(data[offset:offset + 3].hex())
' "$1" "$2"
}

# Whether two colours "rrggbb" are within 8 of each other in every channel.
near() {
    python3 -c '
import sys
a, b = bytes.fromhex(sys.argv[1]), bytes.fromhex(sys.argv[2])
sys.exit(0 if len(a) == 3 and all(abs(x - y) <= 8 for x, y in zip(a, b)) else 1)
' "$1" "$2"
}

# Whether the screen at ($1, $2) is near the colour $3.
pixel_is() { near "$(pixel "$1" "$2")" "$3"; }

# A window of one colour: probe.py's arguments; its log is $1.log.
probe() {
    local log=$1
    shift
    python3 "$SCENARIOS/probe.py" "$@" > "$log.log" 2>&1 &
}
