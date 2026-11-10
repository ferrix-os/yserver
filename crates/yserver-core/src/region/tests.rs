use proptest::prelude::*;
use yserver_protocol::x11::xfixes::RegionRect;

use super::*;
use crate::{core_loop::clip_list::canonical, nested};

fn r(x: i16, y: i16, width: u16, height: u16) -> RegionRect {
    RegionRect {
        x,
        y,
        width,
        height,
    }
}

/// Rects on a small grid, so that edges coincide often, including empty
/// and negative ones, in arbitrary order and overlapping.
fn rects() -> impl Strategy<Value = Vec<RegionRect>> {
    prop::collection::vec((-3i16..9, -3i16..9, 0u16..7, 0u16..7), 0..7)
        .prop_map(|v| v.into_iter().map(|(x, y, w, h)| r(x, y, w, h)).collect())
}

const GRID: std::ops::Range<i32> = -3..16;

/// The grid cells `rects` cover: an oracle independent of both
/// implementations.
fn raster(rects: &[RegionRect]) -> Vec<bool> {
    let mut cells = Vec::new();
    for y in GRID {
        for x in GRID {
            cells.push(rects.iter().any(|r| {
                let b = Box32::from_rect(*r);
                b.x1 <= x && x < b.x2 && b.y1 <= y && y < b.y2
            }));
        }
    }
    cells
}

/// The cells of `GRID` moved by `(bx, by)` that `boxes` cover.
fn raster_boxes(boxes: &[Box32], bx: i32, by: i32) -> Vec<bool> {
    let mut cells = Vec::new();
    for y in GRID {
        for x in GRID {
            let (x, y) = (x + bx, y + by);
            cells.push(
                boxes
                    .iter()
                    .any(|b| b.x1 <= x && x < b.x2 && b.y1 <= y && y < b.y2),
            );
        }
    }
    cells
}

/// Grid origins at the wire's limits as well as at zero.
fn bases() -> impl Strategy<Value = i32> {
    prop_oneof![
        Just(0),
        Just(i32::from(i16::MIN) + 3),
        Just(i32::from(i16::MAX) - 8),
        Just(i32::from(i16::MAX) - 2),
    ]
}

