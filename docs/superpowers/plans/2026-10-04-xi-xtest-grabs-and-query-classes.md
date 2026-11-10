# XTEST grabs, query classes and FakeInput validation — plan 2 of 3

**Spec:** [maintainer-review addendum](../specs/2026-10-04-xi-registry-maintainer-review-addendum.md)
sections B, C1–C5 and D. **Branch:** `feat/xi-dynamic-registry-rebased`.
**Scope:** maintainer items X3–X6 only. Runs after plan 1 is committed. Plan
3 covers E1, X8, G and the docs.

**Executor and rules:** identical to [plan 1](2026-10-04-xi-enabled-state-and-floating.md),
"Rules for every task" (tests A/B, a mutation named per test, test first,
full per-task gate, no env vars, no scope beyond the addendum).

Xorg reference: `CorePointerProc` (`dix/devices.c:655-700`) builds both the
masters' initial classes and the XTEST pointer: 10 buttons with labels
left/middle/right/wheel up/down/hwheel left/right and 3 unlabeled, 2
valuators labeled `Rel X`/`Rel Y` (`InitPointerDeviceStruct`, mode
`Relative`, `NO_AXIS_LIMITS`), and no scroll classes.

---

### Task 1 — XTestFakeInput validation in Xorg's order (D)

**Do:** in `dispatch_fake_input_with_body` (`process_request.rs`), validate
before injecting anything, in `ProcXTestFakeInput`'s order
(`Xext/xtest.c:176-420`):

- XI device events look up `deviceid & 0x7f`; an unknown device is
  `BadDevice` with `errorValue = deviceid & 0x7f`;
- the class checks (`BadValue`) and the DeviceMotion valuator pairing
  (`BadLength`);
- the key range: `detail` outside the target keymap's min/max →
  `BadValue`;
- the button range: `detail == 0` or `detail` greater than the target's
  button count → `BadValue`. The core path targets XTEST 4 (10 buttons).

Button 10 is then injected and delivered like the others. Pick the internal
code it maps to so that the fanout reports detail 10. `fake_input_device_id`
no longer falls back to 4/5 for an unknown device.

**Tests (B through the dispatcher, reading the client's error and the peer
bytes):**
1. A device button press on id 77 (absent): `BadDevice`, errorValue 77,
   nothing delivered, and held state unchanged. *Kills:* the 4/5 fallback.
2. Core FakeInput buttons 10 / 0 / 11: 10 is delivered as detail 10; 0 and 11
   give `BadValue` with no delivery. *Kills:* the old `_ => drop` arm.
3. A keycode outside the keymap range → `BadValue`.

### Task 2 — XTEST 4/5 class shape and device type (C3, C4)

**Do:** `XIQueryDevice` of 4 reports exactly the `CorePointerProc` shape
(above). XI1 `XListInputDevices` / `XOpenDevice` report the matching XI1
classes for 4. In `XListInputDevices`, devices 2, 3, 4 and 5 carry
`type = None` (0). Physical facets keep `MOUSE` / `KEYBOARD` / `TOUCHPAD`.
Do not change physical slave shapes.

**Tests:**
1. Wire parse of `XIQueryDevice(4)`: one button class with 10 labels (the last
   three 0), two valuator classes (Rel X / Rel Y, min/max -1, mode relative),
   and no scroll class. *Kills:* sharing the physical pointer encoder.
2. `XListInputDevices`: type atom 0 on 2..5, and `MOUSE` on a physical pointer.

### Task 3 — Master classes mirror the last slave, with stored sourceid (C2, C5)

**Do:** each master keeps the class set it currently mirrors and the
`sourceid` stored in it. The initial set is the `CorePointerProc` shape for
master 2 and the existing key class for master 3, with the master's own ID.
A slave switch (the existing `announce_xi2_slave_switch` /
`update_from_pointer_master` path) replaces it with the new slave's classes
and that slave's ID. Clearing the last slave (disable, removal) does not
change the stored set. `XIQueryDevice(2/3)` and the `DeviceChanged` class
block are encoded from the stored set.

**Tests (B via input and lifecycle):**
1. Fresh server: `XIQueryDevice(2)` reports `sourceid` 2 and the core shape.
2. Move the physical mouse (id 6): `sourceid` 6 and 6's shape. Disable or
   remove 6: still `sourceid` 6 and the same shape. Then an XTEST motion:
   `sourceid` 4 and 4's shape. *Kills:* deriving sourceid from the last-slave
   field; encoding master 2 with the physical shape unconditionally.

### Task 4 — Button state in queries (C1)

**Do:** `ButtonClass.state` carries the queried device's held buttons. That
is `buttons_down` for a slave and the master's aggregated buttons for a
master, in the XI2 bit layout (bit n for button n,
`Xi/xiquerydevice.c:282-285`).

**Tests:**
1. Hold physical button 1 on id 6 and XTEST button 3 on 4. Then query 6 →
   bit 1 only; 4 → bit 3 only; 2 → bits 1 and 3. Release both → all zero.
   *Kills:* the zero-filled state words; using master state for slaves.

### Task 5 — Explicit XI2 grabs float XTEST 4/5 (B)

**Do:** `detach_xi2_slave` and its callers float 4 and 5 like a physical
facet on an explicit (non-implicit) XI2 grab. While floating, XTEST input to
4 has its own position and no master effect, and input to 5 uses floating
XKB state, exactly as the existing physical-floating rules. Ungrab and client
disconnect re-attach them. A same-device XI2 grab replacement keeps the slave
floating, with its saved master and floating position: `DetachFromMaster`
returns at once for an already floating device (`dix/events.c:1463-1464`), and
the old grab is freed without a reattach (`:1649-1650`). Plan 1's "disabled never
reattaches" does not apply, because 4/5 are always enabled.

**Tests (B through `XIGrabDevice` / `XIUngrabDevice` requests and XTEST
input):**
1. Grab 4. `XIQueryDevice(4)` shows use 5 / attachment 0, and a hierarchy
   event flags the detach as Xorg does. An XTEST motion on 4 does not move
   the master pointer. Ungrab: attached again, and the next XTEST motion moves
   the master. *Kills:* the `facet.is_none()` early return.
2. Grab 5, then inject a key on 5: the master keyboard state is unchanged.
3. Grab 4, move it with XTEST, then replace the grab with another XI2 grab
   from the same client: 4 stays floating at the moved position. *Kills:*
   reattach-then-detach on replacement.
4. (A) Grab 4 and disconnect the client: 4 is re-attached, and
   `xi2_detached_masters` and `floating_pointer_positions` are empty.

Check first whether Xorg sends a hierarchy event for a grab-time detach
(`DetachFromMaster` → `AttachDevice` and its callers). Follow Xorg; if it
sends none, test that none is sent.
