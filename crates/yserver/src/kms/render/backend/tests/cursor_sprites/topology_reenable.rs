use super::support::{
    AtomicPathFixture, PathKind, accept_cursor_detach, accepted_live_records,
    applied_cursor_binding, create_argb_cursor, damage_root, finish_atomic_fixture,
    has_unretired_accepted_event, record_count, record_property, retire_cursor_record,
    retire_pending_output,
};
use crate::{
    drm::{CursorDeviceCaps, atomic::AtomicCommitKind},
    kms::cursor_plane::{
        ops::{CursorBufferIo, recording::RecordingCursorBufferIo},
        topology::{CursorPlaneTestPlane, CursorPlaneTestSnapshot},
    },
};
use std::{collections::HashSet, rc::Rc};
use yserver_core::backend::Backend;

const CURSOR_PROPERTIES: [&str; 10] = [
    "FB_ID", "CRTC_ID", "CRTC_X", "CRTC_Y", "CRTC_W", "CRTC_H", "SRC_X", "SRC_Y", "SRC_W", "SRC_H",
];

#[test]
#[ignore = "needs live Vulkan ICD (lavapipe)"]
// Mutation: restore active-CRTC coverage to decide_cursor_state_path; after
// the uncovered output is removed, the permanent software latch blocks the
// atomic cursor attach.
fn uncovered_topology_reenables_atomic_cursor_after_refresh() {
    let Some(mut fixture) = AtomicPathFixture::with_output_count(PathKind::Physical, 2)
        .expect("two-output atomic cursor fixture")
    else {
        return;
    };

    let crtc_a = fixture.scene.events.backend.platform.outputs[0].output.crtc;
    let crtc_b = fixture.scene.events.backend.platform.outputs[1].output.crtc;
    let crtc_a_raw = u32::from(crtc_a);
    let (buffer_io, ledger) = RecordingCursorBufferIo::new(32);
    let buffer_io: Rc<dyn CursorBufferIo> = Rc::new(buffer_io);
    let snapshot = CursorPlaneTestSnapshot {
        crtcs: vec![crtc_a_raw, u32::from(crtc_b)],
        planes: vec![CursorPlaneTestPlane {
            handle: 21,
            possible_crtcs: vec![crtc_a_raw],
            property_names: CURSOR_PROPERTIES.into_iter().map(str::to_owned).collect(),
        }],
        cursor_caps: Some(CursorDeviceCaps::new(false, Some("amdgpu".to_owned()))),
    };
    fixture
        .scene
        .events
        .backend
        .platform
        .for_tests_install_cursor_plane_from_snapshot(0, snapshot, buffer_io, 64, 64);
    fixture.ledger = ledger;

    let device_fd = fixture.scene.kms_fd(0);
    let cursor = create_argb_cursor(
        &mut fixture.scene.events.backend,
        (5, 4),
        [0x12, 0x34, 0x56, 0xff],
        (2, 1),
    );
    let root = fixture.scene.events.backend.core.window_id;
    Backend::define_cursor(
        &mut fixture.scene.events.backend,
        None,
        root,
        cursor.as_raw(),
    )
    .expect("define cursor through Backend");
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        100,
        100,
    );
    let initial_image = Backend::get_active_cursor_image(&fixture.scene.events.backend)
        .expect("cursor image is available");
    let initial_x_state = initial_image.host_xid == cursor.as_raw()
        && !fixture.scene.events.backend.effective_cursor_hidden()
        && fixture.scene.events.backend.sprite_key() == Some((initial_image.serial.into(), (2, 1)));

    damage_root(&mut fixture);
    let initial_record_offset = record_count(&fixture, device_fd);
    let initial_accepted_offset =
        accepted_live_records(&fixture.scene.events.recorders[&device_fd]).len();
    Backend::maybe_composite(&mut fixture.scene.events.backend)
        .expect("compose cursor while topology blocks the hardware plane");
    let initial_records = fixture.scene.events.recorders[&device_fd].records();
    let accepted_initial_records =
        accepted_live_records(&fixture.scene.events.recorders[&device_fd]);
    let software_scene_shown = accepted_initial_records
        .iter()
        .skip(initial_accepted_offset)
        .any(|record| {
            record.kind == AtomicCommitKind::Scene
                && record.events.iter().any(|event| event.crtc == crtc_a)
        });
    let no_initial_cursor_commit = initial_records
        .iter()
        .skip(initial_record_offset)
        .all(|record| record.kind != AtomicCommitKind::CursorOnly);
    let blocked_while_uncovered = !fixture
        .scene
        .events
        .backend
        .platform
        .cursor_plane_available()
        && !fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_visible_for_output(0);

    let mut delivered = HashSet::new();
    for output_idx in 0..2 {
        if has_unretired_accepted_event(&fixture.scene.events, &delivered, output_idx) {
            retire_pending_output(
                &mut fixture.scene.events,
                &mut delivered,
                output_idx,
                u32::try_from(output_idx + 1).expect("initial frame fits u32"),
            );
        }
    }
    let software_cursor_mode = fixture.scene.events.backend.scene.cursor_mode()
        == crate::kms::render::scene::CursorPlaneMode::Sw;

    let (output_id, connector) = {
        let backend = &mut fixture.scene.events.backend;
        let key = backend.platform.outputs[1].key.clone();
        let output_id = backend.pstate.randr_id_alloc.ids_for(&key).output_id;
        backend
            .pstate
            .output_key_by_id
            .insert(output_id, key.clone());
        backend.platform.for_tests_accept_output_power_commits();
        (output_id, key.connector_name)
    };
    let disabled = Backend::apply_crtc_config(
        &mut fixture.scene.events.backend,
        output_id,
        &connector,
        None,
        0,
        0,
    )
    .expect("remove the uncovered output through Backend");

    let hardware_available_after_refresh = disabled
        && fixture.scene.events.backend.platform.outputs.len() == 1
        && fixture
            .scene
            .events
            .backend
            .platform
            .cursor_plane_available();
    let cursor_properties = fixture.scene.events.backend.platform.devices[0]
        .cursor
        .plane
        .as_ref()
        .and_then(|plane| plane.properties_for(crtc_a))
        .expect("covered CRTC retains complete cursor properties");
    let resume_record_offset = record_count(&fixture, device_fd);
    Backend::warp_pointer_root(
        &mut fixture.scene.events.backend,
        &mut fixture.scene.events.state,
        120,
        110,
    );
    damage_root(&mut fixture);

    let mut atomic_attach_after_refresh = false;
    for frame in 20..24 {
        Backend::maybe_composite(&mut fixture.scene.events.backend)
            .expect("retry the cursor through Backend after topology refresh");
        let records = accepted_live_records(&fixture.scene.events.recorders[&device_fd]);
        let attach_accepted = records.iter().skip(resume_record_offset).any(|record| {
            record.kind == AtomicCommitKind::CursorOnly
                && record.events.iter().any(|event| event.crtc == crtc_a)
                && record_property(record, cursor_properties.fb_id).is_some_and(|fb| fb != 0)
        });
        if has_unretired_accepted_event(&fixture.scene.events, &delivered, 0) {
            retire_pending_output(&mut fixture.scene.events, &mut delivered, 0, frame);
        }
        if attach_accepted && applied_cursor_binding(&fixture.scene.events.backend, 0).is_some() {
            atomic_attach_after_refresh = true;
            break;
        }
    }

    if atomic_attach_after_refresh {
        let detach = accept_cursor_detach(&mut fixture, &mut delivered, 0);
        retire_cursor_record(&mut fixture, &mut delivered, 0, &detach, 30);
    }
    finish_atomic_fixture(fixture, &mut delivered);

    assert!(initial_x_state, "X-visible cursor state stays visible");
    assert!(
        blocked_while_uncovered,
        "uncovered topology blocks device cursor"
    );
    assert!(
        software_scene_shown && software_cursor_mode && no_initial_cursor_commit,
        "uncovered topology shows the software cursor without a cursor-plane commit"
    );
    assert!(
        hardware_available_after_refresh,
        "covered topology refresh makes the hardware cursor available again"
    );
    assert!(
        atomic_attach_after_refresh,
        "covered topology resumes atomic cursor commits"
    );
}
