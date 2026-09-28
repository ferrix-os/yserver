//! Requests the server makes for itself, as a client or a window manager
//! would make them: what a rootless server needs when another display
//! system shows its top-level windows (yserver's Wayland backend).

use super::*;

/// Redirect every child of `parent` for the server itself: the record a
/// client's `RedirectSubwindows(parent, mode)` would make, owned by
/// `SERVER_OWNER`, so no disconnect tears it down. Children that exist now
/// get their backings at once; later ones get theirs as they map, through
/// the hooks a client's redirect goes through.
///
/// A rootless server whose top-level windows another display system shows
/// (yserver's Wayland backend, where each is a Wayland window) needs each
/// top-level's whole subtree in one image it can read back, and that is what
/// a redirect backing holds. Xwayland redirects its top-levels for its own
/// `serverClient` for the same reason.
///
/// Cheap when the record is there already, so it may be called on every
/// loop iteration, which also puts it back after a server reset. Returns
/// whether the server's record is in place: `false` when a client
/// redirected `parent`'s children first, which is left alone, or when the
/// backend cannot redirect.
pub fn redirect_subwindows_for_server(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    parent: ResourceId,
    mode: crate::server::CompositeRedirectMode,
) -> bool {
    let owner = crate::resources::SERVER_OWNER;
    let records = state.composite_redirects.subwindows_records(parent);
    if !records.is_empty() {
        return records.iter().any(|r| r.owner == owner);
    }
    if !backend.supports_redirect_activation() {
        return false;
    }
    // As a client's RedirectSubwindows: top first, and no record for the
    // overlay window.
    let targets: Vec<ResourceId> = state
        .resources
        .children(parent)
        .iter()
        .rev()
        .copied()
        .filter(|child| *child != COMPOSITE_OVERLAY_WINDOW)
        .collect();
    let before: Vec<_> = targets
        .iter()
        .map(|t| state.composite_redirects.window_mode(*t))
        .collect();
    let record = crate::server::RedirectRecord { mode, owner };
    if state
        .composite_redirects
        .redirect_subwindows(parent, &targets, record)
        .is_err()
    {
        return false;
    }
    debug!(
        "the server redirects the children of 0x{:x} ({mode:?})",
        parent.0
    );
    for (target, before) in targets.into_iter().zip(before) {
        sync_redirect_backing(state, backend, None, target, before);
    }
    true
}
