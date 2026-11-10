//! Hardware cursor plane — replaces the Vulkan-composited cursor
//! quad with a kernel-managed DRM cursor overlay.
//!
//! Why: the cursor quad was tied to compositor cadence. Every cursor
//! position change waited for the next `composite_and_flip`, which
//! is stalled by per-op `vkQueueWaitIdle` in the paint pipeline
//! (notably when hovering over GTK widgets that schedule
//! gradient/emboss repaints — observed as severe pointer lag in
//! mate-control-center on fuji). The DRM hardware cursor plane is
//! a separate overlay the kernel positions independently —
//! an atomic position commit is microseconds and doesn't touch the GPU.
//!
//! Atomic cursor support is discovered per plane from its properties and
//! device capabilities. Devices without complete support use the existing
//! Vulkan-composited software cursor.

pub(crate) mod ops;
pub(crate) mod sprites;
pub(crate) mod topology;

use std::{collections::HashMap, io, rc::Rc};

use drm::{
    Device as DrmDevice, DriverCapability,
    control::{crtc, plane, property},
};

use crate::drm::{CursorDeviceCaps, Device};
use ops::{CursorBufferIo, DrmCursorBufferIo};
use sprites::SpritePool;
use topology::{
    CursorPathDecision, CursorPlaneDiscovery, CursorPlaneDiscoveryIo, CursorPlaneProperties,
    DiscoveredCursorPlane, DrmCursorPlaneDiscovery, decide_cursor_state_path,
};

/// A failed cursor bind/reposition operation, including whether the cursor is
/// still bound after best-effort rollback. Callers must not switch an output
/// to software composition while `remains_visible()` is true: doing so would
/// display both the old hardware cursor and the new software sprite.
#[derive(Debug)]
pub enum CursorShowError {
    Unbound(io::Error),
    #[allow(
        dead_code,
        reason = "P2b Task 2 constructs deferred atomic cursor show results"
    )]
    Deferred {
        remains_visible: bool,
        reason: io::Error,
    },
    StillVisible {
        operation_error: io::Error,
        rollback_error: Option<io::Error>,
    },
}

impl CursorShowError {
    #[must_use]
    pub fn remains_visible(&self) -> bool {
        match self {
            Self::Unbound(_) => false,
            Self::Deferred {
                remains_visible, ..
            } => *remains_visible,
            Self::StillVisible { .. } => true,
        }
    }

    #[must_use]
    pub fn is_deferred(&self) -> bool {
        matches!(self, Self::Deferred { .. })
    }

    #[must_use]
    pub fn operation_error(&self) -> &io::Error {
        match self {
            Self::Unbound(error) => error,
            Self::Deferred { reason, .. } => reason,
            Self::StillVisible {
                operation_error, ..
            } => operation_error,
        }
    }
}

impl std::fmt::Display for CursorShowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unbound(error) => write!(f, "cursor remains unbound: {error}"),
            Self::Deferred { reason, .. } => write!(f, "cursor show deferred: {reason}"),
            Self::StillVisible {
                operation_error,
                rollback_error,
            } => match rollback_error {
                Some(rollback_error) => write!(
                    f,
                    "cursor operation failed ({operation_error}) and hide rollback failed ({rollback_error}); prior binding remains visible"
                ),
                None => write!(
                    f,
                    "cursor rebind failed ({operation_error}); prior binding remains visible"
                ),
            },
        }
    }
}

impl std::error::Error for CursorShowError {}

/// Fallback cursor size when `DRM_CAP_CURSOR_WIDTH/HEIGHT` query fails
/// (very old drivers, broken devices). Every Intel / AMD / mainstream-
/// Mali iGPU since ~2010 supports at least 64×64.
///
/// Atomic cursor sprites use the dimensions the driver reports via
/// `DriverCapability::CursorWidth/Height` (typically 64 on
/// Intel i915, 128 or 256 on amdgpu, varies on others). Using the
/// driver-reported size is load-bearing: amdgpu's display engine
/// interprets the cursor framebuffer as if it were `cursor_width ×
/// cursor_height`, so allocating smaller causes it to read past our
/// data → cursor vertically squished + intermittent corruption.
pub const HW_CURSOR_FALLBACK_W: u32 = 64;
pub const HW_CURSOR_FALLBACK_H: u32 = 64;

