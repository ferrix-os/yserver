//! An X game's pointer lock, as the compositor's `zwp_locked_pointer_v1`
//! (Ferrix's `docs/GAME-PATH.md` §4.7).
//!
//! A game that turns the mouse into a direction grabs the pointer to its
//! window, hides the cursor and warps the pointer back to the window's
//! centre after each motion, reading how far it moved (core `MotionNotify`
//! from the centre, or `XInput2` raw motion). On a compositor that moves the
//! pointer itself that pattern breaks: X's warp moves X's pointer only, the
//! compositor's runs into the screen's edge, and the next absolute motion
//! jumps X's pointer to wherever the compositor's is. So the server asks the
//! compositor to lock the pointer on the game's window, and while the lock
//! is in force each `relative_motion` moves X's pointer by its distance
//! from where X has it, which after a warp is the centre.
//!
//! When to lock is Xwayland's rule (`hw/xwayland/xwayland-input.c`,
//! `xwl_seat_maybe_lock_on_hidden_cursor` and `xwl_seat_emulate_pointer_warp`),
//! for a server that, like a rootless Xwayland, has its windows managed by
//! the compositor:
//!
//! * the cursor is hidden: Xwayland's is the `None` cursor; here also the
//!   XFIXES `HideCursor` and a cursor whose every pixel is transparent,
//!   which is how SDL and most games hide it;
//! * the compositor's pointer is on the window, and
//! * a grab confines the pointer to it, or a `WarpPointer` lands inside it.
//!
//! When to let go is the brief's (the customer's) narrowing of Xwayland's:
//! the cursor shown again, the compositor's pointer or keyboard off the
//! window (focus lost), the grab the lock was taken under ended, and, for a
//! lock taken by a warp with no grab, X's pointer moved out of the window
//! (Xwayland's `xwl_pointer_warp_emulator_handle_motion`). The lock is asked
//! for as `persistent`, as Xwayland asks: the compositor lifts it while the
//! window does not have the pointer and puts it back, while the server
//! keeps it.
//!
//! With `YSERVER_POINTER_LOCK=0` the server never locks (the gate's
//! negative control).

use yserver_protocol::x11::ResourceId;

/// What the server and the compositor say now, as far as the lock goes.
/// Windows are X top-levels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Scene {
    /// The cursor in effect is hidden.
    pub hidden: bool,
    /// The top-level the compositor's pointer is on.
    pub pointer_on: Option<ResourceId>,
    /// The top-level the compositor's keyboard is on.
    pub keyboard_on: Option<ResourceId>,
    /// An active pointer grab a client asked for (not the implicit one of a
    /// button held).
    pub grabbed: bool,
    /// The top-level the grab confines the pointer to.
    pub confined_to: Option<ResourceId>,
    /// The top-level a `WarpPointer` since the last look landed inside.
    pub warped_into: Option<ResourceId>,
    /// X's pointer is inside the top-level the lock is on.
    pub inside_held: bool,
}

/// A lock the server holds: on which top-level, and whether it was taken
/// under a grab, whose end lets it go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Held {
    pub window: ResourceId,
    pub grabbed: bool,
}

/// What to do about the lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Nothing changes.
    Stay,
    /// Lock the pointer on this top-level.
    Lock(Held),
    /// Let the lock go, for this reason.
    Release(&'static str),
}

/// What to do, given the scene and the lock held.
pub(super) fn decide(scene: &Scene, held: Option<Held>) -> Verdict {
    if let Some(held) = held {
        let release = if !scene.hidden {
            Some("the cursor is shown")
        } else if scene.pointer_on != Some(held.window) {
            Some("the pointer left the window")
        } else if scene.keyboard_on != Some(held.window) {
            Some("the window lost the keyboard")
        } else if held.grabbed && !scene.grabbed {
            Some("the grab ended")
        } else if !held.grabbed && !scene.grabbed && !scene.inside_held {
            Some("the X pointer left the window")
        } else {
            None
        };
        return release.map_or(Verdict::Stay, Verdict::Release);
    }
    let Some(window) = scene.pointer_on else {
        return Verdict::Stay;
    };
    if !scene.hidden || scene.keyboard_on != Some(window) {
        return Verdict::Stay;
    }
    if scene.confined_to == Some(window) || scene.warped_into == Some(window) {
        return Verdict::Lock(Held {
            window,
            grabbed: scene.grabbed,
        });
    }
    Verdict::Stay
}

/// Whether a cursor of B, G, R, A pixels shows nothing.
pub(super) fn transparent(bgra: &[u8]) -> bool {
    bgra.chunks_exact(4).all(|pixel| pixel[3] == 0)
}

