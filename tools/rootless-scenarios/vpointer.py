#!/usr/bin/env python3
"""A virtual pointer on the Wayland compositor WAYLAND_DISPLAY names, for the
rootless scenarios (tools/rootless-suite.sh): zwlr_virtual_pointer_v1, over
the wire protocol itself, so no Wayland library is needed.

    vpointer.py WIDTH HEIGHT ACTION...

WIDTH and HEIGHT are the output layout's size. Each ACTION is one of

    move:X,Y       to (X, Y) on the layout
    click:BUTTON   press and release an evdev button (272 left)
    scroll:CLICKS  turn the wheel, down positive
    sleep:SECONDS  wait

all on one device: a headless compositor has a pointer only while a virtual
one is there, and drops the pointer's focus when it goes, so a tool that
makes a device per action (wlrctl) loses every click to that.
"""

import os
import socket
import struct
import sys
import time


class Wayland:
    def __init__(self):
        path = os.path.join(os.environ["XDG_RUNTIME_DIR"], os.environ["WAYLAND_DISPLAY"])
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(path)
        self.next_id = 2
        self.buffer = b""
        self.globals = {}

    def new_id(self):
        new = self.next_id
        self.next_id += 1
        return new

    def send(self, obj, opcode, args=b""):
        self.sock.sendall(struct.pack("<II", obj, (8 + len(args)) << 16 | opcode) + args)

    def events(self):
        while True:
            while len(self.buffer) >= 8:
                obj, word = struct.unpack_from("<II", self.buffer)
                size = word >> 16
                if len(self.buffer) < size:
                    break
                body = self.buffer[8:size]
                self.buffer = self.buffer[size:]
                yield obj, word & 0xFFFF, body
            data = self.sock.recv(65536)
            if not data:
                raise SystemExit("vpointer: the compositor closed the connection")
            self.buffer += data

    def roundtrip(self, registry=None):
        callback = self.new_id()
        self.send(1, 0, struct.pack("<I", callback))  # wl_display.sync
        for obj, opcode, body in self.events():
            if obj == 1 and opcode == 0:
                _, code, length = struct.unpack_from("<III", body)
                raise SystemExit(f"vpointer: protocol error {code}: {body[12:12 + length - 1]}")
            if obj == registry and opcode == 0:
                name, length = struct.unpack_from("<II", body)
                interface = body[8:8 + length - 1].decode()
                version = struct.unpack_from("<I", body, 8 + (length + 3) // 4 * 4)[0]
                self.globals[interface] = (name, version)
            if obj == callback:
                return


def string(text):
    data = text.encode() + b"\0"
    return struct.pack("<I", len(data)) + data + b"\0" * (-len(data) % 4)


def main(argv):
    if len(argv) < 3:
        print(__doc__, file=sys.stderr)
        return 2
    width, height = int(argv[1]), int(argv[2])
    wl = Wayland()
    registry = wl.new_id()
    wl.send(1, 1, struct.pack("<I", registry))  # wl_display.get_registry
    wl.roundtrip(registry)

    def bind(interface, version):
        if interface not in wl.globals:
            raise SystemExit(f"vpointer: the compositor has no {interface}")
        name, offered = wl.globals[interface]
        new = wl.new_id()
        wl.send(registry, 0, struct.pack("<I", name) + string(interface)
                + struct.pack("<II", min(version, offered), new))
        return new

    seat = bind("wl_seat", 1)
    manager = bind("zwlr_virtual_pointer_manager_v1", 1)
    pointer = wl.new_id()
    wl.send(manager, 0, struct.pack("<II", seat, pointer))  # create_virtual_pointer
    wl.roundtrip()

    def now():
        return int(time.monotonic() * 1000) & 0xFFFFFFFF

    for action in argv[3:]:
        verb, _, value = action.partition(":")
        if verb == "move":
            x, y = (int(v) for v in value.split(","))
            wl.send(pointer, 1, struct.pack("<IIIII", now(), x, y, width, height))
        elif verb == "click":
            button = int(value)
            for state in (1, 0):
                wl.send(pointer, 2, struct.pack("<III", now(), button, state))
                wl.send(pointer, 4)  # frame
                wl.roundtrip()
                time.sleep(0.05)
            continue
        elif verb == "scroll":
            clicks = int(value)
            wl.send(pointer, 5, struct.pack("<I", 0))  # axis_source: wheel
            # axis_discrete: vertical, 15 units a click, as libinput's wheel
            wl.send(pointer, 7, struct.pack("<IIii", now(), 0, clicks * 15 * 256, clicks))
        elif verb == "sleep":
            time.sleep(float(value))
            continue
        else:
            raise SystemExit(f"vpointer: unknown action {action}")
        wl.send(pointer, 4)  # frame
        wl.roundtrip()
        time.sleep(0.05)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
