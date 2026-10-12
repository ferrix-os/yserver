# X's CLIPBOARD and PRIMARY follow the compositor's clipboard and primary
# selection, both ways: what xclip copies, wl-paste pastes, and what
# wl-copy copies, xclip pastes.
# shellcheck shell=bash
# shellcheck source=lib.sh
. "$SCENARIOS/lib.sh"

pastes() { [ "$("$@" 2> /dev/null)" = "$want" ]; }

for selection in clipboard primary; do
    flag=
    [ "$selection" = primary ] && flag=-p

    # X to Wayland. xclip forks and serves the selection until it loses it.
    want="from X's $selection $$"
    printf '%s' "$want" | xclip -selection "$selection" -i
    # shellcheck disable=SC2086
    wait_for 10 pastes wl-paste $flag -n \
        || fail "wl-paste $flag gave '$(wl-paste $flag -n 2>&1)', not X's $selection '$want'"
    echo "X's $selection reached the compositor"

    # Wayland to X. wl-copy forks and serves until it loses it.
    want="from the compositor's $selection $$"
    # shellcheck disable=SC2086
    wl-copy $flag "$want"
    wait_for 10 pastes xclip -selection "$selection" -o \
        || fail "xclip gave '$(xclip -selection "$selection" -o 2>&1)', not the compositor's '$want'"
    echo "the compositor's $selection reached X"
done
result pass
