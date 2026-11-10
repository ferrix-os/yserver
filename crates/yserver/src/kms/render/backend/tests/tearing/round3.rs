use super::*;
use std::collections::HashSet;

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: classify by XFIXES only, or preserve the deferred cursor wake on tearing exit.
fn transparent_cursor_is_hidden_for_direct_scanout_and_define_cursor_handoff() {
    use yserver_core::{backend::Backend, server::ServerState};

    const WINDOW: u32 = 0x78a1;
    let Some(mut backend) = live_scene_backend() else {
        return;
    };
    let recorder = recording_sink(&mut backend, 0);
    let mut state = ServerState::new();
    let root = backend.core.window_id;
    assert!(
        backend.effective_cursor_hidden(),
        "no effective cursor starts hidden"
    );

    let transparent = create_bitmap_cursor_via_backend(&mut backend, false, false);
    assert!(backend.cursor_records[&transparent.as_raw()].is_fully_transparent);
    Backend::define_cursor(&mut backend, None, root, transparent.as_raw())
        .expect("define transparent core cursor outside tearing");
    assert!(!backend.cursor_hidden, "the cursor is not XFIXES-hidden");
    assert!(backend.effective_cursor_hidden());
    damage_root_and_composite(&mut backend);
    assert!(
        backend.scene.has_pending_page_flips(),
        "expected a pending scene page flip: {}",
        tearing_composite_diagnostics(&backend, None),
    );
    retire_composed_frame(&mut backend, &mut state, 51);
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hidden
    );
    assert!(!backend.platform.any_cursor_plane_visible());

    // The visible one-pixel cursor remains a real cursor through CreateCursor.
    let opaque = create_bitmap_cursor_via_backend(&mut backend, true, true);
    assert!(!backend.cursor_records[&opaque.as_raw()].is_fully_transparent);

    // Classification also covers core glyphs, recoloring, and the animation-wide
    // rule: one opaque frame makes the animated cursor visible.
    let missing_font = yserver_core::backend::FontHandle::from_raw(0x78a2).unwrap();
    let glyph = Backend::create_glyph_cursor(
        &mut backend,
        None,
        missing_font,
        None,
        0,
        0,
        (u16::MAX, u16::MAX, u16::MAX),
        (0, 0, 0),
    )
    .expect("create transparent fallback glyph cursor");
    assert!(backend.cursor_records[&glyph.as_raw()].is_fully_transparent);
    Backend::recolor_cursor(
        &mut backend,
        None,
        glyph.as_raw(),
        (0, 0, 0),
        (u16::MAX, 0, 0),
    )
    .expect("recolor transparent glyph cursor");
    assert!(backend.cursor_records[&glyph.as_raw()].is_fully_transparent);
    let all_transparent =
        Backend::create_anim_cursor(&mut backend, None, &[(transparent, 40), (glyph, 40)])
            .expect("create all-transparent animation")
            .expect("animation handle");
    assert!(backend.anim_cursor_records[&all_transparent.as_raw()].is_fully_transparent);
    let mixed = Backend::create_anim_cursor(&mut backend, None, &[(transparent, 40), (opaque, 40)])
        .expect("create mixed animation")
        .expect("animation handle");
    assert!(!backend.anim_cursor_records[&mixed.as_raw()].is_fully_transparent);
    Backend::define_cursor(&mut backend, None, root, all_transparent.as_raw())
        .expect("define all-transparent animation");
    assert!(backend.effective_cursor_hidden());
    Backend::define_cursor(&mut backend, None, root, mixed.as_raw())
        .expect("define animation with an opaque frame");
    assert!(!backend.effective_cursor_hidden());
    Backend::define_cursor(&mut backend, None, root, transparent.as_raw())
        .expect("restore transparent core cursor");
    assert!(backend.effective_cursor_hidden());
    damage_root_and_composite(&mut backend);
    retire_composed_frame(&mut backend, &mut state, 55);
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hidden
    );
    assert!(!backend.platform.any_cursor_plane_visible());

    // The test cursor plane above checks the non-tearing path. Remove that test
    // device now so every direct async commit below is cursor-plane-free.
    backend.platform.devices[0].cursor.plane = None;
    recorder.clear_records();
    present_until_direct_async(&mut backend, &recorder, &mut state, WINDOW, 15_100);
    assert!(backend.pstate.tearing.is_on());
    assert!(backend.pstate.direct.root().active());
    assert!(!backend.platform.any_cursor_plane_visible());

    // DefineCursor from a fully transparent image to one opaque pixel must
    // unwind direct scanout and compose the cursor while tearing remains On.
    Backend::define_cursor(&mut backend, None, root, opaque.as_raw())
        .expect("define one-pixel cursor");
    assert!(!backend.effective_cursor_hidden());
    assert!(backend.tearing_cursor_wants_visible_for_outputs(&backend.tearing_scope_outputs()));
    assert!(backend.pstate.direct.root().unflip_requested);
    assert_eq!(
        backend.pstate.direct.root().unflip_reason,
        Some("tearing_cursor_handoff")
    );
    Backend::maybe_composite(&mut backend).expect("submit cursor handoff");
    assert!(!recorder.records().is_empty());
    retire_composed_frame(&mut backend, &mut state, 52);
    assert!(!backend.pstate.direct.root().active());
    assert!(!backend.pstate.direct.root().unflip_requested);
    damage_root(&mut backend);
    Backend::maybe_composite(&mut backend).expect("compose visible one-pixel cursor");
    assert!(
        backend.scene.has_pending_page_flips(),
        "expected a pending scene page flip: {}",
        tearing_composite_diagnostics(&backend, None),
    );
    retire_composed_frame(&mut backend, &mut state, 53);
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Sw
    );
    assert!(backend.scene.for_tests_cursor_footprint_was_presented(0));
    assert!(
        try_live_direct_for_ids(
            &mut backend,
            15_150,
            WINDOW,
            WINDOW,
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        )
        .is_none(),
        "a one-pixel visible cursor blocks direct scanout after its handoff"
    );

    // A transparent RENDER cursor must clear the scene cursor and permit direct
    // async scanout again after its hidden frame retires.
    let render_cursor = create_render_cursor_via_backend(&mut backend);
    assert!(backend.cursor_records[&render_cursor.as_raw()].is_fully_transparent);
    Backend::define_cursor(&mut backend, None, root, render_cursor.as_raw())
        .expect("define transparent RENDER cursor");
    assert!(backend.effective_cursor_hidden());
    Backend::maybe_composite(&mut backend).expect("compose hidden RENDER cursor");
    assert!(
        backend.scene.has_pending_page_flips(),
        "expected a pending scene page flip: {}",
        tearing_composite_diagnostics(&backend, None),
    );
    retire_composed_frame(&mut backend, &mut state, 54);
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hidden
    );
    assert!(!backend.platform.any_cursor_plane_visible());
    present_until_direct_async(&mut backend, &recorder, &mut state, WINDOW, 15_200);

    let sequence: Vec<_> = recorder
        .records()
        .iter()
        .map(|record| record.kind)
        .collect();
    assert_eq!(
        sequence,
        [
            AtomicCommitKind::Scene,
            AtomicCommitKind::Async,
            AtomicCommitKind::Scene,
            AtomicCommitKind::Async,
            AtomicCommitKind::Async,
            AtomicCommitKind::Async,
        ],
        "transparent → visible → transparent cursor transitions must hand off composed ↔ direct"
    );
    assert!(backend.pstate.tearing.is_on());
    assert!(backend.pstate.direct.root().active());
    assert!(backend.pstate.direct.root().pending.is_none());
    assert_direct_queue_drained(&backend);
    assert!(!backend.pstate.direct.root().unflip_requested);
    assert!(
        backend
            .pstate
            .direct
            .root()
            .unflip_outputs_to_replace
            .is_empty()
    );
    assert!(backend.pstate.tearing.deferred_cursor_wake.is_none());
    assert!(!backend.scene.has_pending_page_flips());
    assert!(!backend.scene.cursor_transition_pending(&[0]));
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hidden
    );
    assert!(!backend.platform.any_cursor_plane_visible());

    seed_pending_tearing_cursor_wake(&mut backend);
    let exit_present =
        try_direct_present_with_live_storage(&mut backend, 15_300, WINDOW, 800, 600, 0);
    assert!(
        exit_present.is_some(),
        "a non-async Backend Present exits tearing"
    );
    assert_tearing_off(&backend);
    assert!(backend.pstate.direct.root().pending.is_some());
    retire_direct_present(&mut backend, &mut state, 55);

    assert_tearing_off(&backend);
    assert!(backend.pstate.direct.root().active());
    assert!(backend.pstate.direct.root().hold_direct);
    assert!(!backend.pstate.direct.root().unflip_requested);
    assert!(
        backend
            .pstate
            .direct
            .root()
            .unflip_outputs_to_replace
            .is_empty()
    );
    assert_direct_queue_drained(&backend);
    assert_eq!(
        backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hidden,
        "tearing exit preserves transparent-as-hidden cursor policy"
    );
    assert!(!backend.platform.any_cursor_plane_visible());
    assert!(!backend.scene.has_pending_page_flips());
    assert!(!backend.scene.cursor_transition_pending(&[0]));
    assert_commit_sequence(
        &recorder,
        &[
            AtomicCommitKind::Scene,
            AtomicCommitKind::Async,
            AtomicCommitKind::Scene,
            AtomicCommitKind::Async,
            AtomicCommitKind::Async,
            AtomicCommitKind::Async,
            AtomicCommitKind::Scene,
        ],
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: enter tearing at probation eight while ShowOnRetire is queued;
// the entry assertion must keep TearingState::Off until the show retires.
fn entry_waits_for_queued_show_on_retire() {
    use super::super::cursor_sprites::support::{
        AtomicPathFixture, PathKind, create_argb_cursor, damage_root,
        drain_unretired_output_events, finish_atomic_fixture, retire_cursor_record,
        retire_pending_output,
    };
    use crate::drm::atomic::AtomicCommitKind;
    use yserver_core::backend::Backend;

    const WINDOW: u32 = 0x7550;
    let Some(mut fixture) = AtomicPathFixture::new(PathKind::Physical)
        .expect("atomic tearing-entry fixture construction")
    else {
        return;
    };
    let mut delivered = HashSet::new();
    let cursor_plane = fixture.scene.events.backend.platform.devices[0]
        .cursor
        .plane
        .take()
        .expect("AtomicPathFixture installed an Atomic cursor plane");
    let cursor = create_argb_cursor(&mut fixture.scene.events.backend, (4, 4), [0xff; 4], (1, 1));
    let root = fixture.scene.events.backend.core.window_id;
    Backend::define_cursor(
        &mut fixture.scene.events.backend,
        None,
        root,
        cursor.as_raw(),
    )
    .expect("select visible root cursor through Backend");
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, true);
    Backend::set_cursor_hidden(&mut fixture.scene.events.backend, false);
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        100,
        100,
    );
    damage_root(&mut fixture);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("compose initial software cursor frame");
    retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, 1);
    assert_eq!(
        fixture.scene.events.backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Sw
    );
    fixture.scene.events.backend.platform.devices[0].async_page_flip = true;

    let device_fd = fixture.scene.kms_fd(0);
    let recorder = fixture.scene.events.recorders[&device_fd].clone();
    for index in 0..7_u64 {
        assert!(
            try_direct_present_for_window(
                &mut fixture.scene.events.backend,
                1600 + index,
                WINDOW,
                800,
                600,
                yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
            )
            .is_none()
        );
        let before = recorder.records().len();
        damage_root(&mut fixture);
        Backend::maybe_composite(&mut fixture.scene.events.backend)
            .expect("compose probation frame");
        assert_eq!(recorder.records().len(), before + 1);
        assert_eq!(recorder.records()[before].kind, AtomicCommitKind::Scene);
        retire_pending_output(
            &mut fixture.scene.events,
            &mut delivered,
            0,
            u32::try_from(index + 2).expect("frame fits u32"),
        );
    }
    assert!(matches!(
        fixture.scene.events.backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::Off
    ));
    assert_eq!(
        fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .tearing_probation
            .count,
        7
    );

    fixture.scene.events.backend.platform.devices[0]
        .cursor
        .plane = Some(cursor_plane);
    let before_show = recorder.records().len();
    damage_root(&mut fixture);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("queue ShowOnRetire through the scene");
    assert!(
        fixture
            .scene
            .events
            .backend
            .scene
            .cursor_transition_pending(&[0])
    );
    assert_eq!(
        recorder.records()[before_show].kind,
        AtomicCommitKind::Scene
    );
    let before_eighth = recorder.records().len();
    assert!(
        try_direct_present_for_window(
            &mut fixture.scene.events.backend,
            1607,
            WINDOW,
            800,
            600,
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        )
        .is_none()
    );
    let entry_deferred_while_show_pending = matches!(
        fixture.scene.events.backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::Off
    );
    assert_eq!(
        fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .tearing_probation
            .count,
        8
    );
    assert_eq!(recorder.records().len(), before_eighth);
    assert!(
        fixture
            .scene
            .events
            .backend
            .scene
            .cursor_transition_pending(&[0])
    );
    assert!(
        !fixture
            .scene
            .events
            .backend
            .platform
            .any_cursor_plane_visible()
    );

    retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, 9);
    drain_unretired_output_events(&mut fixture, &mut delivered, 0);
    assert!(
        !fixture
            .scene
            .events
            .backend
            .scene
            .cursor_transition_pending(&[0])
    );
    assert!(
        fixture
            .scene
            .events
            .backend
            .platform
            .any_cursor_plane_visible()
    );
    assert_eq!(
        fixture.scene.events.backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hw
    );
    let tearing_stayed_off_after_show_retired = matches!(
        fixture.scene.events.backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::Off
    );
    assert!(
        try_direct_present_for_window(
            &mut fixture.scene.events.backend,
            1608,
            WINDOW,
            800,
            600,
            yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        )
        .is_none()
    );
    let entry_started_after_show_retired = matches!(
        fixture.scene.events.backend.pstate.tearing.state,
        crate::kms::render::backend::TearingState::On { window, .. } if window == WINDOW
    );
    let detach = super::super::cursor_sprites::support::accept_cursor_detach(
        &mut fixture,
        &mut delivered,
        0,
    );
    retire_cursor_record(&mut fixture, &mut delivered, 0, &detach, 10);
    finish_atomic_fixture(fixture, &mut delivered);
    assert!(
        entry_deferred_while_show_pending,
        "tearing entry waits until queued ShowOnRetire is settled"
    );
    assert!(
        tearing_stayed_off_after_show_retired,
        "queued show retirement does not retroactively enter tearing"
    );
    assert!(
        entry_started_after_show_retired,
        "entry resumes after the show retires"
    );
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutations: accept the entry Present directly while still requesting handoff,
// submit the handoff before direct retirement, or skip the tearing-entry unflip.
fn entry_from_held_direct_with_visible_hw_cursor_runs_handoff() {
    use super::super::cursor_sprites::support::{
        AtomicPathFixture, PathKind, accept_cursor_detach, create_argb_cursor, damage_root,
        drain_unretired_output_events, enter_held_direct, establish_hardware_cursor,
        finish_atomic_fixture, retire_cursor_record, retire_pending_output,
    };
    use crate::drm::atomic::AtomicCommitKind;
    use yserver_core::{backend::Backend, core_loop::input_inventory::InputInventory};

    let Some(mut fixture) = AtomicPathFixture::new(PathKind::Physical)
        .expect("atomic cursor handoff fixture construction")
    else {
        return;
    };
    for device in &mut fixture.scene.events.backend.platform.devices {
        device.async_page_flip = true;
    }
    let mut delivered = HashSet::new();
    let cursor = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (8, 8),
        [0xff, 0xff, 0xff, 0xff],
        (2, 3),
    );
    let hardware = establish_hardware_cursor(&mut fixture, &mut delivered, cursor, (120, 100));
    drain_unretired_output_events(&mut fixture, &mut delivered, 0);
    let direct = enter_held_direct(&mut fixture, &mut delivered, 70, None)
        .expect("enter held direct scanout with the applied cursor");
    assert!(
        fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .current
            .is_some()
    );
    assert!(
        fixture
            .scene
            .events
            .backend
            .platform
            .any_cursor_plane_visible()
    );
    assert_eq!(
        fixture.scene.events.backend.scene.cursor_mode(),
        crate::kms::render::scene::CursorPlaneMode::Hw
    );

    let target = direct.target_xid;
    Backend::copy_area(
        &mut fixture.scene.events.backend,
        None,
        direct.candidate.src_host_xid,
        target,
        0,
        0,
        0,
        0,
        800,
        600,
    )
    .expect("paint held-direct fallback target from the direct frame");
    fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root_mut()
        .tearing_probation
        .window = Some(target);
    fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root_mut()
        .tearing_probation
        .count = crate::kms::render::backend::TEARING_ENTRY_PROBATION - 1;
    let (candidate, event, _) = prepare_direct_present_for_ids_with_storage(
        &mut fixture.scene.events.backend,
        71,
        target,
        target,
        800,
        600,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        true,
    );
    let entry_present_accepted_direct =
        Backend::try_present_direct(&mut fixture.scene.events.backend, candidate, event)
            .expect("process the tearing-entry Present");
    Backend::copy_area(
        &mut fixture.scene.events.backend,
        None,
        candidate.src_host_xid,
        target,
        0,
        0,
        0,
        0,
        800,
        600,
    )
    .expect("copy the entry Present into its composed fallback target");
    fixture
        .scene
        .events
        .backend
        .scene
        .mark_scene_structure_dirty();
    Backend::note_present_pixmap(
        &mut fixture.scene.events.backend,
        candidate.src_pixmap_xid,
        target,
    );
    let tearing_started = fixture.scene.events.backend.pstate.tearing.is_on();
    let handoff_requested = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .unflip_requested;
    let handoff_reason = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .unflip_reason;
    let direct_retained_before_handoff = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .current
        .is_some();
    let hardware_cursor_visible_before_handoff = fixture
        .scene
        .events
        .backend
        .platform
        .any_cursor_plane_visible();
    if !handoff_requested || entry_present_accepted_direct {
        if entry_present_accepted_direct {
            drain_unretired_output_events(&mut fixture, &mut delivered, 0);
        }
        super::super::commit_identity::install_recording_vt_io(&mut fixture.scene);
        Backend::begin_vt_release(&mut fixture.scene.events.backend);
        Backend::finish_vt_release(
            &mut fixture.scene.events.backend,
            &mut fixture.scene.events.state,
            &InputInventory::default(),
        );
        Backend::on_vt_acquire(
            &mut fixture.scene.events.backend,
            &mut fixture.scene.events.state,
        );
        let detach = accept_cursor_detach(&mut fixture, &mut delivered, hardware.output_idx);
        retire_cursor_record(
            &mut fixture,
            &mut delivered,
            hardware.output_idx,
            &detach,
            74,
        );
        fixture.scene.events.wait_for_submitted_gpu_work();
        Backend::free_pixmap(
            &mut fixture.scene.events.backend,
            None,
            candidate.src_pixmap_xid,
        )
        .expect("free the entry Present source after VT cleanup");
        finish_atomic_fixture(fixture, &mut delivered);
        assert!(tearing_started, "entry Present starts tearing");
        assert!(
            !entry_present_accepted_direct,
            "entry Present must not be accepted as direct while a cursor handoff is needed"
        );
        assert!(
            handoff_requested,
            "visible hardware cursor requests the direct unflip"
        );
        return;
    }

    let device_fd = hardware.device_fd;
    let recorder = fixture.scene.events.recorders[&device_fd].clone();
    let before_handoff = recorder.records().len();
    damage_root(&mut fixture);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("submit composed cursor handoff");
    let one_handoff_commit_before_retire = recorder.records().len() == before_handoff + 1;
    let handoff_record_ok = recorder
        .records()
        .get(before_handoff)
        .is_some_and(|record| {
            matches!(
                record.kind,
                AtomicCommitKind::Scene | AtomicCommitKind::SceneWithCursor
            ) && !record.events.is_empty()
        });
    if !one_handoff_commit_before_retire {
        super::super::commit_identity::install_recording_vt_io(&mut fixture.scene);
        Backend::begin_vt_release(&mut fixture.scene.events.backend);
        Backend::finish_vt_release(
            &mut fixture.scene.events.backend,
            &mut fixture.scene.events.state,
            &InputInventory::default(),
        );
        Backend::on_vt_acquire(
            &mut fixture.scene.events.backend,
            &mut fixture.scene.events.state,
        );
        let detach = accept_cursor_detach(&mut fixture, &mut delivered, hardware.output_idx);
        retire_cursor_record(
            &mut fixture,
            &mut delivered,
            hardware.output_idx,
            &detach,
            75,
        );
        fixture.scene.events.wait_for_submitted_gpu_work();
        Backend::free_pixmap(
            &mut fixture.scene.events.backend,
            None,
            candidate.src_pixmap_xid,
        )
        .expect("free the entry Present source after VT cleanup");
        finish_atomic_fixture(fixture, &mut delivered);
        assert!(
            one_handoff_commit_before_retire,
            "the next composed frame waits for direct handoff retirement"
        );
        return;
    }
    retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, 72);
    drain_unretired_output_events(&mut fixture, &mut delivered, 0);
    let direct_retired_after_handoff = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .current
        .is_none();
    let hardware_cursor_hidden_after_handoff = !fixture
        .scene
        .events
        .backend
        .platform
        .any_cursor_plane_visible();
    let software_cursor_mode_after_handoff = fixture.scene.events.backend.scene.cursor_mode()
        == crate::kms::render::scene::CursorPlaneMode::Sw;

    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("compose software cursor after the hardware hide");
    let software_async_submitted = fixture.scene.events.backend.scene.has_pending_page_flips()
        && recorder
            .records()
            .last()
            .is_some_and(|record| record.kind == AtomicCommitKind::Async);
    if !software_async_submitted {
        fixture
            .scene
            .events
            .backend
            .platform
            .for_tests_accept_output_power_commits();
        Backend::begin_vt_release(&mut fixture.scene.events.backend);
        Backend::finish_vt_release(
            &mut fixture.scene.events.backend,
            &mut fixture.scene.events.state,
            &InputInventory::default(),
        );
        fixture.scene.events.wait_for_submitted_gpu_work();
        Backend::free_pixmap(
            &mut fixture.scene.events.backend,
            None,
            candidate.src_pixmap_xid,
        )
        .expect("free the entry Present source after VT cleanup");
        finish_atomic_fixture(fixture, &mut delivered);
        assert!(
            software_async_submitted,
            "software cursor frame is submitted after hide"
        );
        return;
    }
    retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, 73);
    let software_cursor_presented = fixture
        .scene
        .events
        .backend
        .scene
        .for_tests_cursor_footprint_was_presented(0);
    let unflip_cleared = !fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .unflip_requested;
    let no_pending_scene = !fixture.scene.events.backend.scene.has_pending_page_flips();
    fixture.scene.events.wait_for_submitted_gpu_work();
    Backend::free_pixmap(
        &mut fixture.scene.events.backend,
        None,
        candidate.src_pixmap_xid,
    )
    .expect("free the manually allocated entry Present source pixmap");
    finish_atomic_fixture(fixture, &mut delivered);
    assert!(tearing_started, "entry Present starts tearing");
    assert!(
        handoff_requested,
        "visible hardware cursor requests the direct unflip"
    );
    assert_eq!(handoff_reason, Some("tearing_cursor_handoff"));
    assert!(
        direct_retained_before_handoff,
        "direct frame remains held through handoff"
    );
    assert!(
        hardware_cursor_visible_before_handoff,
        "hardware cursor stays visible until the composed handoff retires"
    );
    assert!(
        handoff_record_ok,
        "handoff submits an event-bearing Scene commit"
    );
    assert!(
        one_handoff_commit_before_retire,
        "the next composed frame waits for direct handoff retirement"
    );
    assert!(
        direct_retired_after_handoff,
        "handoff retirement releases direct ownership"
    );
    assert!(
        hardware_cursor_hidden_after_handoff,
        "handoff retirement hides the HW cursor"
    );
    assert!(
        software_cursor_mode_after_handoff,
        "retirement transfers cursor to software"
    );
    assert!(
        software_cursor_presented,
        "software cursor appears after hardware hide"
    );
    assert!(unflip_cleared, "handoff clears unflip state");
    assert!(no_pending_scene, "handoff leaves no pending scene flips");
}

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: materialize retained direct source F0 over newest copy F1 before
// unflip; the recorded primary BO must still contain F1.
fn handoff_keeps_newest_copy_not_retained_direct_frame() {
    use super::super::cursor_sprites::support::{
        AtomicPathFixture, PathKind, create_argb_cursor, damage_root,
        drain_unretired_output_events, enter_held_direct, establish_hardware_cursor,
        finish_atomic_fixture, primary_bo_index_for_record, retire_pending_output,
    };
    use crate::drm::atomic::AtomicCommitKind;
    use yserver_core::{backend::Backend, core_loop::input_inventory::InputInventory};

    const F0_RED: u32 = 0xffff_0000;
    const F1_GREEN: u32 = 0xff00_ff00;
    let Some(mut fixture) = AtomicPathFixture::new(PathKind::Physical)
        .expect("atomic newest-copy fixture construction")
    else {
        return;
    };
    fixture.scene.events.backend.platform.devices[0].async_page_flip = true;
    let mut delivered = HashSet::new();
    let cursor = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (8, 8),
        [0xff, 0xff, 0xff, 0xff],
        (2, 3),
    );
    let hardware = establish_hardware_cursor(&mut fixture, &mut delivered, cursor, (120, 100));
    drain_unretired_output_events(&mut fixture, &mut delivered, 0);
    let direct = enter_held_direct(&mut fixture, &mut delivered, 80, Some(F0_RED))
        .expect("enter direct scanout with F0 as the held frame");
    let target = direct.target_xid;
    Backend::fill_rectangle(
        &mut fixture.scene.events.backend,
        None,
        direct.candidate.src_host_xid,
        F0_RED,
        0,
        0,
        800,
        600,
    )
    .expect("paint retained direct frame F0");
    Backend::copy_area(
        &mut fixture.scene.events.backend,
        None,
        direct.candidate.src_host_xid,
        direct.target_xid,
        0,
        0,
        0,
        0,
        800,
        600,
    )
    .expect("paint fallback target from retained direct frame F0");

    fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root_mut()
        .tearing_probation
        .window = Some(target);
    fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root_mut()
        .tearing_probation
        .count = crate::kms::render::backend::TEARING_ENTRY_PROBATION - 1;
    let (candidate, event, _) = prepare_direct_present_for_ids_with_storage(
        &mut fixture.scene.events.backend,
        81,
        target,
        target,
        800,
        600,
        yserver_core::present_scheduler::PRESENT_OPTION_ASYNC,
        true,
    );
    Backend::fill_rectangle(
        &mut fixture.scene.events.backend,
        None,
        candidate.src_host_xid,
        F1_GREEN,
        0,
        0,
        800,
        600,
    )
    .expect("paint newest copy F1");
    let f1_source = fixture
        .scene
        .events
        .backend
        .store
        .lookup(candidate.src_host_xid)
        .expect("F1 source drawable");
    assert!(
        !Backend::try_present_direct(&mut fixture.scene.events.backend, candidate, event)
            .expect("cursor handoff keeps the newest Present as a Copy")
    );
    Backend::copy_area(
        &mut fixture.scene.events.backend,
        None,
        candidate.src_host_xid,
        direct.target_xid,
        0,
        0,
        0,
        0,
        800,
        600,
    )
    .expect("copy newest F1 into its composed fallback target");
    fixture
        .scene
        .events
        .backend
        .scene
        .mark_scene_structure_dirty();
    assert!(fixture.scene.events.backend.pstate.tearing.is_on());
    assert!(
        fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root()
            .unflip_requested
    );
    let fallback_source_is_f1 = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .unflip_fallback_source
        == Some(f1_source);
    Backend::note_present_pixmap(
        &mut fixture.scene.events.backend,
        candidate.src_pixmap_xid,
        target,
    );
    damage_root(&mut fixture);
    let fallback_copy_ready = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .unflip_shadow_ready;
    if !fallback_source_is_f1 || !fallback_copy_ready {
        fixture
            .scene
            .events
            .backend
            .pstate
            .direct
            .root_mut()
            .unflip_fallback_source = Some(f1_source);
        fixture
            .scene
            .events
            .backend
            .platform
            .for_tests_accept_output_power_commits();
        Backend::begin_vt_release(&mut fixture.scene.events.backend);
        Backend::finish_vt_release(
            &mut fixture.scene.events.backend,
            &mut fixture.scene.events.state,
            &InputInventory::default(),
        );
        fixture.scene.events.wait_for_submitted_gpu_work();
        Backend::free_pixmap(
            &mut fixture.scene.events.backend,
            None,
            candidate.src_pixmap_xid,
        )
        .expect("free the F1 source after VT cleanup");
        fixture.scene.finish();
        assert!(
            fallback_source_is_f1,
            "cursor handoff registers newest F1 source"
        );
        assert!(
            fallback_copy_ready,
            "Present copy makes the unflip shadow ready"
        );
        return;
    }

    let device_fd = hardware.device_fd;
    let recorder = fixture.scene.events.recorders[&device_fd].clone();
    let before_scene = recorder.records().len();
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("compose the newest copy as the unflip replacement");
    let scene_record = recorder
        .records()
        .into_iter()
        .skip(before_scene)
        .find(|record| {
            record.kind != AtomicCommitKind::CursorOnly
                && record.result == crate::drm::atomic::tests_support::ScriptedResult::Ok
        })
        .unwrap_or_else(|| {
            panic!(
                "unflip replacement commit missing: kinds={:?}, diagnostics={}",
                recorder
                    .records()
                    .into_iter()
                    .skip(before_scene)
                    .map(|record| record.kind)
                    .collect::<Vec<_>>(),
                super::tearing_composite_diagnostics(&fixture.scene.events.backend, None,),
            )
        });
    let primary_bo = primary_bo_index_for_record(
        &fixture.scene.events.backend.platform,
        hardware.output_idx,
        &scene_record,
    );
    fixture.scene.events.wait_for_submitted_gpu_work();
    let primary_bo_pixmap = 0x6a00 + u32::try_from(primary_bo).expect("BO index fits");
    let primary_pixel = fixture
        .scene
        .events
        .backend
        .get_image_pixels_for_tests(primary_bo_pixmap, 2, 10, 10, 1, 1, !0)
        .expect("read the first handoff's primary BO directly")
        .expect("primary BO pixel bytes");
    let primary_pixel_is_newest_copy = primary_pixel == [0, 0xff, 0, 0xff];
    retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, 82);
    drain_unretired_output_events(&mut fixture, &mut delivered, 0);
    let direct_retired = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .current
        .is_none();

    damage_root(&mut fixture);
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("compose newest copy after direct retirement");
    retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, 83);
    let direct_cleared_after_async = fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .current
        .is_none();
    fixture.scene.events.wait_for_submitted_gpu_work();
    Backend::free_pixmap(
        &mut fixture.scene.events.backend,
        None,
        candidate.src_pixmap_xid,
    )
    .expect("free the manually allocated F1 source pixmap");
    let unflip_cleared = !fixture
        .scene
        .events
        .backend
        .pstate
        .direct
        .root()
        .unflip_requested;
    finish_atomic_fixture(fixture, &mut delivered);
    assert!(
        direct_retired,
        "first composed frame retires direct ownership"
    );
    assert!(
        direct_cleared_after_async,
        "composed Async retirement clears direct ownership"
    );
    assert!(
        fallback_source_is_f1,
        "cursor handoff registers newest F1 source"
    );
    assert!(
        fallback_copy_ready,
        "Present copy makes the unflip shadow ready"
    );
    assert!(
        primary_pixel_is_newest_copy,
        "the primary FB_ID BO contains F1, never retained direct frame F0: kind={:?}, events={:?}, pixel={primary_pixel:?}",
        scene_record.kind, scene_record.events,
    );
    assert!(
        unflip_cleared,
        "unflip state clears after the composed frame retires"
    );
}
