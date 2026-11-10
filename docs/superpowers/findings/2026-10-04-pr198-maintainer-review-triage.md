# PR #198 maintainer review: triage (2026-10-04)

Review by joske on PR #198 (dynamic XI device registry), checked against
`xorg-server-21.1.24`. The branch is `feat/xi-dynamic-registry-rebased` at
`f0e9aaeb`, rebased onto `joske/master`. Implementation findings map to the
fixing commits below. The supervised hardware pass remains open.

## Process

| # | Item | Verdict | Fix / evidence |
|---|---|---|---|
| P1 | Rebase instead of merging master | fixed | Branch history is rebased with no merge commits; the source fixes are listed below. There is no standalone rebase commit. |
| P2 | `border_press_reports_negative_content_coords_on_the_wire` fails | fixed | `018834ad` corrects the test to release between repeated XTEST presses, matching Xorg's held-button guard (`Xi/exevents.c:948`). |

## Differences from Xorg

| # | Item | Verdict | Fixing commit(s) and Xorg reference |
|---|---|---|---|
| X1 | Disabled/new slaves float until enabled, including across VT | fixed | `8281bb1a`, `8fac38e3`, `2a735ffa`; Xorg clears the master on disable and reattaches on enable (`dix/devices.c:388–393,539`). |
| X2 | `Device Enabled` exists on every device | fixed | `1106f4f2`, `41df0716`; Xorg creates the non-deletable property on each device (`dix/devices.c:308–311`) and updates it through `DeviceSetProperty` (`:149–167`). |
| X3 | XI2 explicit grabs float XTEST 4/5 | fixed | `caee9cda`; Xorg detaches a grabbed slave (`dix/events.c:1621–1624`). |
| X4 | XIQueryDevice button state and master class `sourceid` | fixed | `20aae912`, `78aca3ee`; Xorg reports held buttons (`Xi/xiquerydevice.c:282`) and stores class provenance when copying (`Xi/exevents.c:592,630`). |
| X5 | Masters/XTEST have no XI1 type; XTEST pointer shape and button 10 | fixed | `a309ca56`, `f88073d0`; Xorg initializes XTEST with `CorePointerProc` (`Xext/xtest.c:621–629`, `dix/devices.c:657–690`). |
| X6 | Unknown XTestFakeInput device returns `BadDevice` | fixed | `f88073d0`; Xorg validates `deviceid & 0177` and reports that ID (`Xext/xtest.c:182–186`). |
| X7 | Hierarchy/presence delivery is per selecting window and facet | fixed | `8fac38e3`, `a8e65f75`; Xorg sends hierarchy through `SendEventToAllWindows` (`Xi/xichangehierarchy.c:119`) and walks selected windows (`Xi/exevents.c:3279–3312`). |
| X8 | Owner-events smooth-scroll fallback and `LockNoUnlock` behavior | fixed | `f71ffd11`, `45c3f6a6`; Xorg decides fallback for each event (`dix/events.c:4431–4464`) and honors lock-action flags (`xkb/xkbActions.c:372–395`). |

## Testing and documentation

| # | Item | Verdict | Fix / remaining acceptance |
|---|---|---|---|
| T1 | Touchpad laptop and a grab-heavy desktop | open (hardware) | No touchpad laptop on our side; the maintainer offered to cover it. The grab paths it exercises changed in `f71ffd11` and `45c3f6a6`. |
| T2 | VT round trip while a key is held | answered | `d4239b60`, `2a735ffa`; held physical state is released once and client-disabled state survives VT. The supervised VT run remains part of hardware acceptance. |
| T3 | XI property write at VT release | fixed | `4df57890`, `f0e9aaeb`; a source-off write fails `BadMatch`, and submitted writes are answered before VT yield. |
| S1 | `xinput set-prop 4 ...` targets the virtual XTEST pointer | fixed | `91e4b57d` and the docs commit after `f0e9aaeb` document using the physical device's own ID and the design's per-device acceleration loop. |
| S2 | Fold the 2026-09-30 design-review reports | fixed | The docs commit after `f0e9aaeb` folds them into the branch review as one dispositions section and fixes the links. |

## Reviews of the addendum and plans

| Review verdict | Finding and resolution |
|---|---|
| Addendum review: 2 blocking, 1 major, 0 minor | **B-1:** hierarchy/presence events have no window field; E1 now sends one copy to each selecting window (`a8e65f75`; Xorg `Xi/xichangehierarchy.c:119`, `Xi/exevents.c:3279–3312`). **B-2:** master class `sourceid` is stored class provenance, not current `lastSlave`; C2 and `78aca3ee` preserve it (`Xi/exevents.c:592,630`; `Xi/xiquerydevice.c:278,326`). **M-1:** the requested touchpad/desktop-grab scenario was added to supervised hardware acceptance; execution remains pending. |
| Plan 1 review: 1 blocking, 1 major, 0 minor | **B-1:** drain held input before committing disabled state (`8281bb1a`; Xorg `dix/devices.c:466–468,504–506`). **M-1:** capture the attached `XIDeviceDisabled` descriptor before floating the facet (`8fac38e3`; Xorg `dix/devices.c:528–539`). |
| Plan 2 review: 2 blocking, 0 major, 0 minor | **B-1:** the addendum's C3 said absolute valuators while the plan said relative; Xorg is relative, and the addendum was corrected (`87c52356`), matching `CorePointerProc` (`a309ca56`; `dix/devices.c:657–690,1644–1646`). **B-2:** same-device XI2 grab replacement preserves the floating attachment and position (`caee9cda`; `dix/events.c:1463–1464,1621–1624,1649–1650`). |
| Plan 3 review: 0 blocking, 0 major, 0 minor; coverage incomplete | No findings were demonstrated. Its VT-release completion handoff question was resolved by `f0e9aaeb`, which consumes and answers submitted configuration writes before yielding the VT; VT device transitions follow Xorg's `hw/xfree86/common/xf86Events.c:302–320` path. |

The final supervised hardware pass remains open.
