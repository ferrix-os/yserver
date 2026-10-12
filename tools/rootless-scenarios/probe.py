#!/usr/bin/env python3
"""X clients for the rootless scenarios (tools/rootless-suite.sh).

    probe.py window NAME CLASS RRGGBB [WxH]   a top-level of one colour
    probe.py popup X Y WxH RRGGBB             an override-redirect window at
                                              root (X, Y), as a menu is

Each prints its window's id first, then one line per event it hears:
`configure WxH` when its size changes, and `closed` when the window manager
asks it to close (WM_DELETE_WINDOW), after which it exits 0. The colour is
the window's background, which the server paints itself, so nothing here
draws.
"""

import sys

from Xlib import X, display


def size(text):
    width, height = text.split("x")
    return int(width), int(height)


def main(argv):
    if len(argv) < 2 or argv[1] not in ("window", "popup"):
        print(__doc__, file=sys.stderr)
        return 2
    dpy = display.Display()
    screen = dpy.screen()
    if argv[1] == "window":
        name, wm_class, colour = argv[2], argv[3], int(argv[4], 16)
        (width, height), at, override = size(argv[5] if len(argv) > 5 else "300x200"), (0, 0), False
    else:
        at, (width, height), colour = (int(argv[2]), int(argv[3])), size(argv[4]), int(argv[5], 16)
        override = True
    window = screen.root.create_window(
        at[0],
        at[1],
        width,
        height,
        0,
        screen.root_depth,
        X.InputOutput,
        X.CopyFromParent,
        background_pixel=colour,
        override_redirect=override,
        event_mask=X.ExposureMask | X.StructureNotifyMask,
    )
    delete = dpy.intern_atom("WM_DELETE_WINDOW")
    if not override:
        window.set_wm_name(name)
        window.set_wm_class(wm_class.lower(), wm_class)
        window.set_wm_protocols([delete])
    window.map()
    dpy.flush()
    print(f"window 0x{window.id:x}", flush=True)
    last = (width, height)
    while True:
        event = dpy.next_event()
        if event.type == X.ConfigureNotify and (event.width, event.height) != last:
            last = (event.width, event.height)
            print(f"configure {event.width}x{event.height}", flush=True)
        elif event.type == X.ClientMessage and event.data[1][0] == delete:
            print("closed", flush=True)
            return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
