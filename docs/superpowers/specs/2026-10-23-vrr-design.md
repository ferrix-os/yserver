# Variable refresh rate (VRR / adaptive sync) — design

**Date:** 2026-10-23 · **Status:** revision 5.1 (2026-10-26, Mesa is a requirement of `--vrr all` on NVIDIA; standard parsers only); revision 5 (2026-10-26, `--vrr all` is `--vrr` on every driver but `nvidia-drm`; follow-up PR to #294, rebased on revision 2.1); revision 4 (2026-10-25, the NVIDIA list applies only on `nvidia-drm`); revision 3 (2026-10-25, `--vrr all` honours the drivers' application lists); revision 2.1 (2026-10-26, maintainer review of PR #294: cursor hold scope, Xorg's `_VARIABLE_REFRESH` reading, standalone clear framebuffer); revision 2 (2026-10-25; `--vrr all` moved to a follow-up PR the same day, see Revision history), from the maintainer review of draft PR #294: trigger, default, clear priority, gamma/VRR
attribution, animated cursors; see Revision history); revision 1 approved by the user (2026-10-23), reviewed once (sol);
plan `docs/superpowers/plans/2026-10-24-vrr.md` · **Scope:** KMS backend,
one output covered by a client window in direct scanout; cursor behaviour while VRR is active.

## Why

A game that renders at a rate the monitor cannot divide evenly (50 fps on a 60 Hz panel, 170 fps on a 240 Hz panel)
either waits for the next fixed vblank (stutter, latency) or tears. A VRR monitor can instead extend its vertical front
porch until the next frame arrives, so each frame is shown as soon as it is ready. Linux exposes this through two KMS
properties: the connector's `vrr_capable` (read-only) and the CRTC's `VRR_ENABLED` (set by userspace).

yserver has no VRR today: no code reads `vrr_capable` or writes `VRR_ENABLED`.

Xorg modesetting can only enable VRR while it flips one window that covers the whole X screen, and it enables it on
every CRTC at once. yserver composes and commits per output, so VRR can be enabled on the one monitor that shows the
game while the others keep their own fixed rate. This document covers the first step: one output, direct scanout.

## Goals

1. **VRR on the output a client window covers.** While VRR is enabled on the command line, the direct-scanout owner of
   an output is a client window that allows VRR (see Decisions), and that output's connector reports
   `vrr_capable` = 1, the output's CRTC runs with `VRR_ENABLED` = 1. Otherwise it runs with `VRR_ENABLED` = 0.
2. **Frames reach KMS when they are ready.** In direct scanout a client Present is already submitted when the buffer is
   ready, with at most one flip in flight and one queued successor. With VRR that flip ends the front porch, so no new
   frame clock is needed. This goal is "keep it that way": nothing in this design may make a direct Present wait for a
   fixed vblank boundary while VRR is on.
3. **Cursor motion does not drive the refresh rate.** While VRR is on for a CRTC, a new cursor position rides the
   client's next frame commit on that CRTC. If no frame commit arrives within **33 ms** of the first position that is
   still unsent, the position goes out in its own commit. Cursor image changes keep today's path (see The cursor while
   VRR is on).
4. **Off by default, one command-line flag.** Without the flag yserver never writes `VRR_ENABLED` = 1. `--vrr` enables
   VRR for windows that set `_VARIABLE_REFRESH` = 1; `--vrr all` enables it for every client window in direct scanout
   whose application the installed NVIDIA and Mesa lists do not exclude (see Application exclusions).
   No environment variable (`YSERVER_*` variables that change behaviour are not allowed).
5. **Logs that let a tester confirm it works** (see Logging).

## Non-goals

- Several outputs: VRR on an output composed by the per-output scene path (backlog step 3), and per-output direct
  scanout (backlog step 4). Direct scanout today requires the window to cover the whole root, so with several monitors
  this design does not activate VRR unless one window covers all of them.
- HDR, colour management, `CTM` / `DEGAMMA_LUT`.
- Changing Present's MSC/UST semantics. MSC still counts flips/vblanks as today; UST still comes from the flip event.
- yserver's own window rules or application lists. The only per-application exclusions are the ones the installed
  drivers ship and the user's own overrides of those files (revision 3).