/// Discovered atomic cursor-plane properties and the per-device sprite pool.
pub struct CursorPlane {
    buffer_io: Rc<dyn CursorBufferIo>,
    sprites: SpritePool,
    /// Cursor sprite dimensions in pixels. Sourced from
    /// `DriverCapability::CursorWidth/Height`.
    width: u32,
    height: u32,
    /// CRTC to cursor-plane assignment over the device's explicitly exposed
    /// universal cursor planes; no plane serves two CRTCs. `None` means the
    /// driver exposed no cursor planes. A CRTC without an entry has no plane.
    explicit_plane_assignment: Option<HashMap<crtc::Handle, plane::Handle>>,
    /// Atomic position properties of each CRTC's assigned plane.
    atomic_positions: HashMap<crtc::Handle, (plane::Handle, property::Handle, property::Handle)>,
    properties_by_crtc: HashMap<crtc::Handle, CursorPlaneProperties>,
    state_path: CursorPathDecision,
}

/// Assign each CRTC at most one cursor plane and each plane to at most one
/// CRTC, like wlroots: a plane whose `possible_crtcs` is exactly one CRTC goes
/// to that CRTC (the kernel's legacy cursor plane on per-CRTC-cursor drivers),
/// then the remaining planes are matched to the remaining CRTCs with augmenting
/// paths (a maximum matching, so greedy order cannot strand a coverable CRTC).
fn assign_cursor_planes(
    crtcs: &[crtc::Handle],
    planes: &[DiscoveredCursorPlane],
) -> HashMap<crtc::Handle, usize> {
    let mut assignment = HashMap::new();
    let mut exclusive = vec![false; planes.len()];
    for (index, plane) in planes.iter().enumerate() {
        if plane.crtcs.len() == 1
            && let Some(crtc) = plane.crtcs.iter().next()
            && crtcs.contains(crtc)
            && !assignment.contains_key(crtc)
        {
            assignment.insert(*crtc, index);
            exclusive[index] = true;
        }
    }
    let mut owner: Vec<Option<usize>> = vec![None; planes.len()];
    for (crtc_index, crtc) in crtcs.iter().enumerate() {
        if !assignment.contains_key(crtc) {
            let mut visited = vec![false; planes.len()];
            augment_cursor_matching(
                crtc_index,
                crtcs,
                planes,
                &exclusive,
                &mut owner,
                &mut visited,
            );
        }
    }
    for (index, crtc_index) in owner.iter().enumerate() {
        if let Some(crtc_index) = crtc_index {
            assignment.insert(crtcs[*crtc_index], index);
        }
    }
    assignment
}

/// Kuhn's augmenting-path step: give `crtcs[crtc_index]` a plane, moving
/// earlier non-exclusive owners to other planes when that frees one.
fn augment_cursor_matching(
    crtc_index: usize,
    crtcs: &[crtc::Handle],
    planes: &[DiscoveredCursorPlane],
    exclusive: &[bool],
    owner: &mut [Option<usize>],
    visited: &mut [bool],
) -> bool {
    for index in 0..planes.len() {
        if exclusive[index] || visited[index] || !planes[index].crtcs.contains(&crtcs[crtc_index]) {
            continue;
        }
        visited[index] = true;
        let free = match owner[index] {
            None => true,
            Some(other) => augment_cursor_matching(other, crtcs, planes, exclusive, owner, visited),
        };
        if free {
            owner[index] = Some(crtc_index);
            return true;
        }
    }
    false
}

/// Explicit plane assignment and the atomic position properties of each
/// assigned plane. Drivers exposing no cursor planes get no assignment.
type CursorPlaneTopology = (
    Option<HashMap<crtc::Handle, plane::Handle>>,
    HashMap<crtc::Handle, (plane::Handle, property::Handle, property::Handle)>,
);

