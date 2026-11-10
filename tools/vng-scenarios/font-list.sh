# Sourced by tools/vng-shot.sh INSIDE the guest (DISPLAY=:7, cwd = artifacts).
# ListFonts and ListFontsWithInfo replies, compared with a live Xorg run:
# the X core font dirs this distro has (misc, TTF, 100dpi, 75dpi) and CDE's
# alias dir where it is installed; misc and an alias dir of edge cases;
# then built-ins alone (font-list-probe.c, patterns in font-list-*.txt).
# shellcheck shell=sh
# golden: probe.log
# mask: /font-list(\.xorg)?/aliases => /<artifacts>/aliases -- each run writes its alias dir into its own artifact dir
# mask: ^  open: error 17$ =>   open: error 15 -- an alias loop: Xorg's OpenFont gives up with BadImplementation, ours with BadName
# drop: ^   FONT= -- ListFontsWithInfo FONT: ours is the fonts.dir key, which OpenFont reopens (Debian's 19px misc-fixed PCFs carry a different name); Xorg's is the PCF's own
set -u
set +e
cc -O1 -o probe "${YSERVER_REPO:?}/tools/vng-scenarios/font-list-probe.c" -lxcb > cc.log 2>&1 || cat cc.log >&2
set --
for d in misc TTF 100dpi 75dpi; do
    for root in /usr/share/fonts /usr/share/fonts/X11; do
        [ -f "$root/$d/fonts.dir" ] && set -- "$@" "$root/$d"
    done
done
[ -f /usr/dt/etc/cde/fontaliases/fonts.alias ] && set -- "$@" /usr/dt/etc/cde/fontaliases
: > probe.log
if [ $# -gt 0 ]; then
    ./probe "${YSERVER_REPO}/tools/vng-scenarios/font-list-patterns.txt" "$@" >> probe.log 2>&1
fi
# Aliases to an alias, to nothing, in a loop, to a pattern.
mkdir -p aliases
cat > aliases/fonts.alias << 'EOF'
zz-alias-gone no-such-font
zz-alias-fixed fixed
zz-alias-loop zz-alias-loop2
zz-alias-loop2 zz-alias-loop
zz-alias-pattern "-misc-fixed-medium-r-normal--1?-*-*-*-*-*-iso8859-1"
ZZ-Alias-Case 5X7
EOF
chmod -R a+rX aliases
for d in /usr/share/fonts/misc /usr/share/fonts/X11/misc; do
    if [ -f "$d/fonts.dir" ]; then
        ./probe "${YSERVER_REPO}/tools/vng-scenarios/font-list-aliases.txt" "$d" "$PWD/aliases" >> probe.log 2>&1
        break
    fi
done
./probe "${YSERVER_REPO}/tools/vng-scenarios/font-list-builtins.txt" built-ins >> probe.log 2>&1 \
    && touch PROBE-DONE
tail -5 probe.log
if [ ! -x probe ]; then echo "fail: probe did not build (cc.log)" > RESULT
elif [ ! -e PROBE-DONE ]; then echo "fail: the probe stopped early (probe.log)" > RESULT
else echo pass > RESULT; fi