- Tearing (async flips) combined with VRR. The tearing mode keeps working as today; see Interaction with tearing.

## Anti-goals (a failure even if VRR technically works)

1. **The desktop never runs in VRR.** Without a client window owning direct scanout of an output, that output keeps its
   fixed refresh. Panels can change brightness with the refresh rate; normal desktop use must not show that.
2. **No change without VRR.** On an output whose connector lacks `vrr_capable`, or with no client window in direct
   scanout, yserver behaves exactly as before: same commits, same cursor commits, same timing.
3. **Turning VRR on or off is invisible:** no black frame and no full modeset.
4. **Cursor motion never holds the monitor at its maximum refresh** while a client controls the refresh rate. A single
   cursor commit sent at the 33 ms deadline may happen to precede a client frame closely; what must not happen is a
   sustained stream of cursor-driven refreshes.

## Decisions (approved in conversation, 2026-10-23)

- **Activation model: a client window that covers the output** (the behaviour KWin uses for fullscreen windows; see
  Provenance), narrowed in `--vrr` mode by the window's `_VARIABLE_REFRESH` (below).
- **`_VARIABLE_REFRESH` is honoured in `--vrr` mode** (revision 2; revision 1 did not read it). Mesa sets the
  property to 1 on the presented window unless its driconf denylist (`adaptive_sync=false`: browsers, compositors,
  shells) excludes the application, in which case it deletes it; it never sets 0. Clients depend on that denylist, so
  yserver follows Xorg modesetting here. The property is read when direct scanout of the window starts, and a change
  while the window is in direct scanout (set, deleted, value changed) re-evaluates *wanted* at once.
- **`--vrr all` exists for NVIDIA.** The NVIDIA proprietary client never sets `_VARIABLE_REFRESH` (the string is absent
  from `libGLX_nvidia.so.615.78.08`). It decides per application (`__GL_VRR_ALLOWED`, profile key `GLVRRAllowed`, with
  its own denylist of compositors and shells) and passes that decision to `nvidia_drv.so` over NV-GLX, which yserver
  does not implement. In `--vrr` mode NVIDIA clients therefore never get VRR. `--vrr all` ignores the property.
- **`--vrr all` honours the drivers' application lists** (revision 3, user decision; revision 2 accepted that no
  application could be excluded). Without them a fullscreen browser, video player, Steam or a login greeter would
  run with variable refresh: brightness flicker on many panels at irregular frame rates, and a cursor that waits up
  to 33 ms for frames that do not come. yserver reads the lists that the drivers themselves install and apply inside
  the client, so it keeps no list of its own (see Application exclusions). A compositor's image stays excluded by
  ownership (next decision), independent of the lists.
- **What the lists must guarantee** (revision 4, user decision). No compositor or desktop shell gets VRR; ownership
  already ensures it for a compositor's image, and both lists name the compositors again. A browser or video player
  that neither list names (mpv today) may get VRR; that is accepted until a user reports a problem.
