use crate::kms::render::platform::{
    CursorFailureDisposition, CursorHideRecovery, CursorMoveOutcome, KmsCursorState, KmsDevice,
    PendingAtomicCursorMove, PlatformBackend, TransientCursorFallback,
};
use std::{
    collections::{HashMap, HashSet},
    io,
    rc::Rc,
    time::Instant,
};

pub(crate) const VRR_CURSOR_DEADLINE: std::time::Duration = std::time::Duration::from_millis(33);

fn cursor_error_is_transient_fallback(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::InvalidInput
}

fn classify_cursor_failure(error: &io::Error) -> CursorFailureDisposition {
    if cursor_error_is_transient_fallback(error) {
        CursorFailureDisposition::Transient
    } else {
        CursorFailureDisposition::Unchanged
    }
}

pub(super) fn cursor_dimensions_fit(
    plane_width: u32,
    plane_height: u32,
    width: u32,
    height: u32,
) -> bool {
    width <= plane_width && height <= plane_height
}

impl KmsCursorState {
    pub(crate) fn new() -> Self {
        Self {
            plane: None,
            crtc_states: HashMap::new(),
            atomic_positions: HashMap::new(),
            atomic_pending_moves: HashMap::new(),
            atomic_cursor_commits_in_flight: HashMap::new(),
            cursor_retry_waits_for_flip: HashSet::new(),
            cursor_hide_recovery: HashMap::new(),
            permanently_disabled_crtcs: HashSet::new(),
            permanently_disabled: false,
            initialization_retryable: false,
            headless_deferred: true,
            topology_blocked: false,
            transient_fallback_crtcs: HashMap::new(),
            atomic_cursor: false,
            sprite_signature: None,
        }
    }

    pub(crate) fn hw_bound_on(&self, crtc: ::drm::control::crtc::Handle) -> bool {
        if self.state_path() == crate::kms::cursor_plane::topology::CursorStatePath::Atomic {
            self.crtc_states
                .get(&crtc)
                .is_some_and(|state| state.applied.is_some())
        } else {
            false
        }
    }

    fn remember_atomic_position(
        &mut self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
        x: i32,
        y: i32,
        motion_timestamp: Option<Instant>,
    ) {
        if !self.atomic_cursor || self.permanently_disabled_crtcs.contains(&crtc) {
            return;
        }
        let Some((plane, crtc_x, crtc_y)) = self
            .plane
            .as_ref()
            .and_then(|plane| plane.atomic_position_properties(crtc))
        else {
            return;
        };
        let position = crate::drm::atomic::CursorPlanePosition {
            device_key,
            crtc,
            plane,
            crtc_x,
            crtc_y,
            x,
            y,
            motion_timestamp,
        };
        self.atomic_positions.insert(crtc, position);
        if !self.cursor_hide_recovery.contains_key(&crtc) {
            let deadline = self
                .atomic_pending_moves
                .get(&crtc)
                .and_then(|pending| pending.deadline);
            self.atomic_pending_moves
                .insert(crtc, PendingAtomicCursorMove { position, deadline });
        }
    }

    pub(super) fn clear_atomic_position(&mut self, crtc: ::drm::control::crtc::Handle) {
        self.atomic_positions.remove(&crtc);
        self.atomic_pending_moves.remove(&crtc);
    }

    pub(super) fn available_on(&self, crtc: ::drm::control::crtc::Handle) -> bool {
        self.state_path() == crate::kms::cursor_plane::topology::CursorStatePath::Atomic
            && self.atomic_cursor
            && self.plane.is_some()
            && !self.permanently_disabled
            && !self.permanently_disabled_crtcs.contains(&crtc)
            && !self.topology_blocked
            && self
                .transient_fallback_crtcs
                .get(&crtc)
                .is_none_or(|retry| retry.remaining_sw_retires == 0)
    }

    pub(super) fn note_einval(&mut self, crtc: ::drm::control::crtc::Handle) {
        let retry = self
            .transient_fallback_crtcs
            .entry(crtc)
            .or_insert(TransientCursorFallback {
                remaining_sw_retires: 0,
                failures: 0,
            });
        retry.failures = retry.failures.saturating_add(1);
        let shift = retry.failures.saturating_sub(1).min(3);
        retry.remaining_sw_retires = 1_u8 << shift;
    }

    pub(super) fn note_cursor_success(&mut self, crtc: ::drm::control::crtc::Handle) {
        self.transient_fallback_crtcs.remove(&crtc);
        self.cursor_hide_recovery.remove(&crtc);
    }

    pub(super) fn note_cursor_failure_pair(
        &mut self,
        crtc: ::drm::control::crtc::Handle,
        operation_error: &io::Error,
    ) -> CursorFailureDisposition {
        let disposition = classify_cursor_failure(operation_error);
        match disposition {
            CursorFailureDisposition::Unchanged => {}
            CursorFailureDisposition::Transient => {
                self.note_einval(crtc);
                // Once eligibility changes, the cursorless scene handoff owns
                // recovery. A position-only retry must not race it.
            }
        }
        disposition
    }

    pub(super) fn note_initialization_failure(&mut self) {
        self.headless_deferred = false;
        self.initialization_retryable = !self.permanently_disabled;
    }

    pub(super) fn should_initialize_headless_deferred(&self, has_active_crtcs: bool) -> bool {
        has_active_crtcs
            && self.headless_deferred
            && self.plane.is_none()
            && !self.permanently_disabled
            && !self.initialization_retryable
    }

    pub(super) fn should_retry_initialization(&self, has_active_crtcs: bool) -> bool {
        has_active_crtcs
            && self.plane.is_none()
            && !self.permanently_disabled
            && self.initialization_retryable
    }
}

impl PlatformBackend {
    pub(crate) fn invalidate_cursor_applied(
        &mut self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) {
        let Some(device) = self
            .devices
            .iter_mut()
            .find(|device| device.key == device_key)
        else {
            return;
        };
        device.cursor.crtc_states.entry(crtc).or_default().valid = false;
    }

    /// Drop the cursor positions cached and queued for one CRTC, and force
    /// its next cursor commit, after that CRTC moved in the root window.
    pub(crate) fn forget_cursor_position_for_crtc(
        &mut self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) {
        let Some(device) = self
            .devices
            .iter_mut()
            .find(|device| device.key == device_key)
        else {
            return;
        };
        device.cursor.clear_atomic_position(crtc);
        device.cursor.crtc_states.entry(crtc).or_default().valid = false;
    }

    pub(crate) fn invalidate_live_cursor_applied(&mut self) {
        let live_crtcs: Vec<_> = self
            .outputs
            .iter()
            .map(|layout| (layout.key.device_key, layout.output.crtc))
            .collect();
        for (device_key, crtc) in live_crtcs {
            self.invalidate_cursor_applied(device_key, crtc);
        }
    }

    /// Submit the Atomic-path blocking detach before a CRTC is disabled.
    /// Software cursor devices own no KMS cursor binding.
    pub(crate) fn detach_atomic_cursor_before_disable(
        &mut self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) -> io::Result<()> {
        let Some(device_idx) = self
            .devices
            .iter()
            .position(|device| device.key == device_key)
        else {
            return Ok(());
        };
        if self.devices[device_idx].cursor.state_path()
            != crate::kms::cursor_plane::topology::CursorStatePath::Atomic
        {
            return Ok(());
        }
        let cursor = &self.devices[device_idx].cursor;
        let owns_binding = cursor
            .crtc_states
            .get(&crtc)
            .is_some_and(|state| state.applied.is_some())
            || cursor
                .atomic_cursor_commits_in_flight
                .get(&crtc)
                .and_then(|commit| commit.prepared.as_ref())
                .is_some_and(|prepared| prepared.prior.is_some());
        if !owns_binding {
            return Ok(());
        }
        self.detach_cursor_blocking(device_idx, crtc)
    }

