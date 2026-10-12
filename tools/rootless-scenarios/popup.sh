# An override-redirect X window, as a menu is, shows as the compositor's
# popup on the window it belongs to, exactly where X put it relative to
# that window, and goes when it unmaps.
# shellcheck shell=bash
# shellcheck source=lib.sh
. "$SCENARIOS/lib.sh"

probe probe window "rootless menu owner" Rlmenu 3366cc 300x200
wait_for 30 has_view Rlmenu || fail "the compositor never had the owner's window"
read -r x y w h < <(view_rect Rlmenu)
wait_for 10 pixel_is $((x + 10)) $((y + 10)) 3366cc || fail "the owner's window never showed"

# The menu at (40, 30) in the owner's X coordinates, 80x60.
id=$(sed -n 's/^window //p' probe.log)
read -r ox oy < <(xwininfo -id "$id" | awk '/Absolute upper-left X:/ {x = $4} /Absolute upper-left Y:/ {y = $4} END {print x, y}')
probe menu popup $((ox + 40)) $((oy + 30)) 80x60 cc3333
menu=$!
wait_for 10 grep -q 'override-redirect window .* (80x60) is a popup on' "$SERVER_LOG" \
    || fail "the server made no popup for the override-redirect window"
grep 'is a popup on' "$SERVER_LOG" | tail -1
px=$((x + 40)) py=$((y + 30))
wait_for 10 pixel_is $((px + 40)) $((py + 30)) cc3333 \
    || fail "the screen at the menu's middle is $(pixel $((px + 40)) $((py + 30))), not cc3333"
pixel_is "$px" "$py" cc3333 || fail "the menu's top left corner is not at ($px, $py)"
pixel_is $((px - 2)) $((py - 2)) 3366cc || fail "the menu reaches above and left of ($px, $py)"
pixel_is $((px + 81)) $((py + 61)) 3366cc || fail "the menu reaches past 80x60"
grim screen.png 2> /dev/null || true
echo "the popup is at ($px, $py) on the screen, 80x60, where X put it"

# Unmapped, it goes.
kill "$menu"
wait_for 10 pixel_is $((px + 40)) $((py + 30)) 3366cc || fail "the popup stayed after its window went"
result pass