- **NVIDIA driver floor: 580** (revision 4, user decision). Older branches (Kepler's last is 470) lack the atomic KMS
  support yserver requires; Maxwell's last branch is 580. Profile files of older drivers are not a target.
- **"Client window", not the compositor's image.** Only a direct-scanout owner classified as `ScanoutM0Target::Unredirected`
  activates VRR. `Cow` and `CowDescendant` (a compositing manager's overlay window and its children, such as Muffin's
  stage) never do: that buffer is the whole desktop. With a compositing manager, VRR therefore follows whether the
  manager unredirects the fullscreen game.
- **Cursor deadline 33 ms.** NVIDIA's own cursor path in NVKMS and KWin both use 1000/30 ms for the same problem.
- **Off by default** (revision 2, maintainer request; revision 1 was on by default). Some panels flicker at low or
  changing frame rates, and every fullscreen video player would otherwise run with variable refresh.
- **Argument form.** `--vrr` takes an optional value: the next argument is consumed only if it is exactly `all`.
  Repeating the flag: the last one wins, as for `-listen`/`-nolisten`. Double dash because yserver keeps single-dash
  flags for Xorg-compatible options and uses double dash for its own (`--headless`, `--scale`; maintainer request).

## Kernel and driver behaviour this design relies on

Read in the sources (behaviour only; see Provenance). The items marked *by reading* are not yet measured on hardware.

1. **`VRR_ENABLED` is not a modeset property.** The DRM core only stores it in the CRTC state; it is not one of the
   conditions that make a commit need `ALLOW_MODESET`.
2. **amdgpu (Linux 7.1.9):** a change of `VRR_ENABLED` is processed in atomic check without a modeset and maps to the
   FreeSync state (variable or inactive); it takes effect with the next flip of the primary plane, through a stream
   update. Since commit 3477c1b0972d (2025, also in stable), an atomic commit on a CRTC with VRR active pulls the
   primary plane in and starts a refresh cycle, including cursor-only commits. Legacy cursor ioctls are excluded from
   that, but yserver no longer has them (#290).
3. **nvidia-drm / NVKMS 615.78.08:** every modeset through DRM prepares VRR on the head without activating it;
   `VRR_ENABLED` decides per flip whether the flip is a VRR flip (*by reading*). A cursor change in an atomic commit is
   part of an NVKMS flip, and a flip triggers an immediate refresh (*by reading*). On NVIDIA, VRR needs DisplayPort.
4. **Kernel UAPI documentation:** the front porch is extended until a page flip or timeout; large, frequent changes of
   the front porch can worsen perceived stutter and cause brightness flicker.

Consequences: VRR can be switched with the frame commits yserver already sends (no modeset, goal 3 of the anti-goals),
and every standalone cursor commit on an active VRR CRTC is a refresh on both drivers, which is why goal 3 exists.

## Design

### State

Per output, two facts and one decision:

- **capable:** the connector exposes `vrr_capable` with value 1 and the CRTC exposes `VRR_ENABLED`. The property ids
  are cached where the other optional properties are cached when an output is built (the pattern of
  `crtc_gamma_lut_prop`, `plane_in_fence_fd_prop`, `crtc_out_fence_ptr_prop` on `Output`). The `vrr_capable` value must
  also be refreshed on the connector rescan path, which keeps the live `Output` across a disconnect/reconnect and only
  updates connector metadata (`platform/connectors.rs`, rescan); a monitor swap on the same connector must not keep a
  stale value. A missing property means not capable.
- **wanted:** VRR is enabled on the command line and the direct transaction being submitted for this output is owned
  by a client window (`Unredirected`) that allows VRR: in `--vrr all` mode on `nvidia-drm` unless the presenting
  client's application is excluded (Application exclusions), in `--vrr` mode (and `--vrr all` on other drivers) when
  the window's `_VARIABLE_REFRESH` reads as Xorg modesetting reads it (revision 2.1): format 32, exactly one item, any type, any
  nonzero value (absent, another format or item count, or 0: not allowed). Xorg ignores a non-matching change and
  keeps the previous state; yserver evaluates the stored value, so such a change turns VRR off (no client depends on
  the difference). It
  follows the owner of the transaction being submitted, not the retained direct record: first entry submits before the
  pending record is installed, and the previous direct record survives until a composed replacement retires.
- **committed:** the `VRR_ENABLED` value the kernel holds for this CRTC. It starts from the kernel's current value, read
  when the output is built, and is updated when a commit carrying the property is accepted.

`VRR_ENABLED` is written only on a CRTC that exposes it, and only when the wanted value (*capable* and *wanted*) differs
from *committed*. An output whose kernel value is already 0 and that never qualifies never receives the property
(anti-goal 2). A value of 1 inherited from a previous DRM master (at startup or on VT acquire) differs from the wanted
0 and is cleared by the next vsync commit of that CRTC, so the desktop does not run in VRR (anti-goal 1).

### Turning VRR on and off

- **On:** when direct scanout of an output starts with a client-window owner (after the existing direct-entry probation;
  no second probation), *wanted* becomes true.
- **Off:** *wanted* becomes false when direct scanout of that output ends for any reason (unflip, owner changes to the
  compositor's image, the window is unmapped/destroyed/reconfigured, topology change, per-CRTC RandR config, DPMS off,
  VT release, shutdown), when the output is no longer capable, or in `--vrr` mode when the owner's `_VARIABLE_REFRESH`
  stops allowing VRR. A property change that turns *wanted* on rides the window's next direct frame as usual.
- **Transport:** the new value rides the next vsync commit of that CRTC, the same way a pending gamma LUT does
  (`feat/gamma-atomic-lut`): scene flip, direct submit or composed commit. Async (tearing) flips cannot carry CRTC
  properties, so the value waits for the next vsync commit. The pending-CRTC-property mechanism added for gamma is
  generalized to carry this value too, rather than duplicated.
- **One feature class per request** (revision 2). A frame request carries pending gamma LUTs or VRR changes, never both,
  so an EINVAL and its stripped retry are attributed to one class only: a VRR rejection never latches off gamma on
  another output, and the reverse.
  - A VRR **clear** (wanted 0, committed 1) never waits: it goes into the frame that needs it, and every pending gamma
    LUT of that request waits for the next frame (gamma's pending deadline already schedules one).
  - A VRR **enable** may wait: while any output in the request has a pending gamma LUT, the enable stays wanted and
    rides the next frame without gamma.
  - The `VRR_ENABLED` = 0 that the device-release disable carries is not a frame request and keeps its own retry.
- **Off without a frame.** When *wanted* turns false and no frame of that CRTC is due (for example a `--vrr` property
  change on a window whose direct scanout continues without new frames, or a composed replacement that is already
  idle), the clear must not wait for a later unrelated frame: the next pass submits it in a standalone commit, as the
  gamma-only commit does.
- **Before giving up the device** (VT release, shutdown), a CRTC whose *committed* value is 1 gets one commit with
  `VRR_ENABLED` = 0 so the next DRM master (fbcon or another server) does not inherit VRR. If no vsync commit is due at
  that point, a standalone commit carries it, as the gamma-only commit does. This is one attempt: if it fails, the
  failure is logged (warn) and the release proceeds; the VT switch or shutdown never waits on it.
- **Failure:** if a commit carrying `VRR_ENABLED` = 1 is rejected with EINVAL, VRR is latched off for that CRTC (warn
  once) and the frame is resubmitted without the property. Other errors keep the change pending. This is the gamma
  EINVAL latch, reused with gamma's lifetime: it survives an `Output` replacement on the same kernel CRTC
  (`platform/connectors.rs`, where gamma's latch and pending state are carried over) and ends with that CRTC.

Whether VRR on/off needs the primary plane in the same commit (amdgpu applies it on a primary flip) is satisfied by
every frame path: the on change rides a direct frame, and a composed unflip carries a primary-plane replacement
(`drm/modeset.rs`, composed commit). The one commit that may carry no primary flip is the standalone clear before
releasing the device; the builder must check on amdgpu that it takes effect, and if it does not, include the current
primary framebuffer in that commit. The standalone clear while a client is idle carries the primary framebuffer of
the live direct frame only (revision 2.1): with no live direct frame it is not submitted and a debug line says so,
so no path can flip back to a stale composed frame. A VRR-only standalone commit has its own commit kind, not gamma's.

### Application exclusions in `--vrr all`

Applies only in `--vrr all` mode on a `nvidia-drm` scanout device. In `--vrr` mode the client already applied its
driver's list (Mesa deletes the property for an excluded application), and the property decides.

- **Which process.** The presenting client's process id comes from its connection (`SO_PEERCRED`, the helper X-Resource
  already uses for `LocalClientPID`). Its name is derived the way Mesa derives it: the base name of `/proc/<pid>/exe`
  when that path is a prefix of `argv[0]` (`/proc/<pid>/cmdline`), otherwise the base name of `argv[0]`. A client
  without a process id (TCP, or a platform where the helper reports none, such as FreeBSD today) keeps revision 2's
  behaviour: not excluded. This is logged once per client at debug level.
- **`--vrr all` outside NVIDIA is `--vrr`** (revision 5, user decision; supersedes revision 4's "Mesa list on every
  driver"). When the scanout device's DRM driver is not `nvidia-drm` (read through the existing
  `CursorDeviceCaps::driver_name()`; no new detection), `--vrr all` behaves exactly as `--vrr`: the property decides,
  no list or `/proc` file is read, and one startup info line says so. A Mesa client there already applied Mesa's own
  list, so the result is the same without the differences a re-implementation would bring (clients that never set the
  property, driconf forms yserver does not evaluate).
- **Which lists apply on `nvidia-drm`.** Both: the NVIDIA list (profiles and `__GL_VRR_ALLOWED`) for NVIDIA clients,
  and the Mesa list for Mesa clients presenting there, since an excluded Mesa client sets no property and cannot be
  told apart from an NVIDIA client otherwise. The Mesa list complements the NVIDIA one, which names no browser or
  video player (revision 5.1, user decision).
- **Mesa is a requirement of `--vrr all` on NVIDIA** (revision 5.1, user decision). The Mesa list is
  `/usr/share/drirc.d/00-mesa-defaults.conf`, installed by Mesa (Arch and Alpine `mesa`, Debian and Ubuntu
  `libgl1-mesa-dri`, Fedora `mesa-dri-drivers`, FreeBSD `mesa-dri`, Gentoo `media-libs/mesa`); yserver neither ships
  nor copies it. `docs/setup.md` lists it under Requirements. When `--vrr all` runs on `nvidia-drm` and no
  `/usr/share/drirc.d/*.conf` file exists, one startup warning names the missing file and the package; the NVIDIA
  list still applies.
- **The client's environment wins over the files.** From `/proc/<pid>/environ`: `__GL_VRR_ALLOWED` (NVIDIA) or `adaptive_sync` (Mesa's driconf override by environment) set to `0`/`false` excludes;
  set to `1`/`true` allows even a listed application, unless the other variable excludes. An unreadable environment
  counts as no override.
- **NVIDIA application profiles.** Files, in increasing precedence: the driver's
  `/usr/share/nvidia/nvidia-application-profiles-*-rc`, then `/etc/nvidia/nvidia-application-profiles-rc` and the
  `.d` directory next to it, then `$HOME/.nv/nvidia-application-profiles-rc` and its `.d` directory (`$HOME` of the
  server's user). A profile excludes when it sets `GLVRRAllowed` to false (the key every 580+ profile file uses; the
  older `GLGSYNCAllowed` is not read); a rule maps a process name to a profile
  with `"feature": "procname"` or the string-pattern shorthand. Other rule features (`dso`, `findfile`, compound
  patterns) are not evaluated. The format is JSON with `#` comment lines; the comment lines are dropped and the rest is parsed with
  `serde_json`. A user profile that sets `GLVRRAllowed`
  to true for a name lifts a lower file's exclusion. The exact precedence between rules inside one file follows the
  NVIDIA README chapter on application profiles (builder verifies).
- **Mesa driconf.** Files, in the order Mesa reads them, later wins: `/usr/share/drirc.d/*.conf` (sorted), `/etc/drirc`,
  `$HOME/.drirc`. An `<application executable="…">` holding `<option name="adaptive_sync" value="false"/>` excludes
  that name; a later `true` for the same name lifts it. `executable_regexp`, `sha1` and device/driver scoping are not
  evaluated (the installed `adaptive_sync` entries use none of them). Parsed with `roxmltree`.
- **Standard parsers only** (revision 5.1, user decision): JSON and XML go through maintained crates (`serde_json`,
  `roxmltree`); yserver carries no parser of its own for either format.
- **Union.** Where both lists apply, a process is excluded if either excludes it. Both describe the same thing
  (applications whose frame rate must not drive the panel), and the server cannot know which GL vendor a client uses.
- **When it is read.** The files once, at startup, only in `--vrr all` mode. A missing file is an empty list; a file
  that fails to parse is skipped with a warning. The decision for a client is computed once, the first time one of its
  frames could enable VRR, and forgotten when the client disconnects. No `/proc` read happens per frame.
- **Licensing.** The files are read at run time as data. No content of the NVIDIA file is copied into yserver's code,
  tests or documents; tests use synthetic files.

### The cursor while VRR is on

Today a cursor change between frames goes out as its own `cursor-only` commit as soon as the CRTC has nothing in
flight; in held direct scanout this is explicit (`scanout.rs`, the branch that sends cursor-only commits while there is
no queued direct successor). With VRR on, each of those commits is a refresh (driver behaviour 2 and 3).

While VRR is on for a CRTC (revision 2.1: the kernel holds `VRR_ENABLED` = 1 **and** the live direct frame on that
output wants VRR; a value the kernel keeps after a rejected clear, or inherits at startup, never holds the cursor on
the composed desktop):

- A cursor **position** is recorded as pending, as today, but no standalone `cursor-only` commit is sent for that CRTC.
  The pending position rides the client's next direct commit, which already carries every pending position ("its
  grouped direct transaction gets every later position").
- A cursor **image** change (shape change, animation frame, show/hide) keeps today's path: a full cursor-state commit
  that can go out immediately. Direct frames carry position deltas, not image bindings, so batching images would need
  a new mechanism that moves sprite ownership into the direct transaction. **Known limitation of this step:** an
  animated cursor (Adwaita's watch changes about 60 times per second) costs one refresh per image while it is over a
  VRR output, so it can hold the panel near that rate. Motion is the common case and is covered; image batching is a
  later step.
- The first unsent position starts a **33 ms** deadline. The deadline is exposed through the existing
  `next_wakeup` (`backend_scanout_next_wakeup`), as the gamma deadline and the tearing `deferred_cursor_wake` already
  are; no new timer. A later change before the deadline does not move it.
- When the deadline expires with the change still unsent, the existing standalone `cursor-only` path sends it. A client
  frame that carries it first cancels the deadline. The tearing `deferred_cursor_wake` serves a different purpose (it
  caps software-cursor composition while tearing, with its own reset rules) and must keep its semantics if any
  representation is shared.
- With VRR off nothing changes.

Reuse: prefer one deadline shape shared with gamma's pending deadline over a third copy; the builder decides whether
the tearing `deferred_cursor_wake` can be the same field or must stay separate, and justifies the choice.

### Interaction with tearing

Tearing mode (async flips for a direct-eligible window that asked for `PresentOptionAsync`) and VRR are independent.
A window can be in both; async flips carry no CRTC properties, so `VRR_ENABLED` changes wait for the next vsync commit,
and the tearing cursor rules keep applying while tearing is on. Combined tearing+VRR behaviour is not tuned here.

### Logging

So a tester can confirm VRR on a real game:

- One info line per change: `kms: vrr on` / `kms: vrr off` with output, CRTC, owning window, and reason (the off reason
  uses the same reason strings the direct/tearing exits already log).
- In `--vrr all` mode, one info line at startup with the number of excluded names from each source, and one info line
  the first time a client is excluded: `kms: vrr excluded` with the process name and the source (environment, NVIDIA
  profiles or Mesa driconf).
- One info line per output at build: whether it is VRR-capable (`vrr_capable` value and `VRR_ENABLED` present).
- In the existing per-second present latency summary, while VRR is on for an output: the VRR state and the minimum,
  median and maximum interval between retired flips on that CRTC in that second. A game running below the panel's
  maximum must show intervals that follow its frame time instead of multiples of the fixed refresh period.
- The count of standalone cursor commits sent because the 33 ms deadline expired, in the same summary.

## Testing

Unit and backend tests, driven through the Backend paths (no direct calls into internals), with the end state checked:

- capable output, client-window direct owner: the first direct commit after entry carries `VRR_ENABLED` = 1; the commit
  after unflip carries 0. Mutation: never write the property.
- compositor image (`Cow`, `CowDescendant`) in direct scanout: no `VRR_ENABLED` in any commit. Mutation: accept any
  direct owner.
- output without `vrr_capable` (or with value 0), or without `VRR_ENABLED`: no commit carries the property, and the
  commit stream is identical to one from before this change. Mutation: write 0 unconditionally.
- async (tearing) flip while a change is pending: the flip carries no CRTC property; the next vsync commit does.
- VT release and shutdown with VRR on: a commit with `VRR_ENABLED` = 0 is accepted before the device is released.
- EINVAL on a commit carrying `VRR_ENABLED` = 1: latched off for that CRTC, the frame still goes out.
- without the flag: a capable output with a client window in direct scanout never receives `VRR_ENABLED` = 1. `--vrr`:
  a window without `_VARIABLE_REFRESH`, or with value 0, gets none; with 1 it gets VRR; deleting the property while in
  direct scanout clears VRR without waiting for a client frame. `--vrr all` on `nvidia-drm`: the property is ignored. Argument
  parsing: `--vrr`, `--vrr all`, `--vrr :5` (the display is not consumed), last flag wins.
- `--vrr all` exclusions, with synthetic list files: a listed process name gets no `VRR_ENABLED` = 1, an unlisted one
  does; a NVIDIA profile entry and a Mesa entry each exclude on their own; a higher-precedence file lifts a lower
  exclusion; `__GL_VRR_ALLOWED=0` excludes an unlisted process and `=1` allows a listed one; with a driver name other
  than `nvidia-drm`, `--vrr all` behaves as `--vrr` (a window without the property gets no VRR, one with it does, no
  list is loaded; mutation: drop the driver check); a client without a
  process id is not excluded; missing and malformed files leave the other source working. Mutation: skip the lookup.
  The lists do not affect `--vrr` mode.
- gamma pending while VRR is cleared: the clear goes out in that frame and the gamma LUT in the next. Gamma pending
  while VRR is enabled: the enable waits. A grouped request never carries both; an EINVAL on a VRR change never
  latches gamma, and the reverse. Mutation: append both classes to one request.
- cursor with VRR on: motion with a direct frame inside 33 ms rides that frame (0 `cursor-only` commits); motion with no
  frame gives exactly one `cursor-only` commit at the deadline, driven through `next_wakeup`. Mutation: send cursor-only
  immediately (today's behaviour). An image change still commits as today. With VRR off, cursor-only commits are
  unchanged.
- capable state follows the connector rescan: reconnecting a non-capable monitor on the same connector turns VRR off.
- inherited `VRR_ENABLED` = 1 at startup with no client window: the next vsync commit carries 0.
- the release-time clear failing: logged, and the VT release still completes.

The usual gate applies: workspace tests, the lavapipe ignored suite, `render_acceptance`, clippy, `tools/check-layout.sh`,
the FreeBSD cross-check, and the affected suites under CPU load.

Hardware (the user validates with a game and logs):

- MSI MAG 245F X24 (48–240 Hz) on DisplayPort, NVIDIA RTX 5060 Ti; and on AMD Raphael if a DP or FreeSync-capable HDMI
  path is available. The monitor's own refresh read-out or OSD, the yserver logs above, and the game's frame-time log.
- Check while entering and leaving VRR: no black frame, no modeset, nothing visible on the panel.
- Check that the desktop (no fullscreen client, and a compositing manager's image) never logs `vrr on`.
- Check cursor motion over a game capped below the panel maximum: the flip intervals keep following the game.
- Check the 33 ms motion wait and the animated-cursor limitation: Adwaita's watch cursor over a fullscreen video,
  recording what the OSD shows.
- Check `--vrr` with a Mesa client (mpv on AMD); with an NVIDIA client `--vrr` gives no `vrr on`, `--vrr all` does;
  without the flag no `vrr on`.
- Check `--vrr all` on NVIDIA with Firefox fullscreen video (excluded by the Mesa entry: `vrr excluded`, no `vrr on`)
  and with a game or mpv (not listed: `vrr on`). Check `--vrr all` on AMD: the startup line says it
  behaves as `--vrr`; mpv gets `vrr on`, Firefox does not.

## Open items for the builder

- Whether the off path needs an explicit primary-plane flip on amdgpu (see Transport).
- Whether gamma's pending-property and pending-deadline pieces can be shared as-is for `VRR_ENABLED` and the cursor
  deadline, or need a small generalization (reuse first; justify any new piece).
- Interaction with gamma's active-CRTC rule: with VRR the CRTC may flip slower than two fixed refresh periods, so a
  gamma change can go out standalone and trigger a refresh. Gamma changes are rare; note, do not tune, unless trivial.

## Revision history

- **Revision 5.1 (2026-10-26),** user decisions: Mesa becomes a documented requirement of `--vrr all` on NVIDIA, its
  list complements the NVIDIA one, and the list files are read with standard crates only (`serde_json`, `roxmltree`).

- **Revision 5 (2026-10-26),** user decision: on any driver other than `nvidia-drm`, `--vrr all` is `--vrr` (Mesa
  clients already apply Mesa's list); the lists are read only on `nvidia-drm`, both of them there.

- **Revision 4 (2026-10-25),** user decisions: the NVIDIA list and `__GL_VRR_ALLOWED` apply only on `nvidia-drm`, the
  Mesa list on every driver; NVIDIA driver floor 580, so only `GLVRRAllowed` is read; the lists must keep every
  compositor and shell off VRR, while unlisted browsers and players are accepted until reported.

- **Revision 3 (2026-10-25),** user decision after reviewing revision 2's cost: `--vrr all` no longer enables VRR for
  every application; it honours the client's environment and the application lists the NVIDIA driver and Mesa
  install (Application exclusions).

- **Revision 2.1 (2026-10-26),** maintainer review of PR #294: the 33 ms cursor hold requires a live direct frame that
  wants VRR, not only the kernel value; `_VARIABLE_REFRESH` is read as Xorg reads it (format 32, one item, nonzero);
  the standalone clear attaches only the live direct frame's framebuffer and has its own commit kind.

- **Split (2026-10-25),** user decision: PR #294 ships `--vrr` only. `--vrr all`, with exclusions taken from the
  NVIDIA application profiles and Mesa driconf, moves to its own branch and PR.
- **Revision 2 (2026-10-25),** from the maintainer's review of draft PR #294: off by default behind `--vrr`; honour
  `_VARIABLE_REFRESH` including live changes, with `--vrr all` for NVIDIA clients (user decision); a VRR clear takes
  priority over a pending gamma LUT and never waits for an unrelated frame; one feature class per request so EINVAL
  attribution stays separate; animated cursors documented as a limitation and added to the hardware runs.

## Provenance

Sources consulted, by static read only, for behaviour:

- yserver at `feat/gamma-atomic-lut` (`db07d171`): direct-scanout ownership (`ScanoutM0Target`, `state.rs`), direct
  Present submission (`present.rs`), cursor motion and cursor-only commits (`platform/cursor.rs`, `scanout.rs`), the
  tearing state and its cursor deferral (`state.rs`, `platform_hooks.rs`), gamma's pending property and deadline.
- Linux 7.1.9 (GPL): DRM core atomic UAPI and the variable-refresh documentation in `drm_connector.c`; amdgpu display
  manager (`amdgpu_dm.c`, `amdgpu_dm_crtc.c`) and the message of commit 3477c1b0972d.
- NVIDIA open kernel modules 615.78.08 (GPL/MIT dual): nvidia-drm connector and CRTC, NVKMS kapi, modeset, flip and
  VRR code.
- Xorg server modesetting driver (MIT): `driver.c` and `present.c`, for the `VariableRefresh` option and the
  `_VARIABLE_REFRESH` handling, including the live property change while flipped.
- Mesa main (MIT): `src/util/00-mesa-defaults.conf`, the `adaptive_sync` denylist; `src/util/xmlconfig.c` (file
  order, override by environment) and `src/util/u_process.c` (process name) for the exclusion lookup.
- The installed `/usr/share/nvidia/nvidia-application-profiles-615.78.08-rc` (proprietary data, read for its format
  and for which keys mark VRR off; nothing copied) and `/usr/share/drirc.d/00-mesa-defaults.conf`.
- NVIDIA driver 615.78.08 documentation (README section "G-SYNC", application profile file and key documentation):
  `__GL_VRR_ALLOWED`, `GLVRRAllowed`, the "No VRR/OSD" profile; a string search of `libGLX_nvidia.so.615.78.08`.
- Mesa main (MIT): Vulkan WSI X11 (`wsi_common_x11.c`), for how `_VARIABLE_REFRESH` is set or deleted.
- wlroots (MIT): output cursor and frame scheduling, which has no VRR-specific cursor rule.
- KWin 6.7.5 (GPL-2.0): only its observable behaviour is used: a fullscreen window enables adaptive sync on its output,
  and while a window controls the refresh rate, repaints caused by anything else (the cursor included) are delayed by
  up to 1000/30 ms and go out with that window's next frame. No KWin code, expressions or structure are used; the
  extracted sources were removed after reading.

This document specifies behaviour in original prose. It contains no copyleft code excerpts; property names, constants
and file names identify public interfaces.