fn cursor_plane_topology(
    crtcs: &[crtc::Handle],
    planes: &[DiscoveredCursorPlane],
) -> CursorPlaneTopology {
    if planes.is_empty() {
        return (None, HashMap::new());
    }
    let assignment = assign_cursor_planes(crtcs, planes);
    let atomic_positions = assignment
        .iter()
        .filter_map(|(crtc, index)| {
            let plane = &planes[*index];
            Some((
                *crtc,
                (
                    plane.handle,
                    plane.properties.get("CRTC_X").copied()?,
                    plane.properties.get("CRTC_Y").copied()?,
                ),
            ))
        })
        .collect();
    let assignment = assignment
        .into_iter()
        .map(|(crtc, index)| (crtc, planes[index].handle))
        .collect();
    (Some(assignment), atomic_positions)
}

impl CursorPlane {
    /// Discover universal cursor planes and remember their atomic properties.
    ///
    /// # Errors
    /// Cursor-plane topology discovery failures.
    pub fn new(device: Rc<Device>, crtcs: &[crtc::Handle]) -> io::Result<Self> {
        let caps = device.cursor_caps().cloned();
        let buffer_io: Rc<dyn CursorBufferIo> = Rc::new(DrmCursorBufferIo);
        Self::new_with_discovery(device, crtcs, buffer_io, &DrmCursorPlaneDiscovery, caps)
    }

    pub(crate) fn new_with_discovery(
        device: Rc<Device>,
        crtcs: &[crtc::Handle],
        buffer_io: Rc<dyn CursorBufferIo>,
        discovery_io: &dyn CursorPlaneDiscoveryIo,
        caps: Option<CursorDeviceCaps>,
    ) -> io::Result<Self> {
        let discovery = discovery_io.discover(&device)?;
        // Query the driver's preferred cursor dimensions. amdgpu commonly
        // reports 128×128 or 256×256; i915 typically 64×64. We MUST use
        // the reported size — see [`HW_CURSOR_FALLBACK_W`] for the
        // load-bearing rationale.
        let width = device
            .get_driver_capability(DriverCapability::CursorWidth)
            .ok()
            .filter(|&w| w >= u64::from(HW_CURSOR_FALLBACK_W))
            .and_then(|w| u32::try_from(w).ok())
            .unwrap_or(HW_CURSOR_FALLBACK_W);
        let height = device
            .get_driver_capability(DriverCapability::CursorHeight)
            .ok()
            .filter(|&h| h >= u64::from(HW_CURSOR_FALLBACK_H))
            .and_then(|h| u32::try_from(h).ok())
            .unwrap_or(HW_CURSOR_FALLBACK_H);
        log::info!("cursor: driver reports CursorWidth={width} CursorHeight={height}");

        Ok(Self::from_discovery_and_backing(
            buffer_io, discovery, caps, width, height, crtcs,
        ))
    }

    fn from_discovery_and_backing(
        buffer_io: Rc<dyn CursorBufferIo>,
        discovery: CursorPlaneDiscovery,
        caps: Option<CursorDeviceCaps>,
        width: u32,
        height: u32,
        active_crtcs: &[crtc::Handle],
    ) -> Self {
        let (explicit_plane_assignment, atomic_positions) =
            cursor_plane_topology(&discovery.crtcs, &discovery.planes);
        let assigned = explicit_plane_assignment
            .as_ref()
            .cloned()
            .unwrap_or_default();
        let properties_by_crtc = assigned
            .iter()
            .filter_map(|(crtc, handle)| {
                discovery
                    .planes
                    .iter()
                    .find(|candidate| candidate.handle == *handle)
                    .and_then(DiscoveredCursorPlane::cursor_properties)
                    .map(|properties| (*crtc, properties))
            })
            .collect();
        let state_path = decide_cursor_state_path(&assigned, &discovery.planes, caps.as_ref());
        let mut sprites = SpritePool::new(width, height);
        sprites.set_active_crtc_count(active_crtcs.len());
        let plane = Self {
            buffer_io,
            sprites,
            width,
            height,
            explicit_plane_assignment,
            atomic_positions,
            properties_by_crtc,
            state_path,
        };
        if !plane.supports_crtcs(active_crtcs) {
            log::warn!(
                "cursor: no distinct cursor plane is assigned to every one of the {} active CRTCs",
                active_crtcs.len()
            );
        }
        plane
    }

