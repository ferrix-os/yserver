# Dynamic XI registry: maintainer-review addendum (PR #198)

**Status:** draft, 2026-10-04, for codex review and user approval.
**Amends:** [the dynamic XI device registry design](2026-09-29-dynamic-xinput-device-registry-design.md).
Where they disagree, this addendum wins.
**Source:** the maintainer's review of PR #198, triaged in
[the triage record](../findings/2026-10-04-pr198-maintainer-review-triage.md).

Every behavior below is Xorg's, checked against `xorg-server-21.1.24`
(`../xserver`, read via `git show xorg-server-21.1.24:<path>`) and
`xf86-input-libinput` master (`ac86267`). The contract is Xorg behavior
observable on the wire, not the shape of Xorg's code.

## Non-goals

These stay out even though Xorg has them. Reviewers and implementers must
not add them:

- the `Coordinate Transformation Matrix` property that `AddInputDevice` also
  creates (`dix/devices.c:313-323`);
- `XIChangeHierarchy` attach/detach/add-master/remove-master for physical
  devices (still rejected, as today);
- direct touch, an `xorg.conf.d` `InputClass` reader, and any server-wide
  acceleration default;
- any new `YSERVER_*` environment variable that switches behavior.

## A. Enabled state, floating, and the "Device Enabled" property (X1, X2)

**A1. Two reasons to be disabled.** A slave is enabled only when it is
neither *client-disabled* (a client wrote `Device Enabled = 0`) nor
*session-disabled* (VT away, or not yet enabled after add). These are two
independent facts. Xorg keeps them apart the same way: VT leave records an
already disabled device in `XI86_DEVICE_DISABLED` and VT enter does not
re-enable it (`hw/xfree86/common/xf86Events.c:302-320`). A client-disabled
facet stays disabled across a VT round trip. It does not survive a server
reset: Xorg closes and re-creates every device for the new generation
(`dix/main.c:247-249, 299-306`, `devices.c:1052-1123`), and devices come up
enabled (`devices.c:779-784`). The replay therefore registers every facet
enabled.

**A2. A disabled slave floats.** While disabled, a slave reports
`use = XIFloatingSlave (5)` and `attachment = 0` in `XIQueryDevice` and in
hierarchy data, takes part in no master aggregation, and is not the master's
last slave. `DisableDevice` clears the master's `lastSlave` and sets
`dev->master = NULL` (`dix/devices.c:487-492, 539`). `EnableDevice` attaches
it again to the master its role implies before turning it on
(`dix/devices.c:388-393`). The "home" master is therefore derived from the
role (pointer → 2, keyboard → 3), never from the last attachment. A slave
floated by an XI grab (A6) and a disabled slave are separate states. A grab
ending on a disabled slave must not re-attach it.

**A3. Order of a disable** (`DisableDevice`, `dix/devices.c:449-540`):
release held keys/buttons through the existing cleanup owner; clear the
master's last-slave reference; mark it disabled; set `Device Enabled` to 0
with a property notification; send XI1 `DevicePresenceNotify(DeviceDisabled)`;
send XI2 hierarchy `XIDeviceDisabled`; and only then float it. The hierarchy
event of the disable is built before `dev->master = NULL`, so it still reports
the slave attached. Queries made afterwards report it floating.

**A4. Order of an add and an enable** (`ActivateDevice` `dix/devices.c:604`,
`EnableDevice` `:362-430`). A new slave is published floating: its
`XISlaveAdded` hierarchy data and any query before the enable report `use = 5`,
`attachment = 0`, `enabled = false`, and `Device Enabled = 0`. The enable
attaches it, sets `Device Enabled` to 1 with a property notification, sends
`DevicePresenceNotify(DeviceEnabled)`, and sends `XIDeviceEnabled`, which now
reports it attached. The existing presence-before-hierarchy order stays. The
property-creation write at add time sends no notification (`devices.c:308`,
last argument `FALSE`). VT leave and enter run the same disable/enable with
events (`xf86Events.c:312, 319`, `sendevent = TRUE`). A resumed facet is
therefore floating while away and attached again with `XIDeviceEnabled`
afterwards.

**A5. The property.** Every device, masters 2/3 and XTEST 4/5 included,
has `Device Enabled` (`XI_PROP_ENABLED`): type `INTEGER`, format 8, one item,
not deletable (`devices.c:308-311`). The property handler
(`DeviceSetProperty`, `devices.c:149-167`) applies these rules:

- anything other than format 8 / `INTEGER` / size 1 is `BadValue`;
- writing 0 to 2, 3, 4 or 5 is `BadAccess`, and writing 1 there succeeds
  without change;
- on a physical facet that is enabled, 0 runs A3 and sets the
  client-disabled fact. On a client-disabled facet, nonzero clears the fact
  and runs A4's enable;
