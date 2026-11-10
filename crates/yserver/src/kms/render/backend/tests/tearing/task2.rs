use super::*;

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: route the whole motion through the tearing path and suppress foreign CRTC movement.
fn tearing_on_one_output_keeps_hw_cursor_motion_on_the_other() {
    use yserver_core::backend::Backend;

    let Some(mut fixture) = super::cursor_sprites::support::AtomicPathFixture::with_output_count(
        super::cursor_sprites::support::PathKind::Physical,
        2,
    )
    .expect("dual-output hardware cursor fixture") else {
        eprintln!(
            "SKIP tearing_on_one_output_keeps_hw_cursor_motion_on_the_other: Vulkan fixture unavailable"
        );
        return;
    };
    for device in &mut fixture.scene.events.backend.platform.devices {
        device.async_page_flip = true;
    }
    let mut delivered = std::collections::HashSet::new();
    let cursor = super::cursor_sprites::support::create_argb_cursor(
        &mut fixture.scene.events.backend,
        (32, 32),
        [0xff, 0xff, 0xff, 0xff],
        (0, 0),
    );
    let hw = super::cursor_sprites::support::establish_hardware_cursor_on_output(
        &mut fixture,
        &mut delivered,
        cursor,
        0,
        (100, 100),
    );
    let owner = fixture.scene.install_direct_output_present(
        1,
        15_901,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("hide the cursor before Output(1) enters tearing");
    for output_idx in 0..2 {
        super::cursor_sprites::support::drain_output_events(
            &mut fixture.scene.events,
            &mut delivered,
            output_idx,
        );
    }
    super::cursor_sprites::support::drain_unretired_output_events(&mut fixture, &mut delivered, 1);
    let owner_record = enter_output_owner_through_backend(&mut fixture.scene, &owner, 1);
    assert_eq!(owner_record.kind, AtomicCommitKind::Async);
    assert_eq!(fixture.scene.events.backend.tearing_scope_outputs(), [1]);

    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, false);
    let restore_before = fixture.scene.events.recorders[&hw.device_fd]
        .records()
        .len();
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("restore Output(0)'s hardware cursor without handing off Output(1)");
    for output_idx in 0..2 {
        retire_recorded_output_commits(&mut fixture.scene, output_idx, restore_before, 50);
    }
    assert!(
        fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_visible_for_output(0),
        "Output(0) keeps its hardware cursor while Output(1) tears"
    );

    let before = fixture.scene.events.recorders[&hw.device_fd]
        .records()
        .len();
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        790,
        100,
    );
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("service cursor motion crossing into the tearing output");
    let records: Vec<_> = fixture.scene.events.recorders[&hw.device_fd]
        .records()
        .into_iter()
        .skip(before)
        .collect();
    let output0_crtc = fixture.scene.events.backend.platform.outputs[0].output.crtc;
    let cursor_properties = super::cursor_sprites::support::cursor_properties_for_output(
        &fixture.scene.events.backend,
        0,
    );
    let cursor_move = records
        .iter()
        .find(|record| {
            record.events.iter().any(|event| event.crtc == output0_crtc)
                && super::commit_identity::support::accepted_event_id(record).is_some()
                && super::cursor_sprites::support::record_property(record, cursor_properties.crtc_x)
                    == Some(790)
        })
        .expect("the foreign hardware cursor move is submitted on Output(0)");
    assert_eq!(
        super::cursor_sprites::support::record_property(cursor_move, cursor_properties.crtc_x),
        Some(790),
        "Output(0)'s CRTC_X follows the motion whose footprint also intersects Output(1)"
    );
    assert!(
        cursor_move
            .events
            .iter()
            .all(|event| event.crtc == output0_crtc)
    );
    assert_eq!(fixture.scene.events.backend.tearing_scope_outputs(), [1]);
    fixture.scene.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: fresh-unflip eligibility reads the global tearing flag.
