# The compositor's seat reaches X: xev, shown as the compositor's focused
# window, hears the keyboard's focus, the pointer coming in and moving, a
# click, a wheel click and a key, all put in through the compositor's
# virtual pointer and keyboard (vpointer.py, wtype), never through XTEST.
# shellcheck shell=bash
# shellcheck source=lib.sh
. "$SCENARIOS/lib.sh"

xev -geometry 300x200+0+0 > xev.log 2>&1 &
wait_for 30 has_view "Event Tester" || fail "the compositor never had xev's window"
read -r x y w h < <(view_rect "Event Tester")
echo "xev on the compositor: ${w}x${h} at ($x, $y)"

# The pointer into the window, a click and a wheel click down, from one
# virtual pointer (vpointer.py says why one).
python3 "$SCENARIOS/vpointer.py" 1024 768 sleep:1 "move:$((x + w / 2)),$((y + h / 2))" \
    sleep:0.3 click:272 sleep:0.3 scroll:1 sleep:0.5 || fail "the virtual pointer failed"
wait_for 10 grep -q '^EnterNotify event\|^MotionNotify event' xev.log \
    || fail "xev never heard the pointer"
wait_for 10 grep -q '^ButtonRelease event' xev.log || fail "xev never heard the click"

# The keyboard comes onto the focused window, then a key.
wtype -s 1500 a -s 300
wait_for 10 grep -q '^FocusIn event' xev.log \
    || fail "xev never had the focus the compositor's keyboard gave its window"
wait_for 10 grep -q '^KeyRelease event' xev.log || fail "xev never heard the key"
sleep 0.5

# Each event as xev reports it: a press and its release.
for want in 'ButtonPress event' 'ButtonRelease event' 'KeyPress event' 'KeyRelease event'; do
    grep -q "^$want" xev.log || fail "xev reported no $want"
done
grep -A2 '^ButtonPress event' xev.log | grep -q 'button 1,' || fail "the click was not button 1"
grep -A2 '^ButtonPress event' xev.log | grep -q 'button 5,' || fail "the wheel was not button 5"
press=$(grep -A2 '^KeyPress event' xev.log | sed -n 's/.*keycode \([0-9]*\).*/\1/p' | head -1)
release=$(grep -A2 '^KeyRelease event' xev.log | sed -n 's/.*keycode \([0-9]*\).*/\1/p' | head -1)
[ -n "$press" ] && [ "$press" = "$release" ] || fail "key press $press and release $release differ"
echo "xev heard: focus, the pointer, button 1, button 5, keycode $press"
result pass