- a write that changes no state (1 on an enabled device, 0 on a disabled one,
  either value on 2..5 except 0) runs no transition. In particular, 0 written
  while the facet is only session-disabled records nothing: Xorg's handler
  acts only when `dev->enabled` (`devices.c:162-165`), and VT enter re-enables
  the device (`xf86Events.c:316-320`);
- after the handler, every successful write stores the client's bytes as
  written (a nonzero value other than 1 stays as written) and sends one more
  property notification (`Xi/xiproperty.c:759-801`). A transition therefore
  produces its full A3/A4 sequence, including its own property event, followed
  by this final property event;
- any nonzero value means enabled (`devices.c:162-165` tests the byte for
  zero);
- `DeleteProperty` on it is `BadAccess` (`Xi/xiproperty.c:657`, not
  deletable). `GetProperty(delete)` follows the main design's existing
  reply-and-unlink rule, which does not check deletable;
- XI1 and XI2 property requests see the same value.

**A6. Input from a disabled facet.** Input from a disabled facet is dropped
before cursor, XKB, held state, RECORD or fanout can change. The existing
rule for suspended sources extends to client-disabled facets. A source with
two facets keeps delivering through the facet that is still enabled.

**A7. libinput writes to a disabled facet.** xf86-input-libinput closes the
shared libinput device only when every facet of that source is off
(`xf86libinput_shared_disable`, `src/xf86libinput.c:416-432`). A recognized
libinput property write then fails `BadMatch` (`xf86libinput_check_device`,
`:4392-4410`). While a sibling facet is still on, the write is applied. The
"new writes to suspended sources fail BadMatch" rule becomes "fail BadMatch
when no facet of the source is enabled".

## B. Grabs on XTEST slaves (X3)

An explicit (non-implicit) XI2 grab on any slave floats it for the duration
of the grab, XTEST 4/5 included (`ActivatePointerGrab`,
`dix/events.c:1622-1624`, and its keyboard twin). While floating, XTEST input
to 4/5 follows the same floating rules as a physical slave: no master state,
its own position, its own XKB state. Ungrab, client disconnect and grab
replacement re-attach it exactly like a physical slave.

## C. XIQueryDevice classes (X4, X5)

**C1. Button state.** `ButtonClass.state` reports the queried device's
currently held buttons (`Xi/xiquerydevice.c:282-285`). A master reports the
master's aggregated state; a slave reports its own.

**C2. Master source ID.** The `sourceid` of a master's class is stored in
that class when it is copied from a slave (`DeepCopyDeviceClasses`,
`Xi/exevents.c:592, 630`). `XIQueryDevice` serializes the stored value
(`Xi/xiquerydevice.c:278, 326`). It is not derived from the current last
slave. Clearing the last slave on disable or removal (A3) leaves the copied
classes and their `sourceid` unchanged until another slave's classes replace
them. Before any slave has supplied classes, the master's own initial classes
carry its own ID.

**C3. XTEST shape.** The XTEST pointer 4 has the `CorePointerProc` shape:
10 buttons with the core labels, 2 relative valuators (`InitPointerDeviceStruct`, `devices.c:1644-1646`), and no scroll classes
(`dix/devices.c:655-700`). XTEST input on button 10 is delivered, not dropped.

**C4. Device type.** Masters 2/3 and XTEST 4/5 have no XI1 device type
(`type = None` in `XListInputDevices`). Physical facets keep `MOUSE`,
`KEYBOARD` or `TOUCHPAD`.

**C5.** A master's class shape follows its last slave. While the last slave
is XTEST 4, the master shows 4's shape, and a `DeviceChanged(SlaveSwitch)`
carries that shape.

## D. XTestFakeInput target validation (X6)

An XI device event (`type & 0x7f >= first_event`) names `deviceid & 0x7f`.
A device that does not exist is `BadDevice` with
`errorValue = deviceid & 0x7f`. Nothing is injected, and the request does not
fall back to 4/5 (`Xext/xtest.c:182-186`). The other Xorg checks in the same
block (a key event needs a key class, a button event a button class, motion a
valuator class: `BadValue`; DeviceMotion without its valuator event:
`BadLength`, `xtest.c:190-250`) are applied in Xorg's order.

## E. Hierarchy and presence delivery (X7)

**E1. One event per selecting window.** Each `XI_HierarchyChanged` event and
each XI1 `DevicePresenceNotify` goes once to every window with a matching
selection, the same as `SendEventToAllWindows`
(`Xi/xichangehierarchy.c:119`; `SendDevicePresenceEvent`, `dix/devices.c:333-346`). A
client that selected on two windows receives two identical copies. Neither
event has a window field (`XI2proto.h` `xXIHierarchyEvent`; XI1
`devicePresenceNotify`), so the copies are byte-identical apart from the
sequence number. The selection test is Xorg's: `XIAllDevices` for hierarchy, and the
presence class for XI1.

