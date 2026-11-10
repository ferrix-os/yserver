## Verdict

**1 blocking, 0 major, 0 minor**

Coverage: COMPLETE FOR DECLARED SCOPE

This is a design-review result. It does not establish compilation, passing tests, or implementation approval.

## Incorporation audit

| Prior findings | Disposition |
|---|---|
| None | First review; check 1 skipped. |

## Findings

### Blocking

**B-1 — Task 2 requires a button-map rejection that Xorg does not perform**

[Plan lines 53–55](/home/ariel_santangelo/Projects/yserver-xi-dynamic-registry/docs/superpowers/plans/2026-10-05-xi-maintainer-followups.md:53) require `SetDeviceButtonMapping` to reject an 11-entry map on device 4 “as Xorg does.” That contradicts the instruction to follow Xorg’s errors in plan lines 49–50 and the authoritative [wire-behavior contract, spec lines 9–12](/home/ariel_santangelo/Projects/yserver-xi-dynamic-registry/docs/superpowers/specs/2026-10-04-xi-registry-maintainer-review-addendum.md:9). The [C3 amendment, spec lines 242–244](/home/ariel_santangelo/Projects/yserver-xi-dynamic-registry/docs/superpowers/specs/2026-10-04-xi-registry-maintainer-review-addendum.md:242), does not establish the advertised button count as a setter length ceiling.

Verified with `git show xorg-server-21.1.24:<path>`:

- `Xi/setbmap.c:95–123` checks the request’s wire length, then calls `ApplyPointerMapping`.
- `dix/inpututils.c:43–68` checks device/access validity and whether a changed button is down; it does **not** compare `len` with `numButtons`.
- `dix/inpututils.c:72–80,114–124` copies the supplied map and returns success after those checks.

**Concrete failure:** with no buttons held, a client submits a correctly sized 11-entry identity map to the 10-button XTEST pointer. Xorg returns `MappingSuccess`; the plan requires rejection. The proposed test would enforce the incompatible behavior.

The checkout already contains an artificial count ceiling in [process_request.rs lines 22596–22613](/home/ariel_santangelo/Projects/yserver-xi-dynamic-registry/crates/yserver-core/src/core_loop/process_request.rs:22596). Raising that ceiling from seven to ten would preserve the defect.

**Smallest correction:** replace the existing 11-entry rejection expectation with Xorg’s success expectation and explicitly remove the advertised-button-count ceiling from setter semantics. Keep the advertised counts for `GetDeviceButtonMapping` and `QueryDeviceState`.

### Major

None.

### Minor

None.

## Coverage and implementation checks

1. **Incorporation:** skipped because no prior review exists.

2. **Architecture and cross-task contracts:** reviewed all four maintainer tasks. Verified the existing class-shape producer, scroll conversion and emulation paths, configuration submission/result routing, and session disable/resume consumers. The plan identifies the generating slave as the scroll-class owner and separates timeout reconciliation from the completed client request. No additional reportable defect was demonstrated under the restricted categories.

3. **Safety, ownership and failure semantics:** inspected the input thread’s configuration-before-pause ordering, the core’s bounded barrier wait, token/source result matching, stale-result discard, and the two-reason enabled-state model. The cancellation and late-application requirements match [amendment G, lines 250–254](/home/ariel_santangelo/Projects/yserver-xi-dynamic-registry/docs/superpowers/specs/2026-10-04-xi-registry-maintainer-review-addendum.md:250). Task 4 matches [amendment A1, lines 255–261](/home/ariel_santangelo/Projects/yserver-xi-dynamic-registry/docs/superpowers/specs/2026-10-04-xi-registry-maintainer-review-addendum.md:255), including its deliberate departure from Xorg’s suspended-device inconsistency. No further in-scope failure sequence was demonstrated.

4. **Specification and verification:** compared the task rules with the October 5 amendments and relevant Xorg handlers. B-1 identifies an incorrect acceptance oracle. Formatting, tests, mutation checks, and `cargo clippy --all-targets -- -D warnings` are assigned through the inherited [task rules, lines 28–35](/home/ariel_santangelo/Projects/yserver-xi-dynamic-registry/docs/superpowers/plans/2026-10-04-xi-enabled-state-and-floating.md:28). Actual compilation, test execution, and platform buildability remain implementation checks.

**Reading budget:** 24/24 bounded excerpts, conservatively including the inherited task-rules excerpt. Locator searches were bounded separately. Investigation stopped at that limit.

**Limitations:** no broad registry, reset/removal, dependency, ABI, or portability audit was performed. Earlier requirements outside the four amendments, Non-goals, and direct touch remain unassessed. That ground is not claimed sound. No builds, tests, installs, compilation experiments, or additional reviewers were invoked.