/// A distance in pixels, with what is left of the last one, as whole
/// pixels and the new remainder.
pub(super) fn whole(residue: &mut f64, distance: f64) -> i32 {
    let sum = *residue + distance;
    let whole = sum.trunc();
    *residue = sum - whole;
    #[allow(clippy::cast_possible_truncation)]
    let whole = whole as i32;
    whole
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAME: ResourceId = ResourceId(0x20_0001);
    const OTHER: ResourceId = ResourceId(0x40_0001);

    /// The game's scene: cursor hidden, pointer and keyboard on it, a grab
    /// confining the pointer to it.
    fn game() -> Scene {
        Scene {
            hidden: true,
            pointer_on: Some(GAME),
            keyboard_on: Some(GAME),
            grabbed: true,
            confined_to: Some(GAME),
            warped_into: None,
            inside_held: true,
        }
    }

    #[test]
    fn a_hidden_cursor_confined_to_the_window_locks() {
        assert_eq!(
            decide(&game(), None),
            Verdict::Lock(Held {
                window: GAME,
                grabbed: true
            })
        );
    }

    #[test]
    fn a_warp_into_the_window_with_the_cursor_hidden_locks_without_a_grab() {
        let scene = Scene {
            grabbed: false,
            confined_to: None,
            warped_into: Some(GAME),
            ..game()
        };
        assert_eq!(
            decide(&scene, None),
            Verdict::Lock(Held {
                window: GAME,
                grabbed: false
            })
        );
    }

    #[test]
    fn nothing_locks_with_the_cursor_shown_or_the_pointer_or_keyboard_elsewhere() {
        for scene in [
            Scene {
                hidden: false,
                ..game()
            },
            Scene {
                pointer_on: Some(OTHER),
                ..game()
            },
            Scene {
                pointer_on: None,
                ..game()
            },
            Scene {
                keyboard_on: None,
                ..game()
            },
            Scene {
                confined_to: Some(OTHER),
                ..game()
            },
            Scene {
                confined_to: None,
                warped_into: Some(OTHER),
                ..game()
            },
            Scene {
                confined_to: None,
                ..game()
            },
        ] {
            assert_eq!(decide(&scene, None), Verdict::Stay, "{scene:?}");
        }
    }

    #[test]
    fn a_lock_stays_while_the_game_holds_it() {
        let held = Held {
            window: GAME,
            grabbed: true,
        };
        assert_eq!(decide(&game(), Some(held)), Verdict::Stay);
        // Its warps and the confine come and go; the lock stays.
        let scene = Scene {
            confined_to: None,
            warped_into: Some(GAME),
            ..game()
        };
        assert_eq!(decide(&scene, Some(held)), Verdict::Stay);
    }

    #[test]
    fn a_lock_goes_on_ungrab_focus_loss_or_a_shown_cursor() {
        let held = Held {
            window: GAME,
            grabbed: true,
        };
        let release = |scene: Scene| match decide(&scene, Some(held)) {
            Verdict::Release(why) => why,
            other => panic!("{other:?} for {scene:?}"),
        };
        assert_eq!(
            release(Scene {
                grabbed: false,
                confined_to: None,
                ..game()
            }),
            "the grab ended"
        );
        assert_eq!(
            release(Scene {
                keyboard_on: Some(OTHER),
                ..game()
            }),
            "the window lost the keyboard"
        );
        assert_eq!(
            release(Scene {
                pointer_on: None,
                ..game()
            }),
            "the pointer left the window"
        );
        assert_eq!(
            release(Scene {
                hidden: false,
                ..game()
            }),
            "the cursor is shown"
        );
    }

    #[test]
    fn a_lock_taken_by_a_warp_goes_when_x_moves_the_pointer_out() {
        let held = Held {
            window: GAME,
            grabbed: false,
        };
        let scene = Scene {
            grabbed: false,
            confined_to: None,
            ..game()
        };
        assert_eq!(decide(&scene, Some(held)), Verdict::Stay);
        let out = Scene {
            inside_held: false,
            ..scene
        };
        assert_eq!(
            decide(&out, Some(held)),
            Verdict::Release("the X pointer left the window")
        );
    }

    #[test]
    fn a_cursor_with_no_opaque_pixel_is_transparent() {
        assert!(transparent(&[1, 2, 3, 0, 9, 9, 9, 0]));
        assert!(!transparent(&[1, 2, 3, 0, 9, 9, 9, 1]));
        assert!(transparent(&[]));
    }

    #[test]
    fn distances_keep_their_fractions_for_the_next() {
        let mut residue = 0.0;
        assert_eq!(whole(&mut residue, 0.6), 0);
        assert_eq!(whole(&mut residue, 0.6), 1);
        assert!((residue - 0.2).abs() < 1e-9);
        assert_eq!(whole(&mut residue, -2.5), -2);
        assert!((residue + 0.3).abs() < 1e-9);
        assert_eq!(whole(&mut residue, 12.0), 11);
    }
}