    fn release_cursor_crtc_owned_leases(
        &mut self,
        device_idx: usize,
        crtc: ::drm::control::crtc::Handle,
    ) {
        let applied = self.devices[device_idx]
            .cursor
            .crtc_states
            .get(&crtc)
            .and_then(|state| state.applied);
        let prior = self.devices[device_idx]
            .cursor
            .atomic_cursor_commits_in_flight
            .get(&crtc)
            .and_then(|commit| commit.prepared.as_ref())
            .and_then(|prepared| prepared.prior);
        self.release_cursor_bindings_once(device_idx, [applied, prior], "all-off CRTC cleanup");
        let cursor = &mut self.devices[device_idx].cursor;
        cursor.crtc_states.remove(&crtc);
        cursor.atomic_cursor_commits_in_flight.remove(&crtc);
        cursor.clear_atomic_position(crtc);
        cursor.cursor_retry_waits_for_flip.remove(&crtc);
        cursor.cursor_hide_recovery.remove(&crtc);
        cursor.transient_fallback_crtcs.remove(&crtc);
    }

    /// Release ownership after a successful all-off topology removal. The
    /// output must already be absent from the live topology; this guard keeps
    /// refresh/removal bookkeeping from releasing a surviving CRTC's lease.
    pub(crate) fn release_cursor_crtc_ownership_after_all_off(
        &mut self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) {
        if self
            .outputs
            .iter()
            .any(|layout| layout.key.device_key == device_key && layout.output.crtc == crtc)
        {
            log::error!(
                "render cursor: refusing removed-CRTC cleanup for live device {device_key} CRTC {crtc:?}"
            );
            return;
        }
        let Some(device_idx) = self
            .devices
            .iter()
            .position(|device| device.key == device_key)
        else {
            return;
        };
        self.release_cursor_crtc_owned_leases(device_idx, crtc);
    }

    pub(crate) fn settle_disabled_cursor_crtc(
        &mut self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) {
        let Some(device_idx) = self
            .devices
            .iter()
            .position(|device| device.key == device_key)
        else {
            return;
        };
        self.release_cursor_crtc_owned_leases(device_idx, crtc);
    }
}

impl CursorMoveOutcome {
    pub(super) fn merge(&mut self, other: Self) {
        self.ebusy_count = self.ebusy_count.saturating_add(other.ebusy_count);
        self.fallback_changed |= other.fallback_changed;
        self.retry_required |= other.retry_required;
        self.affected_crtcs.extend(other.affected_crtcs);
    }
}

pub(super) fn apply_cursor_operation_error(
    state: &mut KmsCursorState,
    crtc: ::drm::control::crtc::Handle,
    error: &io::Error,
) -> CursorFailureDisposition {
    state.note_cursor_failure_pair(crtc, error)
}

pub(super) fn install_cursor_plane_for_device(
    kms_device: &mut KmsDevice,
    crtcs: &[::drm::control::crtc::Handle],
    boundary: &str,
    plane: crate::kms::cursor_plane::CursorPlane,
) {
    let decision = plane.state_path();
    if decision.path == crate::kms::cursor_plane::topology::CursorStatePath::Atomic {
        log::info!(
            "render cursor: device {} cursor_state_path=atomic reason={}",
            kms_device.key,
            decision.reason,
        );
    } else {
        log::info!(
            "render cursor: device {} using software cursor because {}",
            kms_device.key,
            decision.reason,
        );
    }
    kms_device.cursor.topology_blocked = !plane.supports_crtcs(crtcs);
    kms_device.cursor.permanently_disabled |=
        decision.path == crate::kms::cursor_plane::topology::CursorStatePath::Software;
    kms_device.cursor.atomic_cursor = decision.path
        == crate::kms::cursor_plane::topology::CursorStatePath::Atomic
        && plane.supports_atomic_positions(crtcs);
    kms_device.cursor.initialization_retryable = false;
    kms_device.cursor.headless_deferred = false;
    log::info!(
        "render cursor: device {} initialized {}x{} ARGB8888 for {} active CRTC(s) at {boundary}; atomic_cursor={} topology_blocked={}",
        kms_device.key,
        plane.width(),
        plane.height(),
        crtcs.len(),
        kms_device.cursor.atomic_cursor,
        kms_device.cursor.topology_blocked,
    );
    kms_device.cursor.plane = Some(plane);
}

pub(super) fn initialize_cursor_plane_for_device(
    kms_device: &mut KmsDevice,
    crtcs: &[::drm::control::crtc::Handle],
    boundary: &str,
) {
    kms_device.cursor.headless_deferred = false;
    match crate::kms::cursor_plane::CursorPlane::new(Rc::clone(&kms_device.device), crtcs) {
        Ok(plane) => install_cursor_plane_for_device(kms_device, crtcs, boundary, plane),
        Err(error) => {
            kms_device.cursor.note_initialization_failure();
            let retry = if kms_device.cursor.initialization_retryable {
                "will retry at an explicit topology/resume boundary"
            } else {
                "cursor support is permanently unavailable on this device"
            };
            log::warn!(
                "render cursor: device {} initialization failed at {boundary} ({error}); using software cursor, {retry}",
                kms_device.key
            );
        }
    }
}

impl PlatformBackend {
    // Atomic cursor-plane operations are kept per output and resolve the
    // owning DRM device before preparing, showing, moving, or hiding a sprite.

    /// True iff at least one output currently has an eligible Atomic cursor
    /// plane. The scene strategy decision gates on this value.
    #[must_use]
    pub(crate) fn cursor_plane_available(&self) -> bool {
        self.outputs
            .iter()
            .enumerate()
            .any(|(output_idx, _)| self.cursor_plane_available_for_output(output_idx))
    }

    /// True iff this output's owning DRM device currently has an eligible
    /// Atomic cursor plane. Raw CRTC handles never select a device.
    #[must_use]
    pub(super) fn cursor_plane_available_for_output(&self, output_idx: usize) -> bool {
        let Some(layout) = self.outputs.get(output_idx) else {
            return false;
        };
        self.device_for_key(layout.key.device_key)
            .is_some_and(|device| device.cursor.available_on(layout.output.crtc))
    }

    /// Latest position for every listed output whose owning device selected
    /// the atomic cursor path and whose cursor plane is currently visible.
    #[must_use]
    pub(crate) fn cursor_plane_atomic_positions_for_outputs(
        &self,
        output_indices: &[usize],
    ) -> Vec<crate::drm::atomic::CursorPlanePosition> {
        let mut positions = Vec::new();
        for output_idx in output_indices {
            let Some(layout) = self.outputs.get(*output_idx) else {
                continue;
            };
            let Some(device) = self.device_for_key(layout.key.device_key) else {
                continue;
            };
            let crtc = layout.output.crtc;
            if !device.cursor.atomic_cursor
                || device.cursor.permanently_disabled
                || device.cursor.permanently_disabled_crtcs.contains(&crtc)
                || device.cursor.cursor_hide_recovery.contains_key(&crtc)
                || !device.cursor.hw_bound_on(crtc)
            {
                continue;
            }
            if let Some(position) = device.cursor.atomic_positions.get(&crtc).copied() {
                positions.push(position);
            }
        }
        positions
    }

    fn vrr_cursor_hold_applies(&self, output_idx: usize, live_direct_vrr_wanted: bool) -> bool {
        live_direct_vrr_wanted
            && self
                .outputs
                .get(output_idx)
                .is_some_and(|layout| layout.output.vrr_committed.get())
    }