**E2. One device per transition.** Each facet gets its own `XISlaveAdded`,
then its own `XIDeviceEnabled` (each with presence before hierarchy), and on
removal its own `XIDeviceDisabled` then `XISlaveRemoved`. A source with a
keyboard and a pointer facet produces two complete sequences, one facet at a
time, never one event that flags both facets. The order between facets is
Xorg's add order: the order the facets are created.

## F. The two documented limitations become fixes (X8)

**F1. Owner-events fallback is decided per event.** Under an owner-events
grab, each event is first offered through natural delivery for the owner. Only
if that event, by its own type, reached nobody does it fall back to the grab
window with the grab mask (`DeliverGrabbedEvent`, `dix/events.c:4431-4464`).
A smooth-scroll Motion therefore falls back on its own, regardless of whether
the paired emulated ButtonPress was delivered naturally.

**F2. Lock actions under slave grabs.** A floating keyboard's release of a
key whose press began while attached applies the action's real XKB semantics,
including `affect=lock` / `LockNoUnlock` (`xkb/xkbActions.c:372-395`). It does
not clear saved modifier bits unconditionally.

## G. Configuration writes at VT release (T3)

No XI property request stays unanswered across a VT switch. Xorg handles the
whole request before it processes the VT switch, so a write is either applied
or rejected. At VT release, before the session is yielded, every
configuration write already submitted to the input thread completes. Either
libinput's result is committed and replied, or the write is answered
`BadMatch` and nothing is committed. Nothing is held for resume. The input-thread command queue is FIFO
(main design, "Pause/resume commands are delivered in FIFO order"), so every
submitted write is executed or refused by the input thread before it handles
the pause. Its result is reported ahead of the pause acknowledgement. Before
yielding, the core consumes those results and replies to their clients. A
write libinput applied is committed, never rejected. A write that never
reached a libinput handle is answered `BadMatch` and nothing is committed.
Writes that arrive while away fail `BadMatch` (A7). The "may resolve after rebind"
clause of the main design is withdrawn.

## H. Documentation (S1, S2)

- Release notes / `docs/status.md`: `xinput set-prop 4 …` now targets the
  virtual XTEST pointer and configures no physical device. Use the device's
  own ID instead (see the main design's acceleration section).
- The four `2026-09-30-dynamic-xinput-adversarial-*` findings files and the
  local-checks file are folded into the branch-review summary. Their
  dispositions are kept as a short table, and the originals are deleted.

## Acceptance

Each item ships with tests that meet the project's standing rules:

- **(A) end state and no leaks:** after each scenario, the registry, the
  held state, the floating/detached maps and the client selections are
  checked to be exactly as they were before it;
- **(B) driven through the core loop's real request and input paths**, not
  helper calls;
- **(C) hardware tasks named per real path:** A3/A4 via a real VT round trip
  holding a key, `xinput disable`/`enable` on a real mouse, and a replug, plus
  F1 under a grab-heavy desktop on this machine (MATE, and i3). These are run
  with the user watching. The touchpad-laptop run (T1) needs hardware we do
  not have. The maintainer offered to run it on their side, and the PR reply
  asks them to cover it; it is not a gate this branch can close.

Each test names the mutation it must kill, for example: dropping the float
in A3, building the disable hierarchy after the float, sending one hierarchy
event per client instead of per window, defaulting an unknown XTEST device to
4, or keeping the "resolve after rebind" wait.

## Amendments after the maintainer's second review (2026-10-05)

- **C3:** every XI1 consumer of the class shape follows the device's actual
  classes: `XListInputDevices`, `XOpenDevice`, `GetDeviceButtonMapping`,
  `SetDeviceButtonMapping` and `QueryDeviceState`.
- **C6:** a wheel button (4–7) becomes smooth-scroll Motion only when the source
  device has a scroll class in that direction. Xorg `GetPointerEvents` converts
  through the device's `v_scroll_axis` / `h_scroll_axis` (`dix/getevents.c`). XTEST
  4 has none, so it sends plain wheel buttons, and physical devices keep smooth
  scrolling.
- **G:** on a barrier timeout, the in-flight write is cancelled with a token the
  input thread checks before applying. If libinput had already applied it, the
  late result is reconciled: inventory and property are committed, and the
  property event is sent. The client keeps the `BadMatch` it received, and no
  second reply is sent.
- **A1:** a session disable records `session_enabled = false` even for a facet
  that is already client-disabled. Enabling it while the session is away clears
  only the client fact, and the facet re-enables at resume. Xorg itself ends up
  inconsistent in this sequence: `EnableDevice` fails while the device fd is
  paused, the device stays flagged `XI86_DEVICE_DISABLED` at VT enter, and the
  outer store leaves the property at 1. The two-reason model is kept
  deliberately.
