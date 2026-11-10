# XI enabled state, floating slaves and "Device Enabled" — plan 1 of 3

**Spec:** [maintainer-review addendum](../specs/2026-10-04-xi-registry-maintainer-review-addendum.md)
sections A1–A7 and E2, on top of the
[registry design](../specs/2026-09-29-dynamic-xinput-device-registry-design.md).
**Branch:** `feat/xi-dynamic-registry-rebased` (worktree
`~/Projects/yserver-xi-dynamic-registry`).
**Scope:** only the maintainer's items X1 and X2, plus E2 (it rewrites the same
lifecycle emitters). Plan 2 covers X3–X6; plan 3 covers E1, X8, G and the
docs. Do not implement anything from those plans here.

**Executor:** codex `gpt-6-luna`, reasoning effort `xhigh`, one dispatch per
task (send-backs at `max`). The coordinator reviews each diff and runs the
gate before committing. Codex does not commit.

## Rules for every task

- Read `AGENTS.md`, the addendum, and the actual code at the entry points.
  Line numbers are search hints.
- **Tests (standing project rules):**
  - **(A) end state and leaks:** each scenario checks the registry, the held
    state, `xi2_detached_masters` / `floating_pointer_positions`, the property
    maps and the client selections afterwards, not only the emitted bytes.
  - **(B) real paths:** drive requests through the core dispatcher, and input
    or lifecycle through `on_host_input` / `dispatch_host_input`. Do not call
    the helper under test directly.
  - **(C) hardware:** only Task 7, run with the user watching.
- Every test names the mutation it kills in a comment. The coordinator applies
  that mutation (by line, after confirming the build compiled it) and the test
  must fail.
- Write the test first and show it failing.
- Per-task gate: `cargo +nightly fmt` first. Then `cargo test -p yserver-core
  --lib`, `cargo test -p yserver --lib`, `cargo test -p yserver-protocol --lib`,
  each `crates/yserver/tests/*.rs` file, `cargo clippy --all-targets -- -D
  warnings`, and the focused tests 5× under 16 busy loops.
- No `YSERVER_*` environment variables. No new features beyond the addendum.
  Stop and report a mismatch with Xorg instead of inventing wire behavior.

## Interfaces introduced (names are suggestions; keep the semantics)

- In the registry, per physical facet: a **session-enabled** fact (VT, add
  before enable) and a **client-disabled** fact (from `Device Enabled`). The
  existing `XiDevice::enabled` becomes their derived value: session-enabled
  and not client-disabled. Masters and 4/5 are always enabled.
- **Transition order (A3, Xorg `devices.c:466-468` before `:504-506`):** a
  disable first drains the facet's holds while it is still enabled, because
  synthetic releases pass through `on_host_input`, which drops input from a
  disabled facet. Only then does it commit the disabled fact (session or
  client), then the property, presence and hierarchy steps, and the float
  last.
- The **home master** is derived from the facet role: keyboard → 3, pointer →
  2. It is never stored as "last attachment".
- `Backend` hooks for a client-driven facet disable/enable, for example
  `disable_xi_facet(state, device_id)` / `enable_xi_facet(state, device_id)`,
  with no-op defaults. KMS implements them with the same cleanup owner
  `release_device_state` uses, scoped to one facet.
- Inventory: client-disabled facet kinds per source, kept for server reset.

---

### Task 1 — Registry: two enabled facts, derived floating

**Do:** split the enabled state as above. **Completed-transition invariant:**
once a disable transition has finished, a facet that is not enabled reports
`attached_master = None`. The one exception is the `XIDeviceDisabled`
descriptor of that same transition (Task 2), which is built from a snapshot
taken before the float: the old attachment, with `enabled = false`. This covers queries (`query.rs`
`device_descriptor` / `slave_descriptor`) and hierarchy data (`hotplug.rs`
`hierarchy_descriptor`): `use = 5`, `attachment = 0`. Enabling attaches the
facet to its home master. A facet floated by an XI grab (`detach_xi2_slave`)
and a disabled facet are distinct. Re-attaching after a grab
(`reattach_xi2_slave`) must not attach a disabled facet. A disable while a
grab holds a facet floating removes its saved `xi2_detached_masters` entry.

**Invariants:**
- `XIQueryDevice` and `XListInputDevices` agree, before and after a disable.
- No master aggregation (`attached_pointer_holds_button`, master key state)
  counts a disabled facet.

**Tests (B through `dispatch_host_input` plus requests):**
1. Suspend a source. `XIQueryDevice` reports the facet with `use = 5`,
   `attachment = 0`, `enabled = 0`. Resume it: it reports use 3/4 with
   attachment 2/3. *Kills:* keeping `attached_master` while disabled.
2. A disabled facet that was XI-grabbed: ungrab must leave it floating, and
   `xi2_detached_masters` must be empty afterwards. *Kills:* an unconditional
   reattach.

### Task 2 — Lifecycle order: add floating, disable then float, one facet at a time

**Do:** rewrite the `DeviceAdded` / `DeviceResumed` / `DeviceSuspended` /
`DeviceRemoved` arms in `kms/render/backend.rs` and `release_device_state` to
follow addendum A3, A4 and E2 per facet, in facet creation order:

- **Add:** the facet is registered session-disabled and floating, then
  `DevicePresence(Added)` and `XISlaveAdded` are sent (use 5, attachment 0,
  enabled 0). The enable then attaches it, sends the `Device Enabled` property
  event (Task 3 adds the property; until then that step is skipped),
  `DevicePresence(Enabled)`, and `XIDeviceEnabled` (now attached). The next
  facet follows.
