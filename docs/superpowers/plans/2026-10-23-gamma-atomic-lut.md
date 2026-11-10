# Gamma through atomic `GAMMA_LUT`; remove the legacy `SETGAMMA` ioctl

Short plan (no spec: the gamma cache, sizes and reapply points stay; only the last step, how the LUT reaches the CRTC, changes).
It revises one decision of `specs/2026-06-18-randr-gamma-design.md` (#47), which chose the legacy ioctl and listed atomic
`GAMMA_LUT` as a non-goal.

## Why

`apply_gamma_to_live_output` (`kms/render/backend/kms/randr.rs:1554`) ends in the legacy `DRM_IOCTL_MODE_SETGAMMA`. On
amdgpu, i915/xe and nvidia-drm (615.78.08) none of the drivers has its own `gamma_set` hook, so the kernel
(`drm_crtc_legacy_gamma_set`, `drm_color_mgmt.c`) turns that ioctl into its own **blocking** atomic commit carrying a
`GAMMA_LUT` blob of the legacy size, and clears `DEGAMMA_LUT` and `CTM` in the same commit. That is the second commit per
CRTC that #47 wanted to avoid, made synchronously by the kernel outside our per-CRTC commit stream, and it blocks the
core loop while a flip is pending.

#47's reason for legacy (a separate atomic commit EBUSY-collides with scanout flips) no longer holds: the cursor series
(#272, #276, #287, #288, #290) put cursor state on the same per-CRTC stream, and this change does the same for gamma.

Driver history (when each driver gained `GAMMA_LUT`): i915 v4.7 (2016), amdgpu DC v4.17 (2018), nouveau v5.4 (2019),
nvidia-drm 545.23.06 (2023, legacy and atomic in the same release), msm dpu v7.0 (2025, both in the same release); Apple
DCP has no gamma. Every one predates its driver's Vulkan 1.2 support, or arrived together with legacy gamma.

**No legacy fallback.** The legacy ioctl is removed. No target driver has legacy-only gamma: nvidia-drm gained the legacy
gamma size and `GAMMA_LUT` in the same release (545.23.06), so on older drivers, including 470, the newest branch for
Kepler and yserver's minimum recommended NVIDIA driver, `SETGAMMA` already had nothing to act on (gamma size 0); msm the
same in v7.0. Checked in the nvidia-drm sources of 470.256.02, the last Kepler release: no `gamma_set`, no legacy gamma
size, no `GAMMA_LUT` (the word does not appear). 545.23.06 and later do not support Kepler (Maxwell is the oldest), so
Kepler on nvidia-drm has no gamma on master either; Xorg gets it there through NVIDIA's own DDX, not DRM. Kepler on
nouveau has `GAMMA_LUT` (nv50 and later, since v5.4), so this change gives it gamma. The drivers that do have their own legacy `gamma_set` cannot run yserver: radeon and amdgpu without DC
are not atomic (yserver requires atomic KMS, `drm/device.rs:180`), and vmwgfx, atomic but legacy-only (its `gamma_set`
writes the SVGA palette), has no hardware Vulkan. The other paravirtualized drivers (virtio-gpu, qxl, bochs,
cirrus-qemu, hyperv, xen) have no gamma at all, and vboxvideo reports a legacy size with no `gamma_set`, so `SETGAMMA`
did nothing there. A CRTC without `GAMMA_LUT` reports gamma size 0, as a CRTC without gamma does today.

## Out of scope

- **The RANDR-visible gamma size.** It stays the legacy `gamma_length` (256 on amdgpu/i915, 1024 on NVIDIA). Xorg
  modesetting always reports 1024 (`xf86RandR12InitGamma(pScrn, 1024)` from `xf86HandleColormaps`); that change belongs
  with #263 (VidMode gamma) or its follow-up, which reworks the same LUT code.
- `CTM`, `DEGAMMA_LUT` as client features, HDR, VRR.

## Design

- **Blob size = the legacy `gamma_length`.** That is exactly the blob the kernel builds today inside `SETGAMMA`, so every
  driver that accepts the legacy ioctl accepts it (amdgpu and i915 take a 256-entry `GAMMA_LUT` as their legacy mode:
  `amdgpu_dm_verify_lut_sizes`, `lut_is_legacy`; nvidia-drm's two sizes are both 1024). No client-visible change.
- **Same side effects as today:** each gamma commit also sets `DEGAMMA_LUT` = 0 and `CTM` = 0 where the CRTC exposes them,
  as the kernel's legacy path does.
- **Transport:** the pending LUT rides the next **vsync** commit of that CRTC: scene flip (`drm/page_flip.rs:235`), direct
  scanout (`drm/modeset.rs:1736`) and composed scanout (`drm/modeset.rs:1801`). Async (tearing) flips cannot carry CRTC
  properties, so the LUT waits for the next vsync commit, as cursor positions already do (`drm/page_flip.rs:231`).
- **Nothing in flight:** when no commit is pending or queued for that CRTC, the LUT goes in its own nonblocking commit
  with a page-flip event, through the same per-CRTC stream and commit identity as the other commits, so the scheduler
  sees the CRTC busy until its event, and EBUSY retries on the CRTC's next event.
- **Size:** `GetCrtcGammaSize` is the legacy `gamma_length` when the CRTC has `GAMMA_LUT`, otherwise 0.
- **Failure:** EINVAL on a commit carrying `GAMMA_LUT` latches gamma off for that CRTC (warn once, the LUT stays cached,
  the frame is resubmitted without it); frame handling is otherwise unchanged. Other errors keep the LUT pending.
- **Removed:** the `set_gamma` call in `apply_gamma_to_live_output` and any helper left without a caller.

## Reuse inventory

- **Property lookup and caching:** `PropMap::for_object(..).id(..)` (`drm/modeset.rs:1130`) and the cached optional
  properties on `Output` (`plane_in_fence_fd_prop` / `crtc_out_fence_ptr_prop`, `drm/modeset.rs:203-209`): add
  `crtc_gamma_lut_prop`, `crtc_degamma_lut_prop` and `crtc_ctm_prop` the same way. `None` for `GAMMA_LUT` = no gamma.
- **Blob lifecycle:** `create_property_blob` / `destroy_property_blob` as the mode blob (`drm/modeset.rs:1365-1414`); the
  user handle is destroyed after submit, the kernel keeps its reference in the CRTC state.
- **The LUT itself:** the per-output cache `gamma_luts`, `cached_gamma_for_current_size`, `resample_channel` and the size
  query (`randr.rs:1474-1552`) stay unchanged.
- **Reapply points stay** (modeset `crtc_config.rs:1570`, DPMS-on / VT restore `crtc_config.rs:1163`, composed re-light
  `scanout.rs:1049`); they now mark the LUT pending instead of calling the ioctl.
- **Standalone commit:** `AtomicCommitSink::submit_with_commit_id` with `PAGE_FLIP_EVENT | NONBLOCK`, as the cursor-only
  commit does (`kms/render/platform/cursor_atomic.rs:136`), including its EBUSY handling pattern; and the existing
  in-flight checks the scanout scheduler already consults (`cursor_commit_in_flight_for_outputs`, `scanout.rs:908`,
  `present.rs:922`) so that a scene flip is not submitted over a pending gamma commit.

## Tasks

### Task 1: atomic gamma transport

Capability on `Output`, pending LUT per output, the LUT riding vsync scene/direct/composed commits, the standalone
commit when nothing is in flight, the EINVAL latch, removal of the `SETGAMMA` call.

Tests (Backend-driven, rule B; end state checked, rule A):
- a CRTC with `GAMMA_LUT`: `SetCrtcGamma` issues 0 `SETGAMMA`, the next vsync commit carries `GAMMA_LUT` with a blob of the
  legacy size, `DEGAMMA_LUT` = 0 and `CTM` = 0, and the pending LUT is cleared after it is accepted. Mutation: keep
  calling `set_gamma`.
- idle CRTC (no flip in flight or queued): a standalone commit with a page-flip event carries the LUT; a scene flip
  submitted before that event is held until it. Mutation: submit the scene flip immediately.
- async (tearing) flip: carries no CRTC property; the LUT goes on the next vsync commit. Mutation: add it to the async flip.
- EBUSY on the standalone commit: retried on the CRTC's next event, not dropped.
- a CRTC without `GAMMA_LUT` (even with a legacy size): `GetCrtcGammaSize` = 0, no ioctl, no commit. EINVAL on a
  gamma-carrying commit: gamma latches off for that CRTC, the frame still goes out. Mutation: drop the latch.
- no `SETGAMMA` anywhere: grep in the gate (`set_gamma(` absent under `crates/yserver/src/drm` and `kms`).
- reapply after a modeset and after VT resume still reaches the hardware (now through the commit).
- `GetCrtcGammaSize` with `GAMMA_LUT`: the legacy size, unchanged.

Hardware (rule C), NVIDIA and AMD, release build, baseline on master first (this is also #47's pending smoke):
gammastep (or redshift) changing gamma while mpv plays fullscreen in direct scanout (mpv rendering on the same GPU via
`--vulkan-device`; by default it picks the first Vulkan device and a cross-GPU buffer never reaches direct scanout) and while idle; strace from server start: `SETGAMMA` count (0 on the branch), commits carrying
`GAMMA_LUT`, mpv dropped frames, and that the colour visibly changes and survives a VT switch.

### Task 2: on an active CRTC, gamma waits for the next frame

**Measured on Task 1 (AMD Raphael, mpv fullscreen 60 fps in direct scanout, 10 gamma changes):** every change cost mpv
one frame, on master (legacy `SETGAMMA`, 8 drops) and on Task 1 (8–9 drops). All 13 Task 1 gamma commits were
standalone `gamma-only`, none rode a direct commit: at 60 fps the CRTC is usually free between frames when
`SetCrtcGamma` arrives, so "nothing in flight" sent the LUT in its own event commit, which takes the vblank and holds
the next direct frame.

**Change (user decision 2026-10-23).** "Nothing in flight" is no longer enough for the standalone commit:

- **Active CRTC** (a page flip retired on it within the last two refresh periods, from the existing per-CRTC
  retirement clock `completion_clocks` / `ust_msc`, `kms/render/platform.rs:482-492`, and the mode's `vrefresh`): the
  pending LUT waits for the next vsync scene/direct/composed commit, which already carries it (Task 1). If none is
  submitted within **one refresh period** of the request, the standalone commit goes out at that deadline.
- **Idle CRTC:** the standalone commit goes out immediately, as in Task 1.
- The deadline is exposed through the existing `next_wakeup` (`backend_scanout_next_wakeup`, `scanout.rs:2773`); no
  new timer. A later `SetCrtcGamma` before the deadline replaces the pending LUT and keeps the first deadline (it does
  not push it out). Async flips still carry nothing; the deadline still applies.

**Tests** (Backend-driven, rule B; end state, rule A):
- active CRTC in direct scanout: `SetCrtcGamma`, then a direct frame inside one refresh period: the direct commit
  carries `GAMMA_LUT`, 0 `gamma-only` commits. Mutation: submit standalone immediately (Task 1 behaviour).
- active CRTC, no frame arrives: `next_wakeup` returns the deadline; one tick after it, exactly one `gamma-only`
  commit. Mutation: no deadline (the LUT waits forever).
- idle CRTC (no retirement in two periods): standalone commit at once. Mutation: always defer.
- two `SetCrtcGamma` before the deadline: one commit carrying the second LUT, at the first deadline.
- active scene (composed, windowed) output: the next scene flip carries it.

**Hardware:** the Task 1 run (`gamma-hw.sh`) on AMD, master vs branch: mpv drops per gamma change (target 0), the
count of `gamma-only` commits during playback (target 0), kernel LUT readback unchanged; plus windowed mpv. NVIDIA the
same when the monitor is on it.

**Measured on NVIDIA (RTX 5060 Ti, nvidia-drm 615.78.08, 1080p60, mpv fullscreen in direct scanout):** master drops 7
frames for the 10 changes, Task 2 drops 0, but all 10 changes went out as standalone `gamma-only` commits: the CRTC
was never seen as active. The page-flip event timestamp (UST) on this driver is about 0.97 s behind
`CLOCK_MONOTONIC` at receipt (326 flips: min 0.966 s, median 0.966 s, max 0.984 s), so the last retirement always
looked older than two refresh periods. On AMD the UST is aligned with `CLOCK_MONOTONIC`, which is why Task 2 worked
there.

**Fix (user decision 2026-10-23).** Activity is measured from when the page-flip event was received
(`event_at_monotonic`, already captured for every flip), not from the kernel UST. No dependency on the driver's clock
domain, and the `BOOTTIME` comparison goes away. Present keeps the kernel UST as today.

- Test: a CRTC whose last page-flip event was received within two refresh periods but whose UST is 1 s older is
  active; the direct frame carries the LUT, 0 `gamma-only` commits. Mutation: decide from the UST again.
- Test: a CRTC whose last event was received more than two periods ago is idle even when its UST looks recent:
  standalone at once. Mutation: decide from the UST again.
- Hardware: the NVIDIA run again (fullscreen, windowed, strace), target 0 `gamma-only` commits during playback; AMD
  rerun to confirm no change there.

**Measured on NVIDIA with two outputs (HDMI-2 + DP-3 on card1, mpv fullscreen on HDMI-2, DP-3 idle):** none of the 10
changes made during playback reached DP-3's CRTC (kernel readback stayed at identity until the final reset); master
applied all of them on both CRTCs. The standalone commit waits while any scanout work is queued, and that check
(`gamma_output_has_queued_scanout`) includes scene-wide state (repaint owed, presentation damage, direct scanout
pending or queued) that the other output keeps busy. A held LUT is retried only on its own CRTC's page-flip event or
at the deadline, and an idle CRTC has neither.

**Fix (user decision 2026-10-23).** When the standalone commit cannot go out at request time, the one-refresh deadline
is set for idle CRTCs too, so the existing expiry path (`next_wakeup` → `retry_pending_gamma_after_flip`) sends it.
The LUT is never held longer than one refresh period.

- Test: two outputs, scene work queued for output 0, output 1 idle: `SetCrtcGamma` on output 1 gives exactly one
  `gamma-only` commit on output 1's CRTC at the deadline, none before it. Mutation: set the deadline only for active
  CRTCs.

### Task 3: docs

`docs/status.md` entry; the #47 spec gets a note pointing to this plan for the transport decision.

Gate (every task): fmt, `cargo build --bin yserver`, workspace + lavapipe ignored suite, clippy `-D warnings`,
`tools/check-layout.sh`, FreeBSD cross-clippy; load runs per rule F.

## Review round (#293, maintainer review 2026-10-24)

Four defects, fixed as Tasks 4–7; one note corrected in Task 8. Each task: one implementation, one review, at most
one fix round.

### Task 4: gamma while DPMS is off only marks the LUT pending

With DPMS off every CRTC has `ACTIVE=0`, and the kernel rejects a commit that requests an event on an inactive CRTC.
`apply_gamma_to_live_output` sent the standalone `gamma-only` commit without checking the DPMS state, so a
`SetCrtcGamma` from a night-light tool on a blanked screen got EINVAL, latched gamma off for good (the latch survives
the modeset), and the DPMS-on reapply did nothing. The deadline paths are already gated on the outputs being on;
the request path was not.

**Fix.** While the outputs are off, `SetCrtcGamma` sends no commit: the immediate submission is skipped and the LUT
stays pending with its deadline (the deadline paths already wait for the outputs to be on). The DPMS-on path
already marks the LUT pending again, and the next commit on the CRTC carries it.

- Test: DPMS off, `SetCrtcGamma`: no atomic commit, gamma not latched off; DPMS on: the LUT reaches the CRTC and a
  later `SetCrtcGamma` still applies. Mutation: drop the DPMS check.

### Task 5: EINVAL latches gamma off only when the same frame succeeds without the LUT

On the scene, direct-scanout and composed paths, any EINVAL on a commit carrying the LUT turned gamma off for that
CRTC, even when a plane, the cursor or a modeset race caused it. The three paths already retry the frame without the
LUT after an EINVAL.

**Fix.** One helper shared by the three paths: submit with the LUT; on EINVAL, retry without it; latch gamma off only
when that retry succeeds. When the retry fails too, the LUT stays pending and the retry's error is returned to the
caller as today. The standalone `gamma-only` commit carries only colour properties and keeps its direct latch.

- Test: a plane makes every commit fail with EINVAL while a LUT rides the frame: gamma stays enabled and the LUT stays
  pending. Mutation: latch before the retry.
- Test: only the commit carrying the LUT fails with EINVAL: the frame goes out without it, gamma is latched off once.
  Mutation: never latch.

### Task 6: gamma size from `GAMMA_LUT_SIZE` when the legacy size is 0

A driver can expose `GAMMA_LUT` and `GAMMA_LUT_SIZE` without setting a legacy gamma size; the size then read 0 and
RandR reported no gamma although the hardware has a LUT. New drivers are likely to look like that.

**Fix.** When the legacy `gamma_length` is 0 and the CRTC has `GAMMA_LUT`, the size (RandR and blob) is
`GAMMA_LUT_SIZE`. Where the legacy size is non-zero nothing changes.

- Test: a CRTC with `GAMMA_LUT`, `GAMMA_LUT_SIZE` = 4096 and legacy size 0 reports 4096 to RandR and its blob has
  4096 entries. Mutation: drop the fallback.
- Test: legacy size 256 and `GAMMA_LUT_SIZE` 4096 still reports 256. Mutation: prefer `GAMMA_LUT_SIZE`.

### Task 7: a LUT commit waits until no other CRTC of the device has a commit in flight

Measured on AMD Raphael (two outputs, 30 gamma steps 30 ms apart, `strace -T`): commits carrying a LUT took p50 15 ms
with an idle desktop and 12 ms with mpv on one output (gamma-only 27 ms), max 34 ms; with one output, 0.6 ms. Commits
without a colour change took 0.02 ms. amdgpu takes a global lock for a commit that changes the colour state, and that
lock waits for the last commit of every CRTC on the device to be done and flipped, inside the ioctl, even with
NONBLOCK. With two outputs each CRTC's LUT waits for the other CRTC's flip. One LUT per CRTC per frame (newest wins)
already held, so coalescing alone does not help.

**Fix.** A pending LUT is added to a commit only when no other CRTC on the same device has a commit in flight;
otherwise it stays pending. It is retried on the page-flip events of every CRTC of the device, and the existing
one-refresh deadline still applies: at the deadline the LUT goes out even if the device is busy (the stall is then
paid, as today), so it is never held longer. A frame is never held by this rule: it goes out without the LUT. An active CRTC keeps the Task 2 wait. On other drivers the cost is at most
one refresh of gamma latency.

- Test: two outputs on one device, output 1 has a commit in flight: `SetCrtcGamma` on output 0 adds no LUT to any
  commit until output 1's event, then exactly one commit carries it. Mutation: check only the LUT's own CRTC.
- Test: the device stays busy past the deadline: the LUT goes out at the deadline. Mutation: drop the deadline
  fallback for a busy device.
- Hardware: the same four runs (one/two outputs, idle/mpv) on AMD; target: LUT commits near the one-output cost with
  two outputs.

### Task 7b: the device gate has no time bypass

Measured after Task 7 on AMD (same four runs): two outputs still p50 15 ms idle and 12.7 ms with mpv. A traced burst
shows the cause: a LUT that arrives while its own CRTC's previous commit is in flight is held with the one-refresh
deadline, the deadline is spent waiting on that CRTC, and when its event finally arrives the expired deadline bypasses
the device gate while the other CRTC is in flight; the ioctl then blocks 27 ms. With two outputs that are not in phase
this happens almost every time.

**Fix.** The device gate has no deadline exception: while another CRTC of the device has a commit in flight the LUT
is not added, whatever its deadline. It goes out at the first frame, standalone attempt or device-wide flip-event
retry at which no other CRTC is in flight; the wait is bounded by that CRTC's event (every in-flight commit has one).
A frame is never held. An active CRTC keeps the Task 2 wait before its deadline. An expired deadline on a LUT held by
the gate must not make the event loop wake immediately in a loop.

- Test: output 0's LUT held past its deadline by its own commit, output 1 in flight when output 0's event arrives: no
  LUT until output 1's event, then exactly one. Mutation: restore the deadline exception.
- Test: the device stays busy past the deadline: the LUT does not go out at the deadline (replaces the Task 7 test).
- Test: no immediate-timeout spin with a gate-held, expired deadline. Mutation: keep reporting the expired deadline.
- Hardware: the four runs on AMD again; target: two-output LUT commits near the one-output cost (~0.6 ms), no frame
  drops in mpv, gamma still applied on every CRTC (kernel readback).

**Tasks 7 and 7b reverted (review of #293).** The gate makes LUT submission depend on the other CRTCs: on a
Radeon 680M with two outputs, each running a fullscreen mpv, the CRTCs committed strictly alternately and the first LUT
waited 40 frames; with identical pixel clocks the gap may never open. Bounded recovery is out of proportion for this
change. The submission path is the Task 6 one again; the amdgpu multi-output stall is documented in `docs/status.md`.

### Task 8: docs

The NVIDIA note: measured on nvidia-drm 615.78.08, the flip UST is about 0.97 s behind `CLOCK_MONOTONIC`, with
`CLOCK_BOOTTIME` equal to `CLOCK_MONOTONIC` (no suspend since boot), so it is not BOOTTIME. The comment in
`flip_events.rs` describing a suspend-inclusive UST ahead of monotonic stays; it gets one line for the offset behind
it. `docs/status.md` entry for the review round.
