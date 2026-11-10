# Per-window hotplug delivery, the two limitations, VT writes, docs — plan 3 of 3

**Spec:** [maintainer-review addendum](../specs/2026-10-04-xi-registry-maintainer-review-addendum.md)
sections E1, F1, F2, G and H. **Branch:** `feat/xi-dynamic-registry-rebased`.
**Scope:** maintainer items X7 (delivery multiplicity), X8, T3, S1 and S2.
Runs after plan 2.

**Executor and rules:** identical to [plan 1](2026-10-04-xi-enabled-state-and-floating.md),
"Rules for every task".

---

### Task 1 — One hierarchy/presence event per selecting window (E1)

**Do:** `emit_xi_hierarchy_changed` and `emit_xi1_device_presence`
(`xinput/hotplug.rs`) deliver one copy per window with a matching selection,
not one per client. Hierarchy uses an `XIAllDevices` selection with
`XI_HierarchyChangedMask`. Presence uses the XI1 presence class selected on
that window. Follow `SendEventToAllWindows` (`dix/events.c`): the windows
walked (root and descendants), and the per-window selection test.
`DeviceChanged` already goes per window; keep it.

**Tests (B: real `XISelectEvents` / `SelectExtensionEvent` requests, then a
lifecycle event):**
1. One client selects hierarchy on root and on a child: two identical
   hierarchy events. A second client selecting only on root gets one.
   *Kills:* collecting targets per client.
2. The same for XI1 presence.
3. (A) Destroying the child window removes its selection; the next event
   comes once.

### Task 2 — Owner-events fallback per event (F1)

**Do:** under an owner-events XI grab, each event (smooth-scroll Motion,
emulated button, plain motion) is first offered to natural delivery for the
owner. Only an event that, by its own type, reached nobody falls back to the
grab window with the grab mask (`DeliverGrabbedEvent`,
`dix/events.c:4431-4464`). Remove the ButtonPress-derived fallback marker
that scroll Motion depends on today (`pointer_fanout.rs`, the separate scroll
Motion path).

**Tests (B via input):**
1. The documented case: client A selects master `XI_ButtonPress` on a mapped
   child, grabs master 2 on root with `owner_events = true` and Motion in the
   grab mask, and scrolls over the child. The emulated ButtonPress arrives on
   the child, and the smooth Motion arrives on root with root-relative
   coordinates. *Kills:* the marker coupling.
2. Both selected on the child: both arrive on the child, and nothing on root.

### Task 3 — LockMods with affect=lock under slave grabs (F2)

**Do:** a floating keyboard's release of a key whose press began while
attached applies the action's real XKB semantics (`xkb/xkbActions.c:372-395`),
including `affect=lock` (`LockNoUnlock`). Do not unconditionally clear the
saved bits (`backend.rs`, the floating-release path).

**Tests:**
1. The documented scenario: Caps bound to `LockMods(modifiers=Lock,
   affect=lock)`, Lock on, press Caps, grab that keyboard while held, release,
   then type a key: Lock stays on. *Kills:* the unconditional clear.
2. Default Caps (affect both): the existing test still passes, with Lock
   toggling off.

### Task 4 — No configuration write waits for resume (G)

**Do:** at VT release, before yielding, the core consumes the input thread's
results for every configuration write submitted before the pause. The FIFO
places them ahead of the pause acknowledgement. Applied writes commit and
reply; writes that never reached a handle reply `BadMatch` with nothing
committed. Remove the "resolve after rebind" path and its `SourceGone`
continuation case for writes submitted before the pause.

**Tests (B via the runner's real lane and lifecycle):**
1. Submit an accel write, then VT release before its completion is read: the
   client gets its reply before the release finishes, and the inventory
   carries the applied value. *Kills:* holding the write for resume.
2. Submit a write whose handle was retired first: `BadMatch`, the inventory
   unchanged, and no reply after resume.

### Task 5 — Docs (H)

- `docs/status.md`: remove the two "known limitations". Add one line saying
  `xinput set-prop 4 …` targets the virtual XTEST pointer, and point to the
  per-device loop in the main design.
- Fold the four `docs/superpowers/findings/2026-09-30-dynamic-xinput-adversarial-*.md`
  files and `2026-09-30-dynamic-xinput-local-checks.md` into
  `2026-10-02-xi-registry-branch-review.md` as one short dispositions table,
  delete the originals, and fix the links in the main design's "Review
  correction record".
- In the same way, fold this round's
  `2026-10-04-maintainer-addendum-review.md` and
  `2026-10-04-plan*-review.md` into
  `2026-10-04-pr198-maintainer-review-triage.md`, so the PR adds one findings
  file for the maintainer review.

### Task 6 — Final hardware pass (user watching)

Run plan 1 Task 7 again on the final binary. Add on MATE and i3: wheel and
smooth scroll under the window manager's grabs (Mod+drag, a menu open while
scrolling), and `xdotool` XTEST clicks including button 10. The coordinator
sends the watch list first. Touchpad: maintainer side.
