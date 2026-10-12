#!/usr/bin/env bash
# Run the rootless scenarios (tools/rootless-scenarios/suite.list) against
# ywayland, the rootless Wayland backend, one at a time, each on a headless
# sway of its own, and decide pass/fail on the host. No VM and no GPU:
# sway draws with pixman, ywayland renders on lavapipe. Artifacts of every
# run land in target/rootless-suite/<timestamp>/<scenario>/ (sway.log,
# server.log, the scenario's own logs and screenshots), with summary.txt
# next to them.
#
#   tools/rootless-suite.sh                        # every scenario
#   tools/rootless-suite.sh clip                   # names matching the regex "clip"
#   tools/rootless-suite.sh --binary target/debug/ywayland
#   tools/rootless-suite.sh --out target/rootless-suite/ci
#
# Needs sway (1.10 or later, for ext-data-control), swaymsg, grim,
# wl-clipboard, xclip, x11-utils (xwininfo), python3 with python3-xlib,
# and Mesa's lavapipe. A scenario passes only if it wrote
# RESULT "pass" and ywayland was still alive at the end; a scenario whose
# needs= names a command that is not installed is a skip, not a pass.
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
registry=$repo/tools/rootless-scenarios/suite.list
binary=
filter=
run=

usage() {
    sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \?//;$d'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case $1 in
        --binary) binary=$2; shift 2;;
        --out) run=$2; shift 2;;
        -h|--help) usage 0;;
        -*) echo "rootless-suite: unknown argument $1" >&2; usage 1;;
        *) [ -z "$filter" ] || { echo "rootless-suite: one filter only" >&2; exit 1; }
           filter=$1; shift;;
    esac
done
[ "$filter" != all ] || filter=
regdir=$(cd -- "$(dirname -- "$registry")" && pwd)

for tool in sway swaymsg python3; do
    command -v "$tool" > /dev/null || { echo "rootless-suite: $tool is not installed" >&2; exit 1; }
done

declare -a names=() specs=()
while read -r name spec; do
    case $name in ''|'#'*) continue;; esac
    if [ -z "$filter" ] || [[ $name =~ $filter ]]; then
        names+=("$name"); specs+=("$spec")
    fi
