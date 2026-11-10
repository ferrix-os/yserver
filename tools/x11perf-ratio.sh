#!/usr/bin/env bash
# Run a fixed x11perf set against one X server and record rate + server CPU.
# Run once per server, then compare: yserver-vs-X ratio on the same box (#155).
#
#   DISPLAY=:0 tools/x11perf-ratio.sh <out.tsv> [server-process-name]
#   tools/x11perf-ratio.sh --compare <xorg.tsv> <yserver.tsv>

set -u

TESTS=(
    -noop -pointer -prop -getimage10
    -rect10 -putimage10 -copywinwin10 -copypixwin10 -scroll500
    -create -map -popup -move -umove -movetree -resize -uresize
    -ftext -aa10text -rgb10text
)

if [ "${1:-}" = "--compare" ]; then
    xorg=${2:?usage: --compare <xorg.tsv> <yserver.tsv>}
    ys=${3:?usage: --compare <xorg.tsv> <yserver.tsv>}
    printf '%-14s %12s %12s %7s %10s %10s %7s\n' \
        test x_rate y_rate y/x x_cpu_ms y_cpu_ms y/x
    awk -F'\t' '
        NR == FNR { if (FNR > 1) { xr[$1] = $2; xc[$1] = $3 / ($2 * $4) }; next }
        FNR > 1 && ($1 in xr) {
            yc = $3 / ($2 * $4)
            printf "%-14s %12.0f %12.0f %7.2f %10.4f %10.4f %7.2f\n",
                $1, xr[$1], $2, $2 / xr[$1], xc[$1] * 1000, yc * 1000, yc / xc[$1]
        }' "$xorg" "$ys"
    echo "cpu_ms = server CPU ms per op, approximated as cpu_s / (rate * wall_s)"
    exit 0
fi

out=${1:?usage: x11perf-ratio.sh <out.tsv> [server-process-name]}
name=${2:-}
if [ -z "$name" ]; then
    for candidate in yserver Xorg; do
        pgrep -x "$candidate" >/dev/null && name=$candidate && break
    done
fi
pid=$(pgrep -o -x "$name") || { echo "no $name process" >&2; exit 1; }
ticks=$(getconf CLK_TCK)

server_ticks() { awk '{ print $14 + $15 }' "/proc/$pid/stat"; }

printf 'test\trate_per_s\tserver_cpu_s\twall_s\n' > "$out"
for test in "${TESTS[@]}"; do
    before=$(server_ticks)
    start=$(date +%s.%N)
    rate=$(x11perf -repeat 3 -time 2 "$test" 2>/dev/null \
        | sed -n 's/.*( *\([0-9.]*\)\/sec):.*/\1/p' | tail -1)
    end=$(date +%s.%N)
    after=$(server_ticks)
    if [ -z "$rate" ]; then
        echo "$test: no result" >&2
        continue
    fi
    server_cpu=$(echo "($after - $before) / $ticks" | bc -l)
    printf '%s\t%s\t%.3f\t%.1f\n' "${test#-}" "$rate" "$server_cpu" \
        "$(echo "$end - $start" | bc -l)" >> "$out"
    echo "${test#-}: $rate/s"
done
echo "server=$name pid=$pid -> $out"