fn tearing_on_one_output_keeps_fresh_unflip_on_the_other() {
    use yserver_core::backend::Backend;

    let Some(mut fixture) = super::commit_identity::recording_scene::RecordingSceneFixture::
        with_output_admission_topology()
        .expect("dual-output mixed-refresh fixture")
    else {
        eprintln!(
            "SKIP tearing_on_one_output_keeps_fresh_unflip_on_the_other: Vulkan fixture unavailable"
        );
        return;
    };
    for device in &mut fixture.events.backend.platform.devices {
        device.async_page_flip = true;
    }
    Backend::set_cursor_hidden(&mut fixture.events.backend, true);
    let tearing_owner = fixture.install_direct_output_present(
        1,
        15_902,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    let tearing_record = enter_output_owner_through_backend(&mut fixture, &tearing_owner, 1);
    assert_eq!(tearing_record.kind, AtomicCommitKind::Async);
    assert_eq!(fixture.events.backend.tearing_scope_outputs(), [1]);

    let other = fixture.install_direct_output_present(0, 15_903, 0);
    let other_record = enter_output_owner_through_backend(&mut fixture, &other, 0);
    assert_eq!(other_record.kind, AtomicCommitKind::Scene);
    let mut delivered = std::collections::HashSet::new();
    super::cursor_sprites::support::retire_pending_output(
        &mut fixture.events,
        &mut delivered,
        0,
        54,
    );
    let mut invalid = other.candidate;
    invalid.present_id += 1;
    invalid.valid_region_xid = 0xdead;
    let mut invalid_event = other.event.clone();
    invalid_event.present_id = invalid.present_id;
    invalid_event.serial = invalid_event.serial.wrapping_add(1);
    Backend::note_present_scanout_candidate(&mut fixture.events.backend, invalid);
    assert!(
        !Backend::try_present_direct(&mut fixture.events.backend, invalid, invalid_event)
            .expect("ineligible Output(0) successor requests its unflip"),
        "the invalid region is not submitted direct"
    );
    assert!(fixture.events.backend.pstate.direct.outputs[0].unflip_requested);

    let fd = fixture.kms_fd(0);
    let before = fixture.events.recorders[&fd].records().len();
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("submit Output(0)'s fresh composed unflip while Output(1) tears");
    let output0_crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let replacement = fixture.events.recorders[&fd]
        .records()
        .into_iter()
        .skip(before)
        .find(|record| {
            record.kind != AtomicCommitKind::CursorOnly
                && record.events.iter().any(|event| event.crtc == output0_crtc)
                && super::commit_identity::support::accepted_event_id(record).is_some()
        })
        .expect("Output(0)'s fresh replacement has an accepted commit identity");
    assert_eq!(replacement.events.len(), 1);
    assert_eq!(replacement.events[0].crtc, output0_crtc);
    assert!(
        fixture.events.backend.pstate.direct.outputs[0].fresh_composed_unflip,
        "the replacement uses a fresh scene framebuffer rather than the retained fallback"
    );
    assert!(
        fixture.events.backend.pstate.direct.outputs[1]
            .current
            .is_some()
    );
    assert_eq!(fixture.events.backend.tearing_scope_outputs(), [1]);
    super::cursor_sprites::support::retire_pending_output(
        &mut fixture.events,
        &mut delivered,
        0,
        55,
    );
    fixture.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: use every output's cursor transition; preserve ordinary probation for any tearing owner.
fn tearing_probation_reads_only_the_candidate_outputs() {
    use yserver_core::backend::Backend;

    let Some(mut fixture) = super::cursor_sprites::support::AtomicPathFixture::with_output_count(
        super::cursor_sprites::support::PathKind::Physical,
        2,
    )
    .expect("dual-output cursor transition fixture") else {
        eprintln!(
            "SKIP tearing_probation_reads_only_the_candidate_outputs: Vulkan fixture unavailable"
        );
        return;
    };
    for device in &mut fixture.scene.events.backend.platform.devices {
        device.async_page_flip = true;
    }
    let cursor = super::cursor_sprites::support::create_argb_cursor(
        &mut fixture.scene.events.backend,
        (24, 24),
        [0xff, 0xff, 0xff, 0xff],
        (0, 0),
    );
    super::cursor_sprites::support::define_root_cursor(&mut fixture.scene.events.backend, cursor);
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        900,
        100,
    );
    let root = fixture.scene.events.backend.core.window_id;
    Backend::clear_area(
        &mut fixture.scene.events.backend,
        None,
        root,
        0,
        None,
        800,
        0,
        800,
        600,
        (0, 0),
    )
    .expect("damage the cursor transition on Output(1)");
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("submit Output(1)'s cursor transition through Backend");
    let mut delivered = std::collections::HashSet::new();
    super::cursor_sprites::support::drain_output_events(
        &mut fixture.scene.events,
        &mut delivered,
        0,
    );
    assert!(
        fixture
            .scene
            .events
            .backend
            .scene
            .cursor_transition_pending(&[1])
    );
    assert!(!fixture.scene.events.backend.scene.has_pending_page_flip(0));
    assert!(fixture.scene.events.backend.scene.has_pending_page_flip(1));

    let candidate = fixture.scene.install_direct_output_present(
        0,
        15_904,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    fixture
        .scene
        .events
        .backend
        .for_tests_set_scanout_m1_prober(super::present_scanout::record_scanout_m1_probe);
    let probation = usize::from(crate::kms::render::backend::TEARING_ENTRY_PROBATION);
    for attempt in 0..probation {
        let accepted = output_probation_present(
            &mut fixture.scene,
            &mut delivered,
            &candidate,
            0,
            15_920 + u64::try_from(attempt).expect("Present id fits"),
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            60 + u32::try_from(attempt).expect("frame fits"),
        );
        assert_eq!(accepted, attempt + 1 == probation);
    }
    assert_eq!(fixture.scene.events.backend.tearing_scope_outputs(), [0]);
    assert!(
        fixture
            .scene
            .events
            .backend
            .scene
            .cursor_transition_pending(&[1])
    );
    super::cursor_sprites::support::retire_pending_output(
        &mut fixture.scene.events,
        &mut delivered,
        1,
        70,
    );
    fixture.scene.finish();

    let Some(mut fixture) = super::cursor_sprites::support::AtomicPathFixture::with_output_count(
        super::cursor_sprites::support::PathKind::Physical,
        2,
    )
    .expect("second dual-output cursor transition fixture") else {
        eprintln!(
            "SKIP tearing_probation_reads_only_the_candidate_outputs: Vulkan fixture unavailable for ordinary-probation case"
        );
        return;
    };
    for device in &mut fixture.scene.events.backend.platform.devices {
        device.async_page_flip = true;
    }
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, true);
    let owner = fixture.scene.install_direct_output_present(
        1,
        15_905,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    let _owner_record = enter_output_owner_through_backend(&mut fixture.scene, &owner, 1);
    let candidate = fixture.scene.install_direct_output_present(
        0,
        15_906,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    let cursor = super::cursor_sprites::support::create_argb_cursor(
        &mut fixture.scene.events.backend,
        (24, 24),
        [0xff, 0xff, 0xff, 0xff],
        (0, 0),
    );
    super::cursor_sprites::support::define_root_cursor(&mut fixture.scene.events.backend, cursor);
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, false);
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        100,
        100,
    );
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("start Output(0)'s cursor transition through Backend");
    let output0_mode = fixture
        .scene
        .events
        .backend
        .scene
        .cursor_mode_for_outputs(&[0]);
    assert!(
        fixture
            .scene
            .events
            .backend
            .scene
            .cursor_transition_pending(&[0])
            && fixture.scene.events.backend.scene.has_pending_page_flip(0)
            && matches!(
                output0_mode,
                crate::kms::render::scene::CursorPlaneMode::Mixed
                    | crate::kms::render::scene::CursorPlaneMode::Sw
            ),
        "Output(0) has its own in-flight software/hardware cursor transition: {output0_mode:?}"
    );
    fixture.scene.events.backend.pstate.direct.outputs[0].eligible_streak = 3;
    fixture
        .scene
        .events
        .backend
        .for_tests_set_scanout_m1_prober(super::present_scanout::record_scanout_m1_probe);
    let mut blocked = candidate.candidate;
    blocked.present_id += 1;
    let mut blocked_event = candidate.event.clone();
    blocked_event.present_id = blocked.present_id;
    blocked_event.serial = blocked_event.serial.wrapping_add(1);
    Backend::note_present_scanout_candidate(&mut fixture.scene.events.backend, blocked);
    assert!(
        !Backend::try_present_direct(&mut fixture.scene.events.backend, blocked, blocked_event)
            .expect("cursor-ineligible Output(0) Present falls back through Backend")
    );
    assert_eq!(
        fixture.scene.events.backend.pstate.direct.outputs[0].eligible_streak,
        0
    );
    assert_eq!(fixture.scene.events.backend.tearing_scope_outputs(), [1]);
    assert!(
        fixture.scene.events.backend.pstate.direct.outputs[1]
            .current
            .is_some()
    );
    fixture.scene.finish();
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: use max refresh across all outputs; advance on any composed submit;
// apply one global deadline; omit the deferred owner wake for foreign motion.
fn tearing_cursor_pacing_is_scoped_to_the_owner_outputs() {
    use yserver_core::backend::Backend;

    let Some(mut fixture) = super::commit_identity::recording_scene::RecordingSceneFixture::
        with_output_admission_topology()
        .expect("dual-output mixed-refresh pacing fixture")
    else {
        eprintln!(
            "SKIP tearing_cursor_pacing_is_scoped_to_the_owner_outputs: Vulkan fixture unavailable"
        );
        return;
    };
    for device in &mut fixture.events.backend.platform.devices {
        device.async_page_flip = true;
    }
    Backend::set_cursor_hidden(&mut fixture.events.backend, true);
    let owner = fixture.install_direct_output_present(
        1,
        15_907,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
    );
    let baseline_start = fixture.events.recorders[&fixture.kms_fd(0)].records().len();
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("settle the two-output scene before direct admission");
    retire_recorded_output_commits(&mut fixture, 0, baseline_start, 5);
    retire_recorded_output_commits(&mut fixture, 1, baseline_start, 10);
    assert!(!fixture.events.backend.scene.owes_repaint());
    assert!(
        !fixture
            .events
            .backend
            .store
            .has_pending_presentation_damage()
    );
    let owner_record = enter_output_owner_through_backend(&mut fixture, &owner, 1);
    assert_eq!(owner_record.kind, AtomicCommitKind::Async);
    fixture.events.backend.platform.outputs[0]
        .output
        .picked
        .vrefresh = 240;
    fixture.events.backend.platform.outputs[1]
        .output
        .picked
        .vrefresh = 75;
    assert_eq!(fixture.events.backend.tearing_scope_outputs(), [1]);
    let cursor = super::cursor_sprites::support::create_argb_cursor(
        &mut fixture.events.backend,
        (300, 4),
        [0x18, 0x29, 0x3a, 0xff],
        (1, 2),
    );
    super::cursor_sprites::support::define_root_cursor(&mut fixture.events.backend, cursor);
    let fd = fixture.kms_fd(0);
    let owner_crtc = fixture.events.backend.platform.outputs[1].output.crtc;
    Backend::warp_pointer_root(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        2000,
        120,
    );
    Backend::set_cursor_hidden(&mut fixture.events.backend, false);
    let mut owner_scene_accepted = false;
    for pass in 0..4_u32 {
        let pass_start = fixture.events.recorders[&fd].records().len();
        Backend::maybe_composite(&mut fixture.events.backend)
            .expect("submit cursor and owner scene work through Backend");
        owner_scene_accepted |= fixture.events.recorders[&fd]
            .records()
            .into_iter()
            .skip(pass_start)
            .any(|record| {
                matches!(
                    record.kind,
                    AtomicCommitKind::Scene | AtomicCommitKind::SceneWithCursor
                ) && record.events.iter().any(|event| event.crtc == owner_crtc)
                    && super::commit_identity::support::accepted_event_id(&record).is_some()
            });
        retire_recorded_output_commits(&mut fixture, 0, pass_start, 80 + pass.saturating_mul(4));
        retire_recorded_output_commits(&mut fixture, 1, pass_start, 100 + pass.saturating_mul(4));
    }
    assert!(
        owner_scene_accepted,
        "the cursor handoff submits an accepted scene frame for the tearing owner"
    );
    assert_eq!(
        fixture.events.backend.scene.cursor_mode_for_outputs(&[1]),
        crate::kms::render::scene::CursorPlaneMode::Sw,
        "the oversized cursor is software-composed on the tearing owner"
    );

    let expected_interval = std::time::Duration::from_nanos(1_000_000_000 / 75);
    // Mutation: foreign software-cursor motion must retain the owner's pending
    // cursor cleanup after a foreign scene tick consumes the shared dirty bit.
    use std::{
        rc::Rc,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::Instant,
    };
    assert_eq!(
        fixture.events.backend.pstate.tearing.deferred_cursor_wake,
        None
    );
    let clock_base = Instant::now();
    let elapsed_ns = Arc::new(AtomicU64::new(0));
    let clock_elapsed_ns = Arc::clone(&elapsed_ns);
    fixture
        .events
        .backend
        .for_tests_set_monotonic_clock(Rc::new(move || {
            clock_base + std::time::Duration::from_nanos(clock_elapsed_ns.load(Ordering::SeqCst))
        }));
    fixture.events.backend.pstate.tearing.last_cursor_compose = Some(clock_base);
    let cleanup_deadline = clock_base + expected_interval;
    let cleanup_foreign_x = fixture.events.backend.platform.outputs[0].x
        + i32::from(fixture.events.backend.platform.outputs[0].width)
        - 300;
    Backend::warp_pointer_root(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        cleanup_foreign_x,
        120,
    );
    assert_eq!(
        fixture.events.backend.pstate.tearing.deferred_cursor_wake,
        Some(cleanup_deadline),
        "foreign cursor motion retains the owner's paced cleanup deadline"
    );
    assert!(fixture.events.backend.scene.cursor_only_wake_pending());
    let cleanup_foreign_crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let cleanup_foreign_start = fixture.events.recorders[&fd].records().len();
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("compose the software cursor on foreign Output(0) during owner pacing");
    let cleanup_foreign = fixture.events.recorders[&fd]
        .records()
        .into_iter()
        .skip(cleanup_foreign_start)
        .find(|record| {
            matches!(
                record.kind,
                AtomicCommitKind::Scene | AtomicCommitKind::SceneWithCursor
            ) && record
                .events
                .iter()
                .any(|event| event.crtc == cleanup_foreign_crtc)
                && super::commit_identity::support::accepted_event_id(record).is_some()
        })
        .expect("foreign software cursor still composes before the owner deadline");
    assert!(
        cleanup_foreign
            .events
            .iter()
            .all(|event| event.crtc == cleanup_foreign_crtc)
    );
    retire_recorded_output_commits(&mut fixture, 0, cleanup_foreign_start, 70);
    assert!(
        fixture
            .events
            .backend
            .pstate
            .tearing
            .deferred_cursor_wake
            .is_some()
    );
    assert!(!fixture.events.backend.scene.cursor_only_wake_pending());

    elapsed_ns.store(
        u64::try_from(expected_interval.as_nanos()).expect("pacing interval fits u64 nanoseconds"),
        Ordering::SeqCst,
    );
    let owner_cleanup_start = fixture.events.recorders[&fd].records().len();
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("service owner cursor cleanup at the deadline without further pointer input");
    retire_recorded_output_commits(&mut fixture, 0, owner_cleanup_start, 80);
    retire_recorded_output_commits(&mut fixture, 1, owner_cleanup_start, 81);
    let owner_cleanup = fixture.events.recorders[&fd]
        .records()
        .into_iter()
        .skip(owner_cleanup_start)
        .find(|record| {
            matches!(
                record.kind,
                AtomicCommitKind::Scene
                    | AtomicCommitKind::SceneWithCursor
                    | AtomicCommitKind::Async
            ) && record.events.iter().any(|event| event.crtc == owner_crtc)
                && super::commit_identity::support::accepted_event_id(record).is_some()
        })
        .expect("the deadline re-composes owner Output(1) after foreign dirty state was consumed");
    assert!(
        owner_cleanup
            .events
            .iter()
            .all(|event| event.crtc == owner_crtc)
    );
    assert_eq!(
        fixture.events.backend.pstate.tearing.deferred_cursor_wake,
        None
    );
    retire_recorded_output_commits(&mut fixture, 1, owner_cleanup_start, 75);
    assert!(
        !fixture
            .events
            .backend
            .scene
            .output_has_pending_compose(1, &fixture.events.backend.store),
        "the accepted owner frame retires the cursor cleanup damage"
    );

    let restore_owner_start = fixture.events.recorders[&fd].records().len();
    Backend::warp_pointer_root(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        2000,
        120,
    );
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("restore the software cursor on owner Output(1) for the pacing checks");
    retire_recorded_output_commits(&mut fixture, 0, restore_owner_start, 90);
    retire_recorded_output_commits(&mut fixture, 1, restore_owner_start, 91);
    assert_eq!(
        fixture.events.backend.scene.cursor_mode_for_outputs(&[1]),
        crate::kms::render::scene::CursorPlaneMode::Sw,
        "the subsequent owner pacing check starts with its software cursor composed"
    );
    fixture
        .events
        .backend
        .for_tests_set_monotonic_clock(Rc::new(Instant::now));

    let last_compose = std::time::Instant::now() + std::time::Duration::from_secs(30);
    fixture.events.backend.pstate.tearing.last_cursor_compose = Some(last_compose);
    Backend::warp_pointer_root(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        2001,
        120,
    );
    let owner_deadline = last_compose + expected_interval;
    assert_eq!(
        fixture.events.backend.pstate.tearing.deferred_cursor_wake,
        Some(owner_deadline),
        "Output(1)'s 75 Hz refresh sets the tearing cursor interval (mode={:?}, hidden={}, unflip_reason={:?}, owner={:?})",
        fixture.events.backend.scene.cursor_mode_for_outputs(&[1]),
        fixture.events.backend.effective_cursor_hidden(),
        fixture.events.backend.pstate.direct.outputs[1].unflip_reason,
        fixture.events.backend.tearing_scope_outputs(),
    );
    let foreign_timestamp = fixture.events.backend.pstate.tearing.last_cursor_compose;
    let root = fixture.events.backend.core.window_id;
    Backend::clear_area(
        &mut fixture.events.backend,
        None,
        root,
        0,
        None,
        0,
        0,
        1920,
        1080,
        (0, 0),
    )
    .expect("damage only the composed foreign output");
    let before = fixture.events.recorders[&fd].records().len();
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("compose foreign Output(0) despite Output(1)'s cursor interval");
    let output0_crtc = fixture.events.backend.platform.outputs[0].output.crtc;
    let foreign_frame = fixture.events.recorders[&fd]
        .records()
        .into_iter()
        .skip(before)
        .find(|record| {
            matches!(
                record.kind,
                AtomicCommitKind::Scene | AtomicCommitKind::SceneWithCursor
            ) && record.events.iter().any(|event| event.crtc == output0_crtc)
                && super::commit_identity::support::accepted_event_id(record).is_some()
        })
        .expect("foreign output's composed frame is accepted");
    assert!(
        foreign_frame
            .events
            .iter()
            .all(|event| event.crtc == output0_crtc)
    );
    assert_eq!(
        fixture.events.backend.pstate.tearing.last_cursor_compose, foreign_timestamp,
        "a foreign output submission does not advance the tearing cursor timestamp"
    );
    retire_recorded_output_commits(&mut fixture, 0, before, 120);

    let later_last_compose = std::time::Instant::now() + std::time::Duration::from_secs(30);
    fixture.events.backend.pstate.tearing.last_cursor_compose = Some(later_last_compose);
    Backend::warp_pointer_root(
        &mut fixture.events.backend,
        &mut fixture.events.state,
        120,
        120,
    );
    assert!(fixture.events.backend.scene.cursor_only_wake_pending());
    assert!(
        Backend::next_wakeup(&fixture.events.backend)
            .is_some_and(|deadline| deadline <= std::time::Instant::now()),
        "foreign software-cursor work keeps its immediate scene wake"
    );
    let before = fixture.events.recorders[&fd].records().len();
    Backend::maybe_composite(&mut fixture.events.backend)
        .expect("compose the software cursor on foreign Output(0)");
    let foreign_cursor_frame = fixture.events.recorders[&fd]
        .records()
        .into_iter()
        .skip(before)
        .find(|record| {
            matches!(
                record.kind,
                AtomicCommitKind::Scene | AtomicCommitKind::SceneWithCursor
            ) && record.events.iter().any(|event| event.crtc == output0_crtc)
                && super::commit_identity::support::accepted_event_id(record).is_some()
        })
        .expect("foreign software cursor frame is not deferred by Output(1)'s deadline");
    assert!(
        foreign_cursor_frame
            .events
            .iter()
            .all(|event| event.crtc == output0_crtc)
    );
    assert_eq!(
        fixture.events.backend.pstate.tearing.last_cursor_compose,
        Some(later_last_compose),
        "foreign software-cursor composition leaves the owner's timestamp alone"
    );
    retire_recorded_output_commits(&mut fixture, 0, before, 140);
    assert_eq!(
        fixture.events.backend.scene.cursor_mode_for_outputs(&[0]),
        crate::kms::render::scene::CursorPlaneMode::Sw,
        "the foreign cursor is composed in Output(0)'s scene"
    );
    assert_eq!(fixture.events.backend.tearing_scope_outputs(), [1]);
    fixture.finish();
}