    #[cfg(test)]
    pub(crate) fn for_tests_from_snapshot(
        device: Rc<Device>,
        crtcs: &[crtc::Handle],
        buffer_io: Rc<dyn CursorBufferIo>,
        snapshot: topology::CursorPlaneTestSnapshot,
        width: u32,
        height: u32,
    ) -> Self {
        let (snapshot_discovery, caps) = snapshot.into_discovery();
        let discovery_io = topology::RecordingCursorPlaneDiscovery::new(snapshot_discovery);
        let discovery = discovery_io
            .discover(&device)
            .expect("recording cursor discovery cannot fail");
        Self::from_discovery_and_backing(buffer_io, discovery, caps, width, height, crtcs)
    }

    /// Build an ioctl-free cursor plane for platform state-machine tests.
    #[cfg(test)]
    pub(crate) fn for_tests_stub(width: u32, height: u32) -> Self {
        let discovery = CursorPlaneDiscovery {
            crtcs: Vec::new(),
            planes: Vec::new(),
        };
        let caps = None;
        let state_path =
            decide_cursor_state_path(&HashMap::new(), &discovery.planes, caps.as_ref());
        let mut sprites = SpritePool::new(width, height);
        sprites.set_active_crtc_count(0);
        Self {
            buffer_io: Rc::new(DrmCursorBufferIo),
            sprites,
            width,
            height,
            explicit_plane_assignment: None,
            atomic_positions: HashMap::new(),
            properties_by_crtc: HashMap::new(),
            state_path,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_tests_set_atomic_position_properties(
        &mut self,
        crtc: crtc::Handle,
        plane: plane::Handle,
        crtc_x: property::Handle,
        crtc_y: property::Handle,
    ) {
        self.atomic_positions.insert(crtc, (plane, crtc_x, crtc_y));
        self.explicit_plane_assignment
            .get_or_insert_with(HashMap::new)
            .insert(crtc, plane);
        let base = 0xa000 + u32::from(crtc) * 16;
        let property =
            |offset| ::drm::control::from_u32(base + offset).expect("test cursor property handle");
        self.properties_by_crtc
            .entry(crtc)
            .or_insert_with(|| CursorPlaneProperties {
                plane,
                fb_id: property(1),
                crtc_id: property(2),
                crtc_x,
                crtc_y,
                crtc_w: property(5),
                crtc_h: property(6),
                src_x: property(7),
                src_y: property(8),
                src_w: property(9),
                src_h: property(10),
                hotspot_x: None,
                hotspot_y: None,
            });
        self.state_path = CursorPathDecision {
            path: topology::CursorStatePath::Atomic,
            reason: topology::CursorPathReason::Qualified,
        };
    }

    #[cfg(test)]
    pub(crate) fn for_tests_explicit_plane_assignment(
        &self,
    ) -> Option<&HashMap<crtc::Handle, plane::Handle>> {
        self.explicit_plane_assignment.as_ref()
    }

    pub(crate) fn state_path(&self) -> CursorPathDecision {
        self.state_path
    }

    pub(crate) fn properties_for(&self, crtc: crtc::Handle) -> Option<CursorPlaneProperties> {
        self.properties_by_crtc.get(&crtc).copied()
    }

    pub(crate) fn sprites(&self) -> &SpritePool {
        &self.sprites
    }

    pub(crate) fn sprites_mut(&mut self) -> &mut SpritePool {
        &mut self.sprites
    }

    pub(crate) fn buffer_io(&self) -> Rc<dyn CursorBufferIo> {
        Rc::clone(&self.buffer_io)
    }

    /// Whether every requested CRTC has its own assigned cursor plane.
    #[must_use]
    pub fn supports_crtcs(&self, crtcs: &[crtc::Handle]) -> bool {
        self.explicit_plane_assignment
            .as_ref()
            .is_some_and(|assignment| crtcs.iter().all(|crtc| assignment.contains_key(crtc)))
    }

    /// Atomic plane and position properties for `crtc`, if its assigned
    /// cursor plane exposes both `CRTC_X` and `CRTC_Y`.
    #[must_use]
    pub(crate) fn atomic_position_properties(
        &self,
        crtc: crtc::Handle,
    ) -> Option<(plane::Handle, property::Handle, property::Handle)> {
        self.atomic_positions.get(&crtc).copied()
    }

    /// Whether every requested CRTC has a cursor plane with atomic position
    /// properties. An empty topology is supported vacuously; active topology
    /// selection always supplies its complete CRTC list.
    #[must_use]
    pub(crate) fn supports_atomic_positions(&self, crtcs: &[crtc::Handle]) -> bool {
        crtcs
            .iter()
            .all(|crtc| self.atomic_position_properties(*crtc).is_some())
    }

    /// Cursor plane width in pixels (driver-reported).
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Cursor plane height in pixels (driver-reported).
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn test_crtc(raw: u32) -> crtc::Handle {
        ::drm::control::from_u32(raw).unwrap()
    }

    fn discovered_with_properties(
        handle: u32,
        crtcs: &[crtc::Handle],
        property_names: &[&str],
    ) -> DiscoveredCursorPlane {
        DiscoveredCursorPlane {
            handle: ::drm::control::from_u32(handle).unwrap(),
            crtcs: crtcs.iter().copied().collect(),
            properties: property_names
                .iter()
                .enumerate()
                .map(|(index, name)| {
                    (
                        (*name).to_owned(),
                        ::drm::control::from_u32(handle * 10 + index as u32 + 1).unwrap(),
                    )
                })
                .collect(),
        }
    }

    fn discovered(handle: u32, crtcs: &[crtc::Handle], atomic: bool) -> DiscoveredCursorPlane {
        const FULL: [&str; 10] = [
            "FB_ID", "CRTC_ID", "CRTC_X", "CRTC_Y", "CRTC_W", "CRTC_H", "SRC_X", "SRC_Y", "SRC_W",
            "SRC_H",
        ];
        discovered_with_properties(handle, crtcs, if atomic { &FULL } else { &[] })
    }

    fn assigned_plane(topology: &CursorPlaneTopology, crtc: crtc::Handle) -> Option<plane::Handle> {
        topology.0.as_ref().and_then(|map| map.get(&crtc).copied())
    }

    #[test]
    // Mutation: pick the first compatible plane per CRTC independently. The
    // shared plane 21 would then serve both CRTCs, and CRTC a would not use
    // its own exclusive plane 22 (the kernel's legacy cursor plane for a).
    fn cursor_planes_prefer_exclusive_and_never_serve_two_crtcs() {
        let a = test_crtc(11);
        let b = test_crtc(12);
        let planes = [discovered(21, &[a, b], true), discovered(22, &[a], true)];
        let topology = cursor_plane_topology(&[a, b], &planes);

        assert_eq!(assigned_plane(&topology, a), Some(planes[1].handle));
        assert_eq!(assigned_plane(&topology, b), Some(planes[0].handle));
        assert_eq!(topology.1[&a].0, planes[1].handle);
        assert_eq!(topology.1[&b].0, planes[0].handle);
    }

    #[test]
    // Mutation: filter out planes with incomplete properties before matching.
    fn cursor_matching_ignores_property_completeness() {
        use super::topology::{CursorPathReason, CursorStatePath, decide_cursor_state_path};

        let a = test_crtc(11);
        let b = test_crtc(12);
        let incomplete_names = [
            "CRTC_ID", "CRTC_X", "CRTC_Y", "CRTC_W", "CRTC_H", "SRC_X", "SRC_Y", "SRC_W", "SRC_H",
        ];
        let incomplete = discovered_with_properties(21, &[a], &incomplete_names);
        let shared = discovered(22, &[a, b], true);
        let planes = [shared, incomplete];
        let topology = cursor_plane_topology(&[a, b], &planes);
        let assignment = topology.0.as_ref().expect("cursor planes discovered");

        assert_eq!(assignment.get(&a), Some(&planes[1].handle));
        assert_eq!(assignment.get(&b), Some(&planes[0].handle));
        let decision = decide_cursor_state_path(
            assignment,
            &planes,
            Some(&crate::drm::CursorDeviceCaps::new(
                false,
                Some("amdgpu".to_owned()),
            )),
        );
        assert_eq!(decision.path, CursorStatePath::Software);
        assert_eq!(decision.reason, CursorPathReason::MissingProperty("FB_ID"));
    }

    #[test]
    fn cursor_plane_atomic_positions_come_only_from_the_assigned_plane() {
        let a = test_crtc(11);
        let b = test_crtc(12);
        let planes = [discovered(21, &[a], false), discovered(22, &[a, b], true)];
        let topology = cursor_plane_topology(&[a, b], &planes);

        assert_eq!(assigned_plane(&topology, a), Some(planes[0].handle));
        assert!(!topology.1.contains_key(&a));
        assert_eq!(topology.1[&b].0, planes[1].handle);
    }

    #[test]
    fn cursor_plane_assignment_leaves_an_uncovered_crtc_unassigned() {
        let a = test_crtc(11);
        let b = test_crtc(12);
        let mut plane = CursorPlane::for_tests_stub(64, 64);
        let shared_only = [discovered(21, &[a, b], true)];
        let (assignment, atomic_positions) = cursor_plane_topology(&[a, b], &shared_only);
        plane.explicit_plane_assignment = assignment;
        plane.atomic_positions = atomic_positions;

        assert!(plane.supports_crtcs(&[a]));
        assert!(!plane.supports_crtcs(&[b]));
        assert!(!plane.supports_crtcs(&[a, b]));
        assert!(plane.supports_atomic_positions(&[a]));
        assert!(!plane.supports_atomic_positions(&[b]));

        let independently_drivable = [discovered(21, &[a, b], true), discovered(22, &[a, b], true)];
        let topology = cursor_plane_topology(&[a, b], &independently_drivable);
        assert_ne!(assigned_plane(&topology, a), assigned_plane(&topology, b));
        assert!(assigned_plane(&topology, b).is_some());
        assert_eq!(cursor_plane_topology(&[a, b], &[]).0, None);
    }

    fn assert_every_crtc_has_its_own_plane(crtcs: &[crtc::Handle], topology: &CursorPlaneTopology) {
        let assigned: Vec<_> = crtcs
            .iter()
            .map(|&crtc| assigned_plane(topology, crtc).expect("every CRTC gets a cursor plane"))
            .collect();
        let distinct: HashSet<_> = assigned.iter().collect();
        assert_eq!(distinct.len(), assigned.len(), "a plane serves two CRTCs");
    }

    #[test]
    // Mutation: greedy first-fit gives a→21, b→none, c→22; a maximum matching
    // reroutes a to 22 or 23 so that b can take 21.
    fn cursor_plane_assignment_is_a_maximum_matching() {
        let (a, b, c) = (test_crtc(11), test_crtc(12), test_crtc(13));
        let planes = [
            discovered(21, &[a, b], true),
            discovered(22, &[a, c], true),
            discovered(23, &[a, c], true),
        ];
        let topology = cursor_plane_topology(&[a, b, c], &planes);

        assert_every_crtc_has_its_own_plane(&[a, b, c], &topology);
    }

    #[test]
    // Mutation: read `Device::cursor_caps()` for the path or allocate the
    // initial sprite pair while constructing a plane from a snapshot.
    fn plane_from_snapshot_stores_properties_caps_and_allocates_no_sprite() {
        use super::topology::{CursorPlaneTestPlane, CursorPlaneTestSnapshot};
        use crate::kms::cursor_plane::ops::{CursorBufferIo, recording::RecordingCursorBufferIo};

        let crtc = test_crtc(11);
        let device = Rc::new(crate::drm::Device::for_tests().expect("test DRM device"));
        assert!(device.cursor_caps().is_none());
        let (recording, ledger) = RecordingCursorBufferIo::new(1);
        let buffer_io: Rc<dyn CursorBufferIo> = Rc::new(recording);
        let property_names = [
            "FB_ID",
            "CRTC_ID",
            "CRTC_X",
            "CRTC_Y",
            "CRTC_W",
            "CRTC_H",
            "SRC_X",
            "SRC_Y",
            "SRC_W",
            "SRC_H",
            "HOTSPOT_X",
            "HOTSPOT_Y",
        ];
        let plane = CursorPlane::for_tests_from_snapshot(
            device,
            &[crtc],
            buffer_io,
            CursorPlaneTestSnapshot {
                crtcs: vec![11],
                planes: vec![CursorPlaneTestPlane {
                    handle: 21,
                    possible_crtcs: vec![11],
                    property_names: property_names
                        .iter()
                        .map(|name| (*name).to_owned())
                        .collect(),
                }],
                cursor_caps: Some(CursorDeviceCaps::new(false, Some("amdgpu".to_owned()))),
            },
            64,
            64,
        );

        let properties = plane
            .properties_for(crtc)
            .expect("all cursor properties stored");
        let handles = [
            properties.fb_id,
            properties.crtc_id,
            properties.crtc_x,
            properties.crtc_y,
            properties.crtc_w,
            properties.crtc_h,
            properties.src_x,
            properties.src_y,
            properties.src_w,
            properties.src_h,
            properties.hotspot_x.expect("hotspot X property"),
            properties.hotspot_y.expect("hotspot Y property"),
        ];
        assert_eq!(
            handles.into_iter().collect::<HashSet<_>>().len(),
            handles.len()
        );
        assert_eq!(
            plane.state_path().path,
            super::topology::CursorStatePath::Atomic
        );
        assert!(plane.sprites().all_users_zero());
        assert!(ledger.records().is_empty());
        assert_eq!(ledger.end_state(), (0, 0, 0));
    }
    #[test]
    // Mutation: let augmenting paths reassign exclusive planes. CRTC c's
    // exclusive plane 23 must stay with c while a and b share the rest.
    fn cursor_plane_matching_keeps_exclusive_planes() {
        let (a, b, c) = (test_crtc(11), test_crtc(12), test_crtc(13));
        let planes = [
            discovered(21, &[a, b, c], true),
            discovered(22, &[a, c], true),
            discovered(23, &[c], true),
        ];
        let topology = cursor_plane_topology(&[a, b, c], &planes);

        assert_every_crtc_has_its_own_plane(&[a, b, c], &topology);
        assert_eq!(assigned_plane(&topology, c), Some(planes[2].handle));
        assert_eq!(assigned_plane(&topology, a), Some(planes[1].handle));
        assert_eq!(assigned_plane(&topology, b), Some(planes[0].handle));
    }
    /// Unavailable atomic operations report an error, while a hide on the
    /// software path is a no-op because no kernel cursor plane is owned.
    #[test]
    fn unavailable_plane_returns_err_not_panic() {
        use crate::kms::render::platform::PlatformBackend;

        let mut p = PlatformBackend::for_tests();
        assert!(!p.cursor_plane_available());
        assert!(
            p.cursor_plane_upload_image_for_output(0, 1, 0, 0, 16, 16, &[0u8; 16 * 16 * 4])
                .is_err()
        );
        assert!(p.cursor_plane_show_on_crtc(0, 0, 0, 0, 0).is_err());
        assert!(p.cursor_plane_move(0, 0, 0, 0).is_err());
        assert!(p.cursor_plane_hide_on_crtc(0).is_ok());
        assert!(p.cursor_plane_hide_all().is_err());
        assert!(!p.cursor_plane_uploaded_image_matches_for_output(0, 1, 0, 0));
    }
}