- **Disable** (suspend or remove): release holds (existing owner) while the
  facet is still enabled, clear last slave, mark disabled, send the property event, `DevicePresence(Disabled)`,
  and `XIDeviceDisabled`, built from a descriptor snapshot taken before the
  float (old use/attachment, `enabled = false`). The emitter takes that
  snapshot explicitly rather than reading the current registry. Only then
  float it. **Remove:** `DevicePresence(Removed)`, then `XISlaveRemoved`
  per facet.
- Every hierarchy event flags exactly one device.

**Tests (B via `dispatch_host_input`, with a client selecting hierarchy and
presence):**
1. Add a mixed keyboard+pointer source. The exact wire sequence is Added(k),
   SlaveAdded(k, use 5), Enabled(k), DeviceEnabled(k, use 4 attach 3), then
   the same four for the pointer facet. *Kills:* one event flagging both
   facets; SlaveAdded reporting use 4.
2. Suspend it. The `XIDeviceDisabled` descriptor reports use 4 / attach 3, and
   a query right after reports use 5. *Kills:* floating before the event is
   built.
3. (A) After remove, the registry, the floating maps and the held state hold
   nothing for the source.

### Task 3 — "Device Enabled" on every device

**Do:** seed `Device Enabled` (type `INTEGER`, format 8, one item,
`deletable = false`) on 2, 3, 4 and 5 at server init and reset, and on every
physical facet at registration with the value 0. It is created without a
property notification. Enable and disable transitions update it with an XI1
`DevicePropertyNotify` and an XI2 `XI_PropertyEvent`, at the step position
Task 2 defines. Continuation (`xi_register_source` refresh) must neither
clear nor duplicate it.

**Tests:**
1. `XIListProperties` and XI1 `ListDeviceProperties` list it on 2..5 and on a
   physical facet. `XIGetProperty` returns 1 after enable and 0 after suspend.
   *Kills:* seeding only physical facets.
2. A client selecting property events sees exactly one change per
   transition, and none at creation. *Kills:* notifying at seed time.
3. Server reset: 2..5 still have it with value 1.

### Task 4 — Writes to "Device Enabled"

**Do:** XI2 `XIChangeProperty` and XI1 `ChangeDeviceProperty` on this atom
follow addendum A5:

- anything but format 8 / `INTEGER` / one item → `BadValue`;
- 0 on 2..5 → `BadAccess`; 1 on 2..5 → success, no change;
- on a physical facet, 0 → if the facet is enabled,
  `Backend::disable_xi_facet` runs the Task 2 disable sequence for that facet
  only: holds are drained first, and then the client-disabled fact is
  committed. If it is already session-disabled, only the fact is set; nonzero → the fact is cleared, and the facet
  is enabled when it is session-enabled;
- `XIDeleteProperty` / XI1 delete → `BadAccess`;
- `GetProperty(delete)` keeps the existing reply-and-unlink rule.

Per-client request order is preserved relative to queued libinput writes.
Use whatever serialization the lane already applies to ordinary property
writes.

**Tests (B through the dispatcher):**
1. `xinput disable` equivalent on a mixed source's pointer facet: the pointer
   floats and gets the disable sequence. The keyboard facet stays enabled
   and attached, and keys from the source still reach clients (A6). Pointer
   events from that source are dropped before any state change (cursor
   position and `buttons_down` unchanged). *Kills:* disabling the whole
   source.
2. Disable while a button is held: the release reaches clients, and the master
   button state clears. *Kills:* skipping the facet cleanup.
3. 0 on 2, 3, 4, 5 → `BadAccess`, and nothing changes. A format-16 write →
   `BadValue`. Delete → `BadAccess`.
4. Write 0 while only session-disabled (suspended): exactly one property
   event, no transition, and nothing recorded. Resume re-enables it normally
   (Xorg `devices.c:162-165`, `xf86Events.c:316-320`).
5. Notification order for a client disable: the A3 sequence (property 0,
   presence, hierarchy), then one final property event. Writing 5 to a
   disabled facet enables it and `XIGetProperty` returns 5. *Kills:*
   normalizing the stored byte; omitting the outer event.

### Task 5 — libinput writes to disabled facets (A7)

**Do:** replace "new writes to suspended sources fail BadMatch" with
"recognized libinput property writes fail `BadMatch` when no facet of the
source is enabled", using the same validation order as today. A write to a
client-disabled facet whose sibling facet is enabled is applied.

**Tests:**
1. A mixed source with its pointer client-disabled: an accel write to the
   pointer facet succeeds and reaches the backend. Disable the keyboard too:
   the same write gets `BadMatch` and nothing reaches the backend. *Kills:*
   the per-facet check; the old suspended-only check.

### Task 6 — Client-disabled survives VT, not reset (A1)

**Do:** a client-disabled facet stays disabled across VT suspend/resume (no
Disabled or Enabled events for it on either side). Across a server reset it
is NOT kept: Xorg re-creates and enables every device for the new generation
(`dix/main.c:247-249, 299-306`, `devices.c:779-784`), and the inventory
replay already registers facets enabled.

**Tests (B via `dispatch_host_input`):**
1. Client-disable a pointer, run a VT round trip: no events for that facet,
   and it is still floating. The other facets get Disabled/Enabled. *Kills:*
   re-enabling every facet on resume.

### Task 7 — Hardware check (user watching)

Build the release binary. The coordinator sends the user, before starting, the
steps, what to watch and what counts as wrong. On the test tty:
`xinput list`; `xinput disable <Razer pointer id>` (the cursor stops for that
mouse, the HyperX still moves it, `xinput list` shows it floating);
`xinput enable` (the mouse moves again); a VT round trip while holding a key
(no stuck key or modifier, and Caps Lock kept); a replug (new ID, Added →
Enabled); MATE reapplies its settings. Any visual deviation is a finding.
