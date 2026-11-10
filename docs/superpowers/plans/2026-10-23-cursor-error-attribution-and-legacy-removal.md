# Cursor error attribution and removal of the legacy cursor ioctls

Short plan (no spec: nothing new is built; one rule is narrowed and an unused path is deleted).

## 1. Only EINVAL is attributed to the cursor (maintainer finding 2)

**Problem.** When an atomic commit carrying cursor state failed, every error except EBUSY was blamed on the cursor (`crates/yserver/src/drm/atomic.rs`), and callers rolled the cursor back on every non-EBUSY failure. An ENOMEM or an EIO therefore triggered cursor recovery (detach, demotion, sprite release, possibly a direct unflip) for no cursor reason.

**Change.** Only EINVAL, the kernel rejecting the configuration, is attributed to the cursor. Scene commits keep the existing full-versus-stripped TEST_ONLY comparison, and EBUSY is unchanged. Other errors keep their normal frame/commit handling without touching the cursor.

**Tests.** Cursor-only and position-carrying scene commits failing with ENOMEM and EIO, including a scripted full-test failure with a stripped-test success, cause no detach, qualification change or lease release. An EINVAL control still recovers. Mutation: "non-EBUSY means cursor".

## 2. Remove `set_cursor2` / `move_cursor`

**Who used Legacy:**

- paravirtualized drivers (`virtio_gpu`, `vmwgfx`, `qxl`, `vboxvideo`) on guests older than Linux 6.8. `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT` exists since Linux 6.8 (commit 9724ed6c1b12);
- a failed driver-name query;
- a cursor plane missing a required property;
- no cursor plane at all (Apple DCP). It reached the software cursor through the legacy probe's unsupported-ioctl latch.

**Why it is safe:**

- **No supported GPU depends on it.** yserver needs Vulkan 1.2 with dynamic rendering and synchronization2. Among VMs only virtio-gpu with Venus provides that; vmwgfx, qxl, vboxvideo and virgl-only virtio-gpu have no hardware Vulkan, and lavapipe is refused for scanout by default. Atomic KMS entered the kernel in 2015 (Linux 4.2) and Vulkan 1.2 shipped in January 2020, so every consumer GPU whose driver supports Vulkan 1.2 has an atomic KMS display driver.
- **Clients see no difference.** Cursor selection, XFIXES `GetCursorImage`, serials, hotspot and hide/show are server-side, and root readback already omits the cursor.

**Reuse inventory:**

- **The static decision:** the decision P2 makes once per device (`cursor_plane/topology.rs`) now says `Software` instead of `Legacy`.
- **The software cursor:** the existing per-device latch `permanently_disabled` (`kms/render/platform.rs`), which already sent no-plane devices to the software cursor, is now set from that decision instead of from the legacy probe.
- **Partial plane coverage** (an active CRTC without a cursor plane) keeps the existing `topology_blocked` handling. It is recomputed on every topology refresh, so the hardware cursor comes back when the topology is covered again.
- **Deleted:** the Legacy cursor executors, the shared legacy buffer and its show/move/upload, the legacy probe, Legacy-only state and telemetry, and Legacy-only tests.

**Size.** Production code +650 / −1567 and tests +2782 / −4366 for the whole branch, review follow-up included. Most added production lines are re-flow of code that kept only its atomic branch.

**Tests.**

- Each former Legacy device class shows the software cursor and issues no legacy cursor ioctl. Mutation: route a non-qualified device back to a hardware path.
- Two CRTCs with one cursor plane: the device is blocked while that topology is active and gets the atomic cursor back after a refresh to a covered layout. Mutation: make the coverage check part of the static decision.

## 3. Review follow-up

- **Live, bounded recovery for non-EINVAL direct failures.** A direct-scanout cursor hide/show that fails with an error other than EINVAL or EBUSY is retried on a 50 ms deadline exposed through `next_wakeup`, so an idle held direct frame recovers without another frame or CRTC event. It reuses the per-CRTC `CursorHideRecovery` record and the existing direct-cursor-pending set; after 3 failed attempts (the bound shared with motion recovery) the output unflips. EBUSY keeps the event-driven retry. Test: two outputs, EIO on output 0's hide while output 1's hide retires, further ticks with no frame: output 0 recovers once the error clears; a persistent error unflips after the bound.
- **Policy tests ported to atomic fixtures:** held-direct entry with a visible cursor, entry waiting for a queued show-on-retire, handoff keeping the newest copy, VT-suspended hide retried on resume, failure classification isolated across cards with the same raw CRTC. The software fallback test now checks scanout pixels.
- **Cleanup:** unreachable non-atomic arms in scanout and the non-commit-id submit branch removed, a stale `set_cursor2` comment fixed, a legacy-named test renamed.

## Finding 4 (layout guard) not done

With the executors removed there is no legitimate legacy cursor ioctl left to allowlist. A guard would only catch a deliberate reintroduction, which review already catches.

## Validation

Release builds with an Adwaita `watch` animated cursor and pointer motion, the ioctls counted with `strace` from server start:

| | cursor path | legacy cursor ioctls | atomic ioctls | `no matching entry bytes` |
|---|---|---|---|---|
| NVIDIA RTX 5060 Ti | `atomic reason=qualified` | 0 | 167 | 0 |
| AMD Raphael | `atomic reason=qualified` | 0 | 166 | 0 |