fn wire(v: i32) -> i16 {
    v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

fn check(
    a: &[RegionRect],
    b: &[RegionRect],
    new: &Region,
    old: Vec<RegionRect>,
    cell: fn(bool, bool) -> bool,
) {
    let got = new.to_rects();
    assert!(is_canonical(&new.boxes), "not canonical: {got:?}");
    let want_cells: Vec<bool> = raster(a)
        .into_iter()
        .zip(raster(b))
        .map(|(p, q)| cell(p, q))
        .collect();
    assert_eq!(raster(&got), want_cells, "a={a:?} b={b:?} got={got:?}");
    assert_eq!(got, canonical(old), "a={a:?} b={b:?}");
}

proptest! {
    #[test]
    fn union_matches_the_old_ops_and_the_raster(a in rects(), b in rects()) {
        let new = Region::from_rects(&a).union(&Region::from_rects(&b));
        check(&a, &b, &new, nested::union_regions(&canonical(a.clone()), &b), |p, q| p || q);
    }

    #[test]
    fn intersect_matches_the_old_ops_and_the_raster(a in rects(), b in rects()) {
        let new = Region::from_rects(&a).intersect(&Region::from_rects(&b));
        check(&a, &b, &new, nested::intersect_regions(&a, &b), |p, q| p && q);
    }

    #[test]
    fn subtract_matches_the_old_ops_and_the_raster(a in rects(), b in rects()) {
        let new = Region::from_rects(&a).subtract(&Region::from_rects(&b));
        check(&a, &b, &new, nested::subtract_regions(&a, &b), |p, q| p && !q);
    }

    /// A chain of operations on canonical regions stays canonical without
    /// ever normalising.
    #[test]
    fn chained_operations_stay_canonical(a in rects(), b in rects(), c in rects()) {
        let (ra, rb, rc) = (Region::from_rects(&a), Region::from_rects(&b), Region::from_rects(&c));
        let new = ra.union(&rb).subtract(&rc).intersect(&ra.union(&rc));
        let want: Vec<bool> = raster(&a).iter().zip(raster(&b)).zip(raster(&c))
            .map(|((p, q), s)| (*p || q) && !s && (*p || s))
            .collect();
        prop_assert!(is_canonical(&new.boxes));
        prop_assert_eq!(raster(&new.to_rects()), want);
    }

    /// Arbitrary input becomes canonical boxes covering exactly its pixels,
    /// and, in the wire's range, the same rects as the wire normaliser.
    #[test]
    fn from_rects_normalises_to_the_raster(a in rects()) {
        let region = Region::from_rects(&a);
        prop_assert!(is_canonical(&region.boxes), "not canonical: {:?}", region.boxes);
        prop_assert_eq!(raster_boxes(&region.boxes, 0, 0), raster(&a));
        prop_assert_eq!(region.to_rects(), canonical(a));
    }

    /// The same near the wire's limits, where boxes end past `i16::MAX`:
    /// checked on the internal boxes, which are never clamped.
    #[test]
    fn from_rects_keeps_pixels_past_the_wire_limits(
        (bx, by) in (bases(), bases()),
        offsets in prop::collection::vec((-3i32..9, -3i32..9, 0u16..7, 0u16..7), 0..7),
    ) {
        let a: Vec<RegionRect> = offsets
            .into_iter()
            .map(|(x, y, w, h)| r(wire(bx + x), wire(by + y), w, h))
            .collect();
        let region = Region::from_rects(&a);
        prop_assert!(is_canonical(&region.boxes), "not canonical: {:?}", region.boxes);
        let rects: Vec<Box32> = a.iter().map(|r| Box32::from_rect(*r)).collect();
        prop_assert_eq!(raster_boxes(&region.boxes, bx, by), raster_boxes(&rects, bx, by), "a={:?}", a);
    }

    #[test]
    fn canonical_rects_are_adopted_as_they_are(a in rects()) {
        let c = canonical(a);
        prop_assert!(is_canonical(&Region::from_rects(&c).boxes));
        prop_assert_eq!(Region::from_rects(&c).to_rects(), c);
    }
}

/// `rects`' canonical region with every box cut in two at its middle
/// column and every band in two at its middle row: still YX-banded, as
/// `yx_banded` checks, but with touching boxes and equal adjacent bands.
fn uncoalesced(rects: &[RegionRect]) -> Vec<RegionRect> {
    let mut out = Vec::new();
    for b in &Region::from_rects(rects).boxes {
        let (ym, xm) = ((b.y1 + b.y2) / 2, (b.x1 + b.x2) / 2);
        for (y1, y2) in [(b.y1, ym), (ym, b.y2)] {
            for (x1, x2) in [(b.x1, xm), (xm, b.x2)] {
                if x1 < x2 && y1 < y2 {
                    out.push(Box32 { x1, y1, x2, y2 }.to_rect());
                }
            }
        }
    }
    // Back into band order: the cut made two bands of each.
    out.sort_by_key(|r| (r.y, r.x));
    out
}

proptest! {
    /// A YXBanded region taken as given gives the operations the same
    /// pixels. (Like pixman's, the results need not be canonical: bands
    /// only one side covers are copied as they are.)
    #[test]
    fn yx_banded_regions_take_part_in_the_operations(a in rects(), b in rects()) {
        let banded = uncoalesced(&a);
        prop_assert!(yx_banded(&banded), "not banded: {:?}", banded);
        let (ra, rb) = (Region::from_yx_banded(&banded), Region::from_rects(&b));
        prop_assert_eq!(raster(&ra.to_rects()), raster(&a));
        let cells = |f: fn(bool, bool) -> bool| -> Vec<bool> {
            raster(&a).into_iter().zip(raster(&b)).map(|(p, q)| f(p, q)).collect()
        };
        let union = ra.union(&rb);
        prop_assert_eq!(raster(&union.to_rects()), cells(|p, q| p || q));
        prop_assert_eq!(raster(&ra.intersect(&rb).to_rects()), cells(|p, q| p && q));
        let sub = ra.subtract(&rb);
        prop_assert_eq!(raster(&sub.to_rects()), cells(|p, q| p && !q));
        let rsub = rb.subtract(&ra);
        prop_assert_eq!(raster(&rsub.to_rects()), cells(|p, q| q && !p));
    }
}

/// pixman merges bands whose spans agree, and a hole splits a rect into a
/// top band, two sides and a bottom band.
#[test]
fn coalescing_follows_pixman() {
    let a = Region::from_rects(&[r(0, 0, 10, 5)]);
    let b = Region::from_rects(&[r(0, 5, 10, 5)]);
    assert_eq!(a.union(&b).to_rects(), vec![r(0, 0, 10, 10)]);
    let whole = Region::from_rects(&[r(0, 0, 10, 10)]);
    let hole = Region::from_rects(&[r(3, 3, 4, 4)]);
    assert_eq!(
        whole.subtract(&hole).to_rects(),
        vec![r(0, 0, 10, 3), r(0, 3, 3, 4), r(7, 3, 3, 4), r(0, 7, 10, 3)]
    );
    assert_eq!(whole.subtract(&hole).union(&hole), whole);
}

/// Borders left of and above the parent, extents at the wire's limits:
/// the same rects as the old operations, as long as no piece the old
/// operations cut off starts past `i16::MAX` (see
/// [`a_hole_past_the_wire_origin_limit_is_kept`]).
#[test]
fn wire_limits_match_the_old_operations() {
    let cases = [
        (vec![r(-5, -5, 20, 20)], vec![r(-10, 0, 8, 3)]),
        (
            vec![r(i16::MIN, i16::MIN, u16::MAX, u16::MAX)],
            vec![r(0, 0, 10, 10)],
        ),
        (
            vec![r(i16::MAX - 4, 0, 100, 10)],
            vec![r(i16::MAX - 2, 2, 1, 1)],
        ),
        (
            vec![r(0, 0, i16::MAX as u16, 1)],
            vec![r(100, 0, 1, 1), r(i16::MAX - 5, 0, 5, 1)],
        ),
        (vec![r(0, 0, 0, 5)], vec![r(0, 0, 5, 5)]),
    ];
    for (a, b) in cases {
        let (ra, rb) = (Region::from_rects(&a), Region::from_rects(&b));
        assert_eq!(
            ra.subtract(&rb).to_rects(),
            canonical(nested::subtract_regions(&a, &b)),
            "subtract {a:?} {b:?}"
        );
        assert_eq!(
            ra.intersect(&rb).to_rects(),
            canonical(nested::intersect_regions(&a, &b)),
            "intersect {a:?} {b:?}"
        );
    }
}

/// A hole starting past `i16::MAX` stays a hole in the region; only the
/// rects sent out are clamped to the wire.
#[test]
fn a_hole_past_the_wire_origin_limit_is_kept() {
    let a = Region::from_rects(&[r(0, 0, u16::MAX, 1)]);
    let b = Region::from_rects(&[r(100, 0, 1, 1), r(i16::MAX, 0, 5, 1)]);
    let b32 = |x1, x2| Box32 {
        x1,
        y1: 0,
        x2,
        y2: 1,
    };
    // The old operations lost this hole by clamping mid-operation.
    assert_eq!(
        a.subtract(&b).boxes,
        vec![b32(0, 100), b32(101, 32767), b32(32772, 65535)]
    );
}

#[test]
fn empty_regions() {
    let e = Region::default();
    let a = Region::from_rects(&[r(1, 1, 2, 2)]);
    assert_eq!(e.union(&a), a);
    assert_eq!(a.union(&e), a);
    assert!(e.intersect(&a).is_empty());
    assert!(a.intersect(&e).is_empty());
    assert_eq!(a.subtract(&e), a);
    assert!(e.subtract(&a).is_empty());
    assert!(Region::from_rects(&[r(0, 0, 0, 0)]).is_empty());
}

/// Input whose pieces start past `i16::MAX` keeps every pixel and stays
/// canonical: the constructor never goes through the wire's range.
#[test]
fn arbitrary_input_keeps_internal_bands_above_wire_origin_limit() {
    let input = [r(0, 32760, 10, 20), r(20, 32767, 10, 10)];
    let region = Region::from_rects(&input);
    for y in 32760..32780 {
        for x in 0..30 {
            let want = input.iter().any(|r| {
                let b = Box32::from_rect(*r);
                b.x1 <= x && x < b.x2 && b.y1 <= y && y < b.y2
            });
            let got = region
                .boxes
                .iter()
                .any(|b| b.x1 <= x && x < b.x2 && b.y1 <= y && y < b.y2);
            assert_eq!(
                got, want,
                "constructor changed the pixel set at ({x},{y}): {:?}",
                region.boxes
            );
        }
    }
    assert!(
        is_canonical(&region.boxes),
        "constructor must always return canonical boxes"
    );
}

fn bx(x1: i32, y1: i32, x2: i32, y2: i32) -> Box32 {
    Box32 { x1, y1, x2, y2 }
}

/// `pixman_region_translate` (16-bit) results, from a run against the
/// system pixman: boxes clipped at both edges, wholly moved-out boxes
/// dropped, bands not coalesced again.
#[test]
fn protocol_translation_clips_like_pixman16() {
    let moved =
        |rects: &[RegionRect], dx, dy| Region::from_rects(rects).translate_clipped(dx, dy).boxes;
    let edge = [r(32760, 0, 7, 10)];
    assert_eq!(moved(&edge, 5, 0), [bx(32765, 0, 32767, 10)]);
    assert_eq!(
        moved(&[r(0, 32760, 10, 7)], 0, 5),
        [bx(0, 32765, 10, 32767)]
    );
    assert_eq!(
        moved(&[r(-32768, 0, 10, 10)], -5, 0),
        [bx(-32768, 0, -32763, 10)]
    );
    assert!(moved(&edge, 8, 0).is_empty());
    assert!(moved(&[r(-32768, 0, 10, 10)], -11, 0).is_empty());
    // Exactly onto the limit: pixman keeps a box without area; we drop it.
    assert!(moved(&edge, 7, 0).is_empty());
    assert!(moved(&[r(-32768, 0, 10, 10)], -10, 0).is_empty());

    // Dropping a band's far box leaves two equal bands, not merged.
    let multi = [r(0, 0, 10, 10), r(32760, 0, 7, 10), r(0, 10, 10, 10)];
    assert_eq!(Region::from_rects(&multi).boxes.len(), 3);
    assert_eq!(moved(&multi, 8, 0), [bx(8, 0, 18, 10), bx(8, 10, 18, 20)]);
    // Every box out on a different axis.
    let diag = [r(32000, 0, 767, 10), r(0, 32000, 10, 767)];
    assert!(moved(&diag, 1000, 1000).is_empty());
    let two = [r(0, 0, 10, 10), r(20, 0, 32747, 10)];
    assert_eq!(
        moved(&two, -15, 0),
        [bx(-15, 0, -5, 10), bx(5, 0, 32752, 10)]
    );
}

/// Translating back keeps only what the first move left: the clipped
/// pixels do not come back.
#[test]
fn protocol_translation_and_back_keeps_the_clipped_part() {
    let back = Region::from_rects(&[r(32760, 0, 7, 10)])
        .translate_clipped(5, 0)
        .translate_clipped(-5, 0);
    assert_eq!(back.boxes, [bx(32760, 0, 32762, 10)]);
    let back = Region::from_rects(&[r(-10, -10, 30, 30)])
        .translate_clipped(-32760, -32760)
        .translate_clipped(32760, 32760);
    assert_eq!(back.boxes, [bx(-8, -8, 20, 20)]);
}

/// Repeated moves by the largest offsets neither overflow nor bring
/// anything back.
#[test]
fn repeated_protocol_translation_saturates_to_empty() {
    let mut region = Region::from_rects(&[r(0, 0, 1, 1)]);
    for _ in 0..4 {
        region = region.translate_clipped(i32::from(i16::MAX), 0);
    }
    assert!(region.is_empty());
    let mut region = Region::from_rects(&[r(-32768, -32768, 65535, 65535)]);
    for _ in 0..4 {
        region = region.translate_clipped(i32::from(i16::MIN), i32::from(i16::MIN));
    }
    assert!(region.is_empty());
    let region = Region::from_rects(&[r(0, 0, 1, 1)]).translate_clipped(i32::MAX, i32::MIN);
    assert!(region.is_empty());
}