done < "$registry"
[ ${#names[@]} -gt 0 ] || { echo "rootless-suite: filter '$filter' selects no scenario" >&2; exit 2; }

cd "$repo"
if [ -n "$binary" ]; then
    binary=$(cd -- "$(dirname -- "$binary")" && pwd)/$(basename -- "$binary")
else
    cargo build --features wayland --bin ywayland
    binary=$repo/target/debug/ywayland
fi
[ -x "$binary" ] || { echo "rootless-suite: $binary is not executable" >&2; exit 1; }

run=${run:-$repo/target/rootless-suite/$(date +%Y%m%d-%H%M%S)}
mkdir -p "$run"
run=$(cd -- "$run" && pwd)
summary=$run/summary.txt
{
    echo "date:     $(date -Is)"
    echo "yserver:  $(git -C "$repo" describe --always --dirty --abbrev=12 2> /dev/null || echo unknown)"
    echo "binary:   $binary ($(sha256sum "$binary" | cut -c1-16))"
    echo "sway:     $(sway --version 2> /dev/null || echo '?')"
    echo
} > "$summary"
cat "$summary"

# The first display number with neither a socket nor a lock.
free_display() {
    local n=70
    while [ -e "/tmp/.X11-unix/X$n" ] || [ -e "/tmp/.X$n-lock" ]; do
        n=$((n + 1))
    done
    echo "$n"
}

spec_value() { sed -n "s/.*\b$1=\([^ ]*\).*/\1/p" <<< "$2"; }

pass=0 failed=0 skipped=0
for i in "${!names[@]}"; do
    name=${names[$i]} spec=${specs[$i]}
    script=$regdir/$(spec_value script "$spec")
    timeout=$(spec_value timeout "$spec"); timeout=${timeout:-120}
    dir=$run/$name
    mkdir -p "$dir"

    missing=
    for tool in $(spec_value needs "$spec" | tr , ' '); do
        command -v "$tool" > /dev/null || missing="$missing $tool"
    done
    if [ -n "$missing" ]; then
        echo "skip: not installed:$missing" > "$dir/RESULT"
    else
        runtime=$(mktemp -d)
        chmod 700 "$runtime"
        cat > "$dir/sway.config" << 'CONFIG'
output HEADLESS-1 resolution 1024x768 position 0 0
default_border none
default_floating_border none
gaps inner 0
focus_follows_mouse no
CONFIG
        # sway and the server in a session of their own, killed as one.
        setsid env XDG_RUNTIME_DIR="$runtime" WLR_BACKENDS=headless WLR_RENDERER=pixman \
            WLR_LIBINPUT_NO_DEVICES=1 WLR_HEADLESS_OUTPUTS=1 \
            sway -c "$dir/sway.config" > "$dir/sway.log" 2>&1 < /dev/null &
        sway_pid=$!
        socket=
        for _ in $(seq 100); do
            socket=$(cd "$runtime" && ls wayland-[0-9]* 2> /dev/null | grep -v lock | head -1 || true)
            swaysock=$(ls "$runtime"/sway-ipc.*.sock 2> /dev/null | head -1 || true)
            [ -n "$socket" ] && [ -n "$swaysock" ] && break
            sleep 0.1
        done
        display=$(free_display)
        server_pid=
        if [ -z "$socket" ] || [ -z "$swaysock" ]; then
            echo "fail: sway never made its sockets" > "$dir/RESULT"
        else
            setsid env XDG_RUNTIME_DIR="$runtime" WAYLAND_DISPLAY="$socket" \
                YSERVER_ALLOW_SOFTWARE_VULKAN=1 RUST_LOG=info,yserver::wayland::input=debug \
                "$binary" ":$display" -nolisten tcp > "$dir/server.log" 2>&1 < /dev/null &
            server_pid=$!
            for _ in $(seq 600); do
                [ -S "/tmp/.X11-unix/X$display" ] && break
                kill -0 "$server_pid" 2> /dev/null || break
                sleep 0.1
            done
            if [ ! -S "/tmp/.X11-unix/X$display" ]; then
                echo "fail: ywayland never listened on :$display" > "$dir/RESULT"
            else
                (
                    cd "$dir"
                    export DISPLAY=":$display" WAYLAND_DISPLAY="$socket" SWAYSOCK="$swaysock"
                    export XDG_RUNTIME_DIR="$runtime" SCENARIOS="$regdir" SERVER_LOG="$dir/server.log"
                    timeout --kill-after=5 "$timeout" setsid bash "$script" > "$dir/scenario.log" 2>&1 < /dev/null
                ) || [ -s "$dir/RESULT" ] || echo "fail: timed out or died after ${timeout}s" > "$dir/RESULT"
                [ -s "$dir/RESULT" ] || echo "fail: wrote no RESULT" > "$dir/RESULT"
                if ! kill -0 "$server_pid" 2> /dev/null && grep -q '^pass' "$dir/RESULT"; then
                    echo "fail: ywayland died" > "$dir/RESULT"
                fi
            fi
        fi
        # The server and sway, by session; the scenario's clients go with
        # their connections.
        for leader in $server_pid $sway_pid; do
            pkill -TERM -s "$leader" 2> /dev/null || true
        done
        sleep 0.5
        for leader in $server_pid $sway_pid; do
            pkill -KILL -s "$leader" 2> /dev/null || true
        done
        wait 2> /dev/null || true
        rm -rf "$runtime" "/tmp/.X11-unix/X$display" "/tmp/.X$display-lock"
    fi

    verdict=$(head -1 "$dir/RESULT")
    case $verdict in
        pass) pass=$((pass + 1));;
        skip*) skipped=$((skipped + 1));;
        *) failed=$((failed + 1));;
    esac
    printf '%-12s %s\n' "$name" "$verdict" | tee -a "$summary"
done

echo | tee -a "$summary"
echo "passed $pass, failed $failed, skipped $skipped ($run)" | tee -a "$summary"
[ "$failed" = 0 ]