    fn eligible_atomic_cursor_moves<'a>(
        &'a self,
        live_direct_vrr_wanted: &'a [bool],
        eligible_outputs: Option<&'a [bool]>,
    ) -> impl Iterator<Item = (Option<Instant>, bool)> + 'a {
        self.devices.iter().flat_map(move |device| {
            device
                .cursor
                .atomic_pending_moves
                .iter()
                .filter_map(move |(crtc, pending)| {
                    let output_is_eligible = eligible_outputs.is_none_or(|eligible| {
                        self.outputs.iter().enumerate().any(|(output_idx, layout)| {
                            layout.key.device_key == device.key
                                && layout.output.crtc == *crtc
                                && eligible.get(output_idx).copied().unwrap_or(false)
                        })
                    });
                    let output_is_vrr_on =
                        self.outputs.iter().enumerate().any(|(output_idx, layout)| {
                            layout.key.device_key == device.key
                                && layout.output.crtc == *crtc
                                && self.vrr_cursor_hold_applies(
                                    output_idx,
                                    live_direct_vrr_wanted
                                        .get(output_idx)
                                        .copied()
                                        .unwrap_or(false),
                                )
                        });
                    (device.cursor.atomic_cursor
                        && !device
                            .cursor
                            .atomic_cursor_commits_in_flight
                            .contains_key(crtc)
                        && !device.cursor.cursor_retry_waits_for_flip.contains(crtc)
                        && !device.cursor.cursor_hide_recovery.contains_key(crtc)
                        && device.cursor.available_on(*crtc)
                        && device.cursor.hw_bound_on(*crtc))
                    .then_some((pending.deadline, output_is_vrr_on))
                    .filter(|_| output_is_eligible)
                })
        })
    }

    /// Whether a selected output has a cursor-only position that can make the
    /// composed-scene scheduling pass runnable.
    pub(crate) fn has_eligible_atomic_cursor_move_for_outputs(
        &self,
        now: Instant,
        live_direct_vrr_wanted: &[bool],
        eligible_outputs: &[bool],
    ) -> bool {
        self.eligible_atomic_cursor_moves(live_direct_vrr_wanted, Some(eligible_outputs))
            .any(|(deadline, output_is_vrr_on)| {
                !output_is_vrr_on || deadline.is_none_or(|deadline| now >= deadline)
            })
    }

    /// Earliest held VRR cursor position that is eligible to wake scanout.
    pub(crate) fn vrr_cursor_deadline(&self, live_direct_vrr_wanted: &[bool]) -> Option<Instant> {
        self.eligible_atomic_cursor_moves(live_direct_vrr_wanted, None)
            .filter_map(|(deadline, output_is_vrr_on)| {
                output_is_vrr_on.then_some(deadline).flatten()
            })
            .min()
    }

    /// Only an accepted cursor-only commit owes a flip event. An EBUSY wait owes
    /// none of ours: the old-event drain fail-stops on a missing event, so it must
    /// not wait for one; the wait mark is cleared after the drain instead.
    pub(crate) fn cursor_atomic_event_pending_for_output(&self, output_idx: usize) -> bool {
        let Some(layout) = self.outputs.get(output_idx) else {
            return false;
        };
        self.device_for_key(layout.key.device_key)
            .is_some_and(|device| {
                device
                    .cursor
                    .atomic_cursor_commits_in_flight
                    .contains_key(&layout.output.crtc)
            })
    }

    pub(crate) fn cursor_retry_waits_for_flip(&self, output_idx: usize) -> bool {
        let Some(layout) = self.outputs.get(output_idx) else {
            return false;
        };
        self.device_for_key(layout.key.device_key)
            .is_some_and(|device| {
                device
                    .cursor
                    .cursor_retry_waits_for_flip
                    .contains(&layout.output.crtc)
            })
    }

    /// Whether the scene's output gate must wait for an accepted cursor-only
    /// commit on this output.
    pub(crate) fn cursor_commit_in_flight_for_output(&self, output_idx: usize) -> bool {
        let Some(layout) = self.outputs.get(output_idx) else {
            return false;
        };
        self.device_for_key(layout.key.device_key)
            .is_some_and(|device| {
                device
                    .cursor
                    .atomic_cursor_commits_in_flight
                    .contains_key(&layout.output.crtc)
            })
    }

    /// Whether any CRTC in a grouped transaction is still owned by a
    /// cursor-only commit. Direct scanout and composed unflip submit one
    /// atomic request for every listed output, so all of them must be clear.
    pub(crate) fn cursor_commit_in_flight_for_outputs(&self, output_indices: &[usize]) -> bool {
        output_indices
            .iter()
            .any(|output_idx| self.cursor_commit_in_flight_for_output(*output_idx))
    }

    pub(crate) fn any_cursor_commit_in_flight(&self) -> bool {
        self.devices
            .iter()
            .any(|device| !device.cursor.atomic_cursor_commits_in_flight.is_empty())
    }

    /// Submit the pending move for one output. The caller is the scheduling
    /// pass, after direct and scene work have had their turn. A rejected ioctl
    /// leaves the position unsent and creates no in-flight record.
    pub(crate) fn submit_atomic_cursor_move_for_output(
        &mut self,
        output_idx: usize,
        now: Instant,
        live_direct_vrr_wanted: bool,
    ) -> io::Result<CursorMoveOutcome> {
        use ::drm::control::AtomicCommitFlags;

        let Some(layout) = self.outputs.get(output_idx) else {
            return Ok(CursorMoveOutcome::default());
        };
        let device_key = layout.key.device_key;
        let crtc = layout.output.crtc;
        let vrr_cursor_hold = self.vrr_cursor_hold_applies(output_idx, live_direct_vrr_wanted);
        let Some(device_idx) = self
            .devices
            .iter()
            .position(|device| device.key == device_key)
        else {
            return Ok(CursorMoveOutcome::default());
        };
        let device = Rc::clone(&self.devices[device_idx].device);
        let commit_sink = Rc::clone(&self.devices[device_idx].atomic_commit_sink);
        let cursor = &mut self.devices[device_idx].cursor;
        if !cursor.atomic_cursor
            || cursor.atomic_cursor_commits_in_flight.contains_key(&crtc)
            || cursor.cursor_retry_waits_for_flip.contains(&crtc)
            || cursor.cursor_hide_recovery.contains_key(&crtc)
            || !cursor.available_on(crtc)
            || !cursor.hw_bound_on(crtc)
        {
            return Ok(CursorMoveOutcome::default());
        }
        let Some(pending) = cursor.atomic_pending_moves.get_mut(&crtc) else {
            return Ok(CursorMoveOutcome::default());
        };
        if vrr_cursor_hold {
            let deadline = *pending
                .deadline
                .get_or_insert_with(|| now + VRR_CURSOR_DEADLINE);
            if now < deadline {
                return Ok(CursorMoveOutcome::default());
            }
        }
        let position = pending.position;
        let mut request = crate::drm::atomic::AtomicCommitRequest::new();
        request.add_cursor_position(position);
        let event = crate::drm::atomic::AtomicCommitEvent {
            crtc,
            motion_timestamp: position.motion_timestamp,
        };
        let result = commit_sink
            .submit_with_commit_id(
                &device,
                AtomicCommitFlags::PAGE_FLIP_EVENT | AtomicCommitFlags::NONBLOCK,
                crate::drm::atomic::AtomicCommitKind::CursorOnly,
                request,
                &[event],
            )
            .map(Some);
        let commit_id = match result {
            Ok(commit_id) => commit_id,
            Err(error) => {
                let cursor_rejected = commit_sink.take_last_cursor_rejection();
                if error.raw_os_error() == Some(libc::EBUSY) {
                    self.devices[device_idx]
                        .cursor
                        .cursor_retry_waits_for_flip
                        .insert(crtc);
                    return Ok(CursorMoveOutcome {
                        ebusy_count: 1,
                        ..CursorMoveOutcome::default()
                    });
                }
                if !cursor_rejected {
                    return Err(error);
                }
                let position = [position];
                let outcome = self.reject_atomic_cursor_commit(&position, &error.to_string());
                return Ok(outcome);
            }
        };

        if vrr_cursor_hold {
            commit_sink.note_vrr_cursor_deadline_commit(crtc);
        }

        let cursor = &mut self.devices[device_idx].cursor;
        if cursor
            .atomic_pending_moves
            .get(&crtc)
            .is_some_and(|pending| pending.position == position)
        {
            cursor.atomic_pending_moves.remove(&crtc);
        }
        if let Some(latest) = cursor.atomic_positions.get_mut(&crtc)
            && *latest == position
        {
            latest.motion_timestamp = None;
        }
        let matches_latest = cursor
            .atomic_positions
            .get(&crtc)
            .is_some_and(|latest| latest.x == position.x && latest.y == position.y);
        if matches_latest
            && let Some(applied) = cursor
                .crtc_states
                .get_mut(&crtc)
                .and_then(|state| state.applied.as_mut())
            && applied.properties.plane == position.plane
        {
            applied.x = position.x;
            applied.y = position.y;
        }
        cursor.atomic_cursor_commits_in_flight.insert(
            crtc,
            crate::kms::render::platform::cursor_state::InFlightCursorCommit {
                commit_id,
                prepared: None,
            },
        );
        Ok(CursorMoveOutcome::default())
    }

    /// Attribute a flip event to this CRTC's cursor-only commit, if present.
    /// The caller still records general scene-commit latency independently.
    pub(crate) fn retire_atomic_cursor_commit_for_output(
        &mut self,
        output_idx: usize,
        event_id: Option<u64>,
    ) -> bool {
        let Some(layout) = self.outputs.get(output_idx) else {
            return false;
        };
        let device_key = layout.key.device_key;
        let crtc = layout.output.crtc;
        let Some(device) = self
            .devices
            .iter_mut()
            .find(|device| device.key == device_key)
        else {
            return false;
        };
        let should_retire = device
            .cursor
            .atomic_cursor_commits_in_flight
            .get(&crtc)
            .is_some_and(|commit| commit.commit_id.is_none() || commit.commit_id == event_id);
        let removed = should_retire
            .then(|| device.cursor.atomic_cursor_commits_in_flight.remove(&crtc))
            .flatten();
        if let Some(prepared) = removed.as_ref().and_then(|commit| commit.prepared.as_ref())
            && let Some(prior) = prepared.prior
            && prepared.next.is_none_or(|next| prior.lease != next.lease)
            && let Some(plane) = device.cursor.plane.as_mut()
            && !plane.sprites_mut().release_user(prior.lease)
        {
            log::error!("render cursor: retired commit had no user for its prior sprite lease");
        }
        let retired = removed.is_some();
        let retry_reenabled = device.cursor.cursor_retry_waits_for_flip.remove(&crtc);
        retired || retry_reenabled
    }

    /// Apply the rollback rule to every cursor CRTC in a rejected scene
    /// transaction. Atomic devices detach their applied plane binding, then
    /// discard the rejected position.
    pub(crate) fn reject_atomic_cursor_commit(
        &mut self,
        positions: &[crate::drm::atomic::CursorPlanePosition],
        error: &str,
    ) -> CursorMoveOutcome {
        let mut outcome = CursorMoveOutcome::default();
        for position in positions {
            let Some(device_idx) = self
                .devices
                .iter()
                .position(|device| device.key == position.device_key)
            else {
                continue;
            };
            let crtc = position.crtc;
            let atomic_path = self.devices[device_idx].cursor.state_path()
                == crate::kms::cursor_plane::topology::CursorStatePath::Atomic;
            {
                let cursor = &mut self.devices[device_idx].cursor;
                cursor.clear_atomic_position(crtc);
                cursor.cursor_retry_waits_for_flip.remove(&crtc);
            }
            if !atomic_path {
                continue;
            }
            let rollback_error = io::Error::other(error.to_owned());
            match self.atomic_rollback(device_idx, crtc, &rollback_error, false) {
                Ok(()) => outcome.fallback_changed = true,
                Err(_) => outcome.retry_required = true,
            }
            outcome
                .affected_crtcs
                .insert(crate::kms::render::platform::CrtcKey {
                    device_key: position.device_key,
                    crtc,
                });
        }
        outcome
    }

    pub(crate) fn defer_cursor_move_outcome(&mut self, outcome: CursorMoveOutcome) {
        self.deferred_cursor_move_outcome.merge(outcome);
    }

    pub(crate) fn take_deferred_cursor_move_outcome(&mut self) -> CursorMoveOutcome {
        std::mem::take(&mut self.deferred_cursor_move_outcome)
    }

    /// Drop atomic per-CRTC state only after the old-event drain has proved
    /// that no cursor-only flip can be attributed to the next topology epoch.
    pub(crate) fn clear_atomic_cursor_state_after_old_events_discard(&mut self) {
        for device_idx in 0..self.devices.len() {
            let discarded_priors: Vec<_> = self.devices[device_idx]
                .cursor
                .atomic_cursor_commits_in_flight
                .values()
                .filter_map(|commit| commit.prepared.as_ref())
                .filter(|prepared| {
                    prepared.prior.is_some_and(|prior| {
                        prepared.next.is_none_or(|next| next.lease != prior.lease)
                    })
                })
                .map(|prepared| prepared.prior)
                .collect();
            for prior in discarded_priors {
                self.release_cursor_bindings_once(device_idx, [prior], "discarded old event");
            }
            let cursor = &mut self.devices[device_idx].cursor;
            cursor.atomic_positions.clear();
            cursor.atomic_pending_moves.clear();
            cursor.atomic_cursor_commits_in_flight.clear();
            cursor.cursor_retry_waits_for_flip.clear();
            cursor.cursor_hide_recovery.clear();
            cursor.transient_fallback_crtcs.clear();
        }
    }

    /// Clear only the unsent values carried by an accepted scene commit. The
    /// latest positions stay cached so subsequent scene commits continue to
    /// include the cursor plane state.
    pub(crate) fn cursor_plane_scene_commit_submitted(
        &mut self,
        positions: &[crate::drm::atomic::CursorPlanePosition],
    ) {
        for position in positions {
            for device in &mut self.devices {
                if device.key != position.device_key {
                    continue;
                }
                let matches_latest = device
                    .cursor
                    .atomic_positions
                    .get(&position.crtc)
                    .is_some_and(|latest| latest == position);
                if !matches_latest {
                    continue;
                }
                if let Some(applied) = device
                    .cursor
                    .crtc_states
                    .get_mut(&position.crtc)
                    .and_then(|state| state.applied.as_mut())
                    && applied.properties.plane == position.plane
                {
                    applied.x = position.x;
                    applied.y = position.y;
                }
                if device
                    .cursor
                    .atomic_pending_moves
                    .get(&position.crtc)
                    .is_some_and(|unsent| unsent.position == *position)
                {
                    device.cursor.atomic_pending_moves.remove(&position.crtc);
                }
                if let Some(latest) = device.cursor.atomic_positions.get_mut(&position.crtc) {
                    latest.motion_timestamp = None;
                }
            }
        }
    }

    /// Seed one already-visible cursor position without issuing a DRM ioctl.
    /// Tests use the normal output-origin conversion and production cache;
    /// production never calls this helper.
    #[cfg(test)]
    pub(crate) fn for_tests_seed_atomic_cursor_position(
        &mut self,
        output_idx: usize,
        plane_handle: ::drm::control::plane::Handle,
        crtc_x: ::drm::control::property::Handle,
        crtc_y: ::drm::control::property::Handle,
        root_x: i32,
        root_y: i32,
        hot_x: u16,
        hot_y: u16,
    ) -> crate::drm::atomic::CursorPlanePosition {
        let layout = &self.outputs[output_idx];
        let device_key = layout.key.device_key;
        let crtc = layout.output.crtc;
        let (crtc_x_local, crtc_y_local) =
            cursor_root_to_crtc_local(root_x, root_y, layout.x, layout.y, hot_x, hot_y);
        let device = self
            .devices
            .iter_mut()
            .find(|device| device.key == device_key)
            .expect("test output has an owning KMS device");
        if device.cursor.plane.is_none() {
            device.cursor.plane = Some(crate::kms::cursor_plane::CursorPlane::for_tests_stub(
                64, 64,
            ));
        }
        let plane = device
            .cursor
            .plane
            .as_mut()
            .expect("test cursor plane was installed");
        plane.for_tests_set_atomic_position_properties(crtc, plane_handle, crtc_x, crtc_y);
        let properties = plane
            .properties_for(crtc)
            .expect("test cursor plane has atomic properties");
        device.cursor.crtc_states.insert(
            crtc,
            crate::kms::render::platform::cursor_state::CursorCrtcState {
                applied: Some(crate::kms::render::platform::cursor_state::CursorBinding {
                    properties,
                    lease: crate::kms::cursor_plane::sprites::SpriteBufferLease::for_tests(
                        1, hot_x, hot_y,
                    ),
                    framebuffer: 1,
                    x: crtc_x_local,
                    y: crtc_y_local,
                    hot_x,
                    hot_y,
                }),
                valid: true,
                resume_visible: true,
            },
        );
        device.cursor.atomic_cursor = true;
        device.cursor.remember_atomic_position(
            device_key,
            crtc,
            crtc_x_local,
            crtc_y_local,
            Some(Instant::now()),
        );
        device
            .cursor
            .atomic_positions
            .get(&crtc)
            .copied()
            .expect("test cursor plane exposes atomic position properties")
    }

    #[cfg(test)]
    pub(crate) fn for_tests_atomic_position_is_unsent(
        &self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) -> bool {
        self.devices
            .iter()
            .find(|device| device.key == device_key)
            .is_some_and(|device| device.cursor.atomic_pending_moves.contains_key(&crtc))
    }

    #[cfg(test)]
    pub(crate) fn for_tests_atomic_cursor_commit_is_in_flight(
        &self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) -> bool {
        self.devices
            .iter()
            .find(|device| device.key == device_key)
            .is_some_and(|device| {
                device
                    .cursor
                    .atomic_cursor_commits_in_flight
                    .contains_key(&crtc)
            })
    }

    #[cfg(test)]
    pub(crate) fn for_tests_atomic_cursor_prior_lease(
        &self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) -> Option<crate::kms::cursor_plane::sprites::SpriteBufferLease> {
        self.devices
            .iter()
            .find(|device| device.key == device_key)?
            .cursor
            .atomic_cursor_commits_in_flight
            .get(&crtc)?
            .prepared
            .as_ref()?
            .prior
            .map(|binding| binding.lease)
    }

    #[cfg(test)]
    pub(crate) fn for_tests_atomic_cursor_pending_position(
        &self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) -> Option<crate::drm::atomic::CursorPlanePosition> {
        self.devices
            .iter()
            .find(|device| device.key == device_key)
            .and_then(|device| device.cursor.atomic_pending_moves.get(&crtc).copied())
            .map(|pending| pending.position)
    }

    #[cfg(test)]
    pub(crate) fn for_tests_cursor_hide_recovery_attempts(
        &self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) -> Option<u8> {
        self.devices
            .iter()
            .find(|device| device.key == device_key)
            .and_then(|device| device.cursor.cursor_hide_recovery.get(&crtc))
            .map(|recovery| recovery.attempts)
    }

    #[cfg(test)]
    pub(crate) fn for_tests_cursor_permanently_disabled(
        &self,
        device_key: crate::platform::drm::DrmDeviceKey,
    ) -> bool {
        self.devices
            .iter()
            .find(|device| device.key == device_key)
            .is_some_and(|device| device.cursor.permanently_disabled)
    }

    #[cfg(test)]
    pub(crate) fn for_tests_cursor_crtc_permanently_disabled(
        &self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) -> bool {
        self.devices
            .iter()
            .find(|device| device.key == device_key)
            .is_some_and(|device| device.cursor.permanently_disabled_crtcs.contains(&crtc))
    }

    #[cfg(test)]
    pub(crate) fn for_tests_cursor_fallback_remaining(
        &self,
        device_key: crate::platform::drm::DrmDeviceKey,
        crtc: ::drm::control::crtc::Handle,
    ) -> Option<u8> {
        self.devices
            .iter()
            .find(|device| device.key == device_key)
            .and_then(|device| device.cursor.transient_fallback_crtcs.get(&crtc))
            .map(|fallback| fallback.remaining_sw_retires)
    }

    #[must_use]
    pub(crate) fn cursor_plane_fits_for_output(
        &self,
        output_idx: usize,
        width: u32,
        height: u32,
    ) -> bool {
        let Some(layout) = self.outputs.get(output_idx) else {
            return false;
        };
        let Some(device) = self.device_for_key(layout.key.device_key) else {
            return false;
        };
        device.cursor.available_on(layout.output.crtc)
            && device.cursor.plane.as_ref().is_some_and(|plane| {
                cursor_dimensions_fit(plane.width(), plane.height(), width, height)
            })
    }

    pub(super) fn cursor_output_route(
        &self,
        output_idx: usize,
    ) -> io::Result<(usize, ::drm::control::crtc::Handle, i32, i32)> {
        let layout = self
            .outputs
            .get(output_idx)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such output"))?;
        let device_idx = self
            .devices
            .iter()
            .position(|device| device.key == layout.key.device_key)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "no DRM device {} for output {output_idx}",
                        layout.key.device_key
                    ),
                )
            })?;
        Ok((device_idx, layout.output.crtc, layout.x, layout.y))
    }

    /// A new sprite or hotspot invalidates EINVAL observations made against a
    /// previous parameter set. This is deliberately a global sprite fanout,
    /// but every device clears only its own CRTC retry records.
    pub(crate) fn cursor_plane_note_sprite_hotspot(
        &mut self,
        width: u16,
        height: u16,
        hot_x: u16,
        hot_y: u16,
    ) {
        let signature = (width, height, hot_x, hot_y);
        for device in &mut self.devices {
            if device.cursor.sprite_signature != Some(signature) {
                device.cursor.sprite_signature = Some(signature);
                device.cursor.transient_fallback_crtcs.clear();
            }
        }
    }

    pub(crate) fn cursor_plane_upload_image_for_output(
        &mut self,
        output_idx: usize,
        version: u64,
        hot_x: u16,
        hot_y: u16,
        width: u32,
        height: u32,
        bgra_bytes: &[u8],
    ) -> io::Result<crate::kms::render::scene::CursorUploadOutcome> {
        let (device_idx, crtc, _, _) = self.cursor_output_route(output_idx)?;
        if !self.devices[device_idx].cursor.available_on(crtc) {
            return Err(io::Error::other("cursor plane unavailable for output"));
        }
        let device = Rc::clone(&self.devices[device_idx].device);
        let result = {
            let plane = self.devices[device_idx]
                .cursor
                .plane
                .as_mut()
                .expect("available cursor state has a plane");
            let buffer_io = plane.buffer_io();
            plane.sprites_mut().prepare(
                device, buffer_io, version, hot_x, hot_y, width, height, bgra_bytes,
            )
        };
        match result {
            Ok(crate::kms::cursor_plane::sprites::SpriteStage::Ready) => {
                Ok(crate::kms::render::scene::CursorUploadOutcome::Ready)
            }
            Ok(crate::kms::cursor_plane::sprites::SpriteStage::Deferred) => {
                Ok(crate::kms::render::scene::CursorUploadOutcome::Deferred)
            }
            Err(error) => {
                apply_cursor_operation_error(&mut self.devices[device_idx].cursor, crtc, &error);
                Err(error)
            }
        }
    }

    #[must_use]
    pub(crate) fn cursor_plane_uploaded_image_matches_for_output(
        &self,
        output_idx: usize,
        version: u64,
        hot_x: u16,
        hot_y: u16,
    ) -> bool {
        let Ok((device_idx, _, _, _)) = self.cursor_output_route(output_idx) else {
            return false;
        };
        let cursor = &self.devices[device_idx].cursor;
        cursor.plane.as_ref().is_some_and(|plane| {
            plane
                .sprites()
                .candidate_lease()
                .is_some_and(|lease| lease.matches(version, hot_x, hot_y))
        })
    }

    /// Count a failed non-EINVAL direct cursor refresh and arm its next retry.
    /// The existing per-CRTC hide-recovery slot also gates position-only
    /// commits while the current show/hide intent is being retried.
    pub(crate) fn note_direct_cursor_retry_failure(
        &mut self,
        output_idx: usize,
        retry_deadline: Instant,
    ) -> Option<u8> {
        let (device_idx, crtc, _, _) = self.cursor_output_route(output_idx).ok()?;
        let recovery = self.devices[device_idx]
            .cursor
            .cursor_hide_recovery
            .entry(crtc)
            .or_default();
        if !recovery.direct_scanout_retry {
            recovery.attempts = 0;
        }
        recovery.direct_scanout_retry = true;
        recovery.attempts = recovery.attempts.saturating_add(1);
        recovery.retry_deadline = Some(retry_deadline);
        Some(recovery.attempts)
    }

    /// Deadline for a direct cursor retry on this output, if its last failure
    /// was a generic atomic error rather than an event-gated EBUSY.
    #[must_use]
    pub(crate) fn direct_cursor_retry_deadline(&self, output_idx: usize) -> Option<Instant> {
        let (device_idx, crtc, _, _) = self.cursor_output_route(output_idx).ok()?;
        self.devices[device_idx]
            .cursor
            .cursor_hide_recovery
            .get(&crtc)
            .filter(|recovery| recovery.direct_scanout_retry)
            .and_then(|recovery| recovery.retry_deadline)
    }

    /// Clear only direct-scanout retry bookkeeping; a separate failed
    /// blocking-detach recovery still belongs to pointer-motion handling.
    pub(crate) fn clear_direct_cursor_retry(&mut self, output_idx: usize) {
        let Ok((device_idx, crtc, _, _)) = self.cursor_output_route(output_idx) else {
            return;
        };
        let cursor = &mut self.devices[device_idx].cursor;
        if cursor
            .cursor_hide_recovery
            .get(&crtc)
            .is_some_and(|recovery| recovery.direct_scanout_retry)
        {
            cursor.cursor_hide_recovery.remove(&crtc);
        }
    }

    /// Bind the atomic cursor plane on `output_idx` at `(x, y)` in root space.
    ///
    /// # Errors
    /// Cursor commit failure; `NotFound` if `output_idx` is out of range or
    /// the atomic cursor plane is unavailable.
    pub(crate) fn cursor_plane_show_on_crtc(
        &mut self,
        output_idx: usize,
        hot_x: u16,
        hot_y: u16,
        x: i32,
        y: i32,
    ) -> Result<(), crate::kms::cursor_plane::CursorShowError> {
        if self.latest_root_pointer.is_none() {
            self.note_root_pointer_position(x, y);
        }
        self.atomic_show(output_idx, hot_x, hot_y, x, y)
    }

    /// Record the latest pointer position for each visible atomic cursor.
    /// Hidden CRTCs are skipped; the scene handles cross-CRTC show and hide.
    ///
    /// # Errors
    /// `Err` only when no atomic cursor plane is available.
    pub(crate) fn cursor_plane_move(
        &mut self,
        x: i32,
        y: i32,
        hot_x: u16,
        hot_y: u16,
    ) -> io::Result<CursorMoveOutcome> {
        let motion_timestamp = Instant::now();
        let mut aggregate = CursorMoveOutcome::default();
        let mut found = false;
        for device_idx in 0..self.devices.len() {
            if !self.devices[device_idx].cursor.atomic_cursor
                || self.devices[device_idx].cursor.plane.is_none()
            {
                continue;
            }
            found = true;
            let outcome = self.try_cursor_plane_move_for_device(
                device_idx,
                x,
                y,
                hot_x,
                hot_y,
                motion_timestamp,
            )?;
            aggregate.merge(outcome);
        }
        for device in &self.devices {
            device.atomic_commit_sink.emit_telemetry_if_due();
        }
        found
            .then_some(aggregate)
            .ok_or_else(|| io::Error::other("cursor plane unavailable"))
    }

    fn apply_cursor_motion_hide_recovery_result(
        &mut self,
        device_idx: usize,
        crtc: ::drm::control::crtc::Handle,
        recovery: CursorHideRecovery,
        result: io::Result<()>,
        operation: &str,
    ) -> CursorMoveOutcome {
        let device_key = self.devices[device_idx].key;
        let mut outcome = CursorMoveOutcome::default();
        match result {
            Ok(()) => {
                let state = &mut self.devices[device_idx].cursor;
                state.cursor_hide_recovery.remove(&crtc);
                state.clear_atomic_position(crtc);
                state.note_einval(crtc);
                outcome.fallback_changed = true;
                log::warn!(
                    "render cursor hide recovery: device {device_key} CRTC {crtc:?} {operation} succeeded on pointer motion; entering transient software fallback"
                );
            }
            Err(error) => {
                let attempts = recovery.attempts.saturating_add(1);
                let state = &mut self.devices[device_idx].cursor;
                state.cursor_hide_recovery.insert(
                    crtc,
                    CursorHideRecovery {
                        attempts,
                        ..CursorHideRecovery::default()
                    },
                );
                outcome.retry_required = true;
                if attempts >= crate::kms::render::platform::CURSOR_HIDE_RECOVERY_MAX_ATTEMPTS {
                    state.permanently_disabled_crtcs.insert(crtc);
                    state.clear_atomic_position(crtc);
                    state.cursor_retry_waits_for_flip.remove(&crtc);
                    state.cursor_hide_recovery.remove(&crtc);
                    outcome.fallback_changed = true;
                    log::error!(
                        "render cursor hide recovery: device {device_key} CRTC {crtc:?} {operation} failed {attempts} times ({error}); permanently disabling the hardware cursor on this CRTC"
                    );
                } else {
                    log::warn!(
                        "render cursor hide recovery: device {device_key} CRTC {crtc:?} {operation} retry {attempts}/{max_attempts} failed ({error})",
                        max_attempts =
                            crate::kms::render::platform::CURSOR_HIDE_RECOVERY_MAX_ATTEMPTS,
                    );
                }
            }
        }
        outcome
            .affected_crtcs
            .insert(crate::kms::render::platform::CrtcKey { device_key, crtc });
        outcome
    }

    /// Record the latest atomic position on each currently bound CRTC.
    fn try_cursor_plane_move_for_device(
        &mut self,
        device_idx: usize,
        x: i32,
        y: i32,
        hot_x: u16,
        hot_y: u16,
        motion_timestamp: Instant,
    ) -> io::Result<CursorMoveOutcome> {
        let device_key = self.devices[device_idx].key;
        let layouts: Vec<(::drm::control::crtc::Handle, i32, i32)> = self
            .outputs
            .iter()
            .filter(|layout| layout.key.device_key == device_key)
            .map(|l| (l.output.crtc, l.x, l.y))
            .collect();
        if self.devices[device_idx].cursor.plane.is_none() {
            return Err(io::Error::other("cursor plane unavailable"));
        }
        if self.devices[device_idx].cursor.permanently_disabled {
            return Ok(CursorMoveOutcome::default());
        }
        let mut outcome = CursorMoveOutcome::default();
        for (crtc, layout_x, layout_y) in layouts {
            if self.devices[device_idx]
                .cursor
                .permanently_disabled_crtcs
                .contains(&crtc)
            {
                continue;
            }
            if !self.devices[device_idx].cursor.hw_bound_on(crtc) {
                continue;
            }
            let recovery = self.devices[device_idx]
                .cursor
                .cursor_hide_recovery
                .get(&crtc)
                .copied();
            if let Some(recovery) = recovery {
                if recovery.direct_scanout_retry {
                    continue;
                }
                let result = self.detach_cursor_blocking(device_idx, crtc);
                outcome.merge(self.apply_cursor_motion_hide_recovery_result(
                    device_idx,
                    crtc,
                    recovery,
                    result,
                    "atomic detach",
                ));
                continue;
            }

            let state = &mut self.devices[device_idx].cursor;
            let (position_hot_x, position_hot_y) = state
                .crtc_states
                .get(&crtc)
                .and_then(|crtc_state| crtc_state.applied)
                .map(|applied| (applied.hot_x, applied.hot_y))
                .unwrap_or((hot_x, hot_y));
            let (cx, cy) =
                cursor_root_to_crtc_local(x, y, layout_x, layout_y, position_hot_x, position_hot_y);
            state.remember_atomic_position(device_key, crtc, cx, cy, Some(motion_timestamp));
            if state
                .crtc_states
                .get(&crtc)
                .and_then(|crtc_state| crtc_state.applied)
                .is_some_and(|applied| applied.x == cx && applied.y == cy)
            {
                state.atomic_pending_moves.remove(&crtc);
            }
        }
        Ok(outcome)
    }

    /// True iff the set of CRTCs whose region the cursor footprint
    /// intersects differs from the set the plane is currently bound on
    /// (`hw_bound_on`).
    ///
    /// The pointer fast path (`cursor_plane_move`) only *repositions*
    /// the cursor on already-bound CRTCs — it never shows the plane on
    /// a CRTC the pointer newly crosses into, nor hides it on one it
    /// leaves. Cross-CRTC show/hide is decided by the scene's
    /// `CursorAssignment` during compose. While an idle desktop
    /// composited every frame (pre-#30) that reassignment happened for
    /// free; now that idle desktops stop compositing, the fast path
    /// must detect a boundary crossing and route it through one compose
    /// tick. This predicate is that detector, using the same footprint
    /// intersection rule as `cursor_footprint_rect` so its membership
    /// decision matches the scene's exactly.
    ///
    /// `(x, y)` is the root-space cursor position, `(hot_x, hot_y)` the
    /// sprite hotspot, `(cw, ch)` the sprite extent.
    pub(crate) fn cursor_crtc_membership_dirty(
        &self,
        x: i32,
        y: i32,
        hot_x: u16,
        hot_y: u16,
        cw: i32,
        ch: i32,
        ignored_output_indices: &[usize],
    ) -> bool {
        for (output_idx, l) in self.outputs.iter().enumerate() {
            if ignored_output_indices.contains(&output_idx) {
                continue;
            }
            let Some(device) = self.device_for_key(l.key.device_key) else {
                continue;
            };
            if device.cursor.state_path()
                != crate::kms::cursor_plane::topology::CursorStatePath::Atomic
            {
                continue;
            }
            let dx = x - i32::from(hot_x) - l.x;
            let dy = y - i32::from(hot_y) - l.y;
            let intersects = cursor_footprint_intersects_output(
                dx,
                dy,
                cw,
                ch,
                i32::from(l.width),
                i32::from(l.height),
            );
            if intersects != device.cursor.hw_bound_on(l.output.crtc) {
                return true;
            }
        }
        false
    }

    /// Detach the atomic plane on a single CRTC. Output-local recovery uses
    /// this before the software cursor takes ownership.
    ///
    /// # Errors
    /// `NotFound` if `output_idx` is out of range or plane is unavailable.
    pub(crate) fn cursor_plane_hide_on_crtc(&mut self, output_idx: usize) -> io::Result<()> {
        let (device_idx, _, _, _) = self.cursor_output_route(output_idx)?;
        if self.devices[device_idx].cursor.state_path()
            == crate::kms::cursor_plane::topology::CursorStatePath::Atomic
        {
            self.atomic_hide(output_idx)
        } else {
            Ok(())
        }
    }

    /// Kernel-side visibility for this output's device-qualified CRTC. Scene
    /// bookkeeping can temporarily lag after an ioctl/rollback failure, so a
    /// compose tick must consult this before deciding it is safe to draw SW.
    #[must_use]
    pub(crate) fn cursor_plane_visible_for_output(&self, output_idx: usize) -> bool {
        let Ok((device_idx, crtc, _, _)) = self.cursor_output_route(output_idx) else {
            return false;
        };
        self.devices[device_idx].cursor.hw_bound_on(crtc)
    }

    /// Physical cursor-plane state across the current output inventory.
    #[must_use]
    pub(crate) fn any_cursor_plane_visible(&self) -> bool {
        (0..self.outputs.len()).any(|output_idx| self.cursor_plane_visible_for_output(output_idx))
    }

    /// Advance only this output's EINVAL retry budget after its own successful
    /// software-composed retirement. Returns true when another repaint is
    /// needed either to consume more backoff or to perform the now-eligible HW
    /// retry. A retirement on another card cannot touch this record.
    pub(crate) fn cursor_plane_note_composed_retirement(&mut self, output_idx: usize) -> bool {
        let Ok((device_idx, crtc, _, _)) = self.cursor_output_route(output_idx) else {
            return false;
        };
        let Some(retry) = self.devices[device_idx]
            .cursor
            .transient_fallback_crtcs
            .get_mut(&crtc)
        else {
            return false;
        };
        if retry.remaining_sw_retires > 0 {
            retry.remaining_sw_retires -= 1;
            return true;
        }
        false
    }

    /// Revalidate every existing per-device cursor plane after an active CRTC
    /// topology change. A genuinely headless-deferred device is deliberately
    /// skipped here: only the post-success explicit-enable hook may make its
    /// first attempt. A prior transient attempt is retried at this later
    /// topology/resume boundary.
    pub(super) fn refresh_cursor_topology_for_devices(
        &mut self,
        changed_devices: &HashSet<crate::platform::drm::DrmDeviceKey>,
    ) {
        self.refresh_cursor_topology_for_devices_with(
            changed_devices,
            initialize_cursor_plane_for_device,
        );
    }

    pub(super) fn refresh_cursor_topology_for_devices_with<F>(
        &mut self,
        changed_devices: &HashSet<crate::platform::drm::DrmDeviceKey>,
        factory: F,
    ) where
        F: FnMut(&mut KmsDevice, &[::drm::control::crtc::Handle], &str),
    {
        self.refresh_cursor_topology_scoped_with(changed_devices, None, factory);
    }

    /// [`Self::refresh_cursor_topology_for_devices_with`]; with `only_crtc`
    /// set, per-CRTC cursor state is dropped for that CRTC alone, so the
    /// device's other live CRTCs keep their queued moves. A per-CRTC RANDR
    /// change uses this; a global quiesce drops every CRTC's state.
    pub(super) fn refresh_cursor_topology_scoped_with<F>(
        &mut self,
        changed_devices: &HashSet<crate::platform::drm::DrmDeviceKey>,
        only_crtc: Option<::drm::control::crtc::Handle>,
        mut factory: F,
    ) where
        F: FnMut(&mut KmsDevice, &[::drm::control::crtc::Handle], &str),
    {
        let mut crtcs_by_device: HashMap<_, Vec<_>> = HashMap::new();
        for output in &self.outputs {
            crtcs_by_device
                .entry(output.key.device_key)
                .or_default()
                .push(output.output.crtc);
        }
        for device in &mut self.devices {
            if !changed_devices.contains(&device.key) {
                continue;
            }
            let atomic = device.cursor.state_path()
                == crate::kms::cursor_plane::topology::CursorStatePath::Atomic;
            if let Some(crtc) = only_crtc {
                device.cursor.clear_atomic_position(crtc);
                if !atomic
                    || device
                        .cursor
                        .atomic_cursor_commits_in_flight
                        .get(&crtc)
                        .is_some_and(|commit| commit.commit_id.is_none())
                {
                    device.cursor.atomic_cursor_commits_in_flight.remove(&crtc);
                }
                device.cursor.cursor_retry_waits_for_flip.remove(&crtc);
                device.cursor.cursor_hide_recovery.remove(&crtc);
                device.cursor.transient_fallback_crtcs.remove(&crtc);
            } else {
                device.cursor.atomic_positions.clear();
                device.cursor.atomic_pending_moves.clear();
                if atomic {
                    device
                        .cursor
                        .atomic_cursor_commits_in_flight
                        .retain(|_, commit| commit.commit_id.is_some());
                } else {
                    device.cursor.atomic_cursor_commits_in_flight.clear();
                }
                device.cursor.cursor_retry_waits_for_flip.clear();
                device.cursor.cursor_hide_recovery.clear();
                device.cursor.transient_fallback_crtcs.clear();
            }
            let crtcs = crtcs_by_device.remove(&device.key).unwrap_or_default();
            if device.cursor.should_retry_initialization(!crtcs.is_empty()) {
                factory(device, &crtcs, "lifecycle retry");
            }
            if let Some(plane) = device.cursor.plane.as_mut() {
                plane.sprites_mut().set_active_crtc_count(crtcs.len());
            }
            let blocked = device
                .cursor
                .plane
                .as_ref()
                .is_some_and(|plane| !plane.supports_crtcs(&crtcs));
            device.cursor.atomic_cursor = device.cursor.state_path()
                == crate::kms::cursor_plane::topology::CursorStatePath::Atomic
                && device
                    .cursor
                    .plane
                    .as_ref()
                    .is_some_and(|plane| plane.supports_atomic_positions(&crtcs));
            if blocked != device.cursor.topology_blocked {
                log::warn!(
                    "render cursor: device {} topology_blocked {} -> {} for {} active CRTC(s)",
                    device.key,
                    device.cursor.topology_blocked,
                    blocked,
                    crtcs.len()
                );
            }
            device.cursor.topology_blocked = blocked;
        }
    }

    #[cfg(test)]
    pub(crate) fn for_tests_install_cursor_plane_from_snapshot(
        &mut self,
        device_idx: usize,
        snapshot: crate::kms::cursor_plane::topology::CursorPlaneTestSnapshot,
        buffer_io: Rc<dyn crate::kms::cursor_plane::ops::CursorBufferIo>,
        width: u32,
        height: u32,
    ) {
        let device = Rc::clone(
            &self
                .devices
                .get(device_idx)
                .expect("test cursor device index")
                .device,
        );
        let device_key = self.devices[device_idx].key;
        let crtcs: Vec<_> = self
            .outputs
            .iter()
            .filter(|output| output.key.device_key == device_key)
            .map(|output| output.output.crtc)
            .collect();
        let plane = crate::kms::cursor_plane::CursorPlane::for_tests_from_snapshot(
            device, &crtcs, buffer_io, snapshot, width, height,
        );
        install_cursor_plane_for_device(
            &mut self.devices[device_idx],
            &crtcs,
            "test snapshot",
            plane,
        );
    }

    /// Run the one-time first-output factory for an opened device that began
    /// genuinely headless. Callers must invoke this only after the successful
    /// ActiveOutput insertion. All post-insertion active CRTCs on the owning
    /// device are passed to the factory, and no other card is touched.
    pub(super) fn initialize_headless_cursor_for_device_with<F>(
        &mut self,
        device_key: crate::platform::drm::DrmDeviceKey,
        boundary: &str,
        factory: F,
    ) -> bool
    where
        F: FnOnce(&mut KmsDevice, &[::drm::control::crtc::Handle], &str),
    {
        let crtcs: Vec<_> = self
            .outputs
            .iter()
            .filter(|output| output.key.device_key == device_key)
            .map(|output| output.output.crtc)
            .collect();
        let Some(device) = self
            .devices
            .iter_mut()
            .find(|device| device.key == device_key)
        else {
            return false;
        };
        if !device
            .cursor
            .should_initialize_headless_deferred(!crtcs.is_empty())
        {
            return false;
        }

        // Consume genuine-deferred before invoking the injectable factory so
        // even a test/factory panic cannot make ordinary probes look like a
        // never-attempted device. The production factory records success or
        // retryable/permanent failure in the remaining state.
        device.cursor.headless_deferred = false;
        factory(device, &crtcs, boundary);
        true
    }

    fn refresh_cursor_topology(&mut self) {
        let all_devices: HashSet<_> = self.devices.iter().map(|device| device.key).collect();
        self.refresh_cursor_topology_for_devices(&all_devices);
    }

    /// Detach every currently owned atomic cursor binding during global
    /// recovery, shutdown, VT-leave, or DRM-master loss.
    ///
    /// # Errors
    /// Per-CRTC failures are logged; this never returns `Err`
    /// unless the plane is unavailable.
    pub(crate) fn cursor_plane_hide_all(&mut self) -> io::Result<()> {
        let outputs: Vec<_> = self
            .outputs
            .iter()
            .map(|layout| (layout.key.device_key, layout.output.crtc))
            .collect();
        let mut found = false;
        for device_idx in 0..self.devices.len() {
            let device_key = self.devices[device_idx].key;
            let atomic_path = self.devices[device_idx].cursor.state_path()
                == crate::kms::cursor_plane::topology::CursorStatePath::Atomic;
            if self.devices[device_idx].cursor.plane.is_none() {
                continue;
            }
            found = true;
            if !atomic_path {
                continue;
            }
            let mut crtcs: Vec<_> = outputs
                .iter()
                .filter(|(key, _)| *key == device_key)
                .map(|(_, crtc)| *crtc)
                .collect();
            for crtc in self.devices[device_idx]
                .cursor
                .crtc_states
                .keys()
                .chain(
                    self.devices[device_idx]
                        .cursor
                        .atomic_cursor_commits_in_flight
                        .keys(),
                )
                .copied()
            {
                if !crtcs.contains(&crtc) {
                    crtcs.push(crtc);
                }
            }
            let bound_crtcs: HashSet<_> = crtcs
                .iter()
                .copied()
                .filter(|crtc| {
                    let cursor = &self.devices[device_idx].cursor;
                    cursor.hw_bound_on(*crtc)
                        || cursor
                            .atomic_cursor_commits_in_flight
                            .get(crtc)
                            .and_then(|commit| commit.prepared.as_ref())
                            .is_some_and(|prepared| prepared.prior.is_some())
                })
                .collect();
            crtcs.retain(|crtc| bound_crtcs.contains(crtc));
            for crtc in crtcs {
                let result = self.detach_cursor_blocking(device_idx, crtc);
                if let Err(error) = result {
                    if error.kind() == io::ErrorKind::PermissionDenied {
                        log::debug!(
                            "render cursor hide_all: device {device_key} CRTC {crtc:?} (no master): {error}"
                        );
                    } else {
                        log::warn!(
                            "render cursor hide_all: device {device_key} CRTC {crtc:?} failed: {error}"
                        );
                    }
                }
            }
        }
        if found {
            Ok(())
        } else {
            Err(io::Error::other("cursor plane unavailable"))
        }
    }

    /// Re-arm the hardware cursor plane on every CRTC that was
    /// showing the cursor before the suspend. This restores the
    /// kernel-side cursor binding that the VT switch tore down.
    ///
    /// Called after a connector snapshot has been applied so the output list
    /// is up to date. The cursor position and hotspot come from the
    /// caller (backend passes `core.cursor_x/y` and the effective
    /// cursor's hotspot).
    pub(crate) fn rearm_cursor(&mut self, hot_x: u16, hot_y: u16, x: i32, y: i32) {
        self.refresh_cursor_topology();
        let routes: Vec<_> = self
            .outputs
            .iter()
            .enumerate()
            .filter_map(|(output_idx, layout)| {
                let device = self.device_for_key(layout.key.device_key)?;
                (device
                    .cursor
                    .crtc_states
                    .get(&layout.output.crtc)
                    .is_some_and(|state| state.resume_visible)
                    || device.cursor.hw_bound_on(layout.output.crtc))
                .then_some((output_idx, device.key, layout.output.crtc))
            })
            .collect();
        log::info!(
            "render resume rearm_cursor: outputs={} initialized_devices={} visible_routes={} hot=({hot_x},{hot_y}) pos=({x},{y})",
            self.outputs.len(),
            self.devices
                .iter()
                .filter(|device| device.cursor.plane.is_some())
                .count(),
            routes.len(),
        );
        let mut shown = 0usize;
        let mut failed = 0usize;
        for (output_idx, device_key, crtc) in routes {
            match self.cursor_plane_show_on_crtc(output_idx, hot_x, hot_y, x, y) {
                Ok(()) => {
                    shown += 1;
                    log::info!(
                        "render resume rearm_cursor: device {device_key} CRTC={crtc:?} show ok"
                    );
                }
                Err(error) => {
                    failed += 1;
                    log::warn!(
                        "render resume rearm_cursor: device {device_key} CRTC={crtc:?} show failed: {error}"
                    );
                }
            }
        }
        log::info!("render resume rearm_cursor: done — shown={shown} failed={failed}");
    }
}

pub(super) fn cursor_root_to_crtc_local(
    x: i32,
    y: i32,
    layout_x: i32,
    layout_y: i32,
    hot_x: u16,
    hot_y: u16,
) -> (i32, i32) {
    (
        x - layout_x - i32::from(hot_x),
        y - layout_y - i32::from(hot_y),
    )
}

/// Whether the cursor footprint `[dx, dx+cw) × [dy, dy+ch)` (in
/// output-local coordinates) overlaps the output's `[0, w) × [0, h)`
/// region. This is the boolean form of `cursor_footprint_rect`'s
/// non-empty condition, kept in sync so `cursor_crtc_membership_dirty`
/// decides on-output membership exactly as the scene does.
pub(super) fn cursor_footprint_intersects_output(
    dx: i32,
    dy: i32,
    cw: i32,
    ch: i32,
    w: i32,
    h: i32,
) -> bool {
    dx < w && dx + cw > 0 && dy < h && dy + ch > 0
}
