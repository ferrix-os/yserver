//! `tools/xfixes-region-probe.c` replayed through the dispatcher, its
//! output compared line by line with the Xephyr run kept next to it
//! (`tools/xfixes-region-probe.xephyr.txt`). Every expected value here is
//! that Xorg output; nothing is derived from this server.

use super::*;

const GOLDEN: &str = include_str!("../../../../../../tools/xfixes-region-probe.xephyr.txt");

/// Golden lines this server does not reproduce, by tag, with the reason.
const KNOWN_DIVERGENT: &[&str] = &[
    // RegionExtents of an empty region: Xorg's `RegionReset` leaves a
    // region whose one box has no area, which FetchRegion reports as one
    // 0x0 rect; a canonical empty region has none.
    "extents.empty ",
    // The default Input shape: Xorg's is the bounding default, border
    // included (`CreateBoundingShape` for ShapeInput); ours excludes the
    // border, which nested.rs's
    // `the_default_shape_region_includes_the_border_for_bounding_only`
    // pins. A Subtract on an unset Input shape starts from it.
    "fresh.subtract.on2.k2 ",
    "input.sub.k2 ",
    "offset.c.k2 ",
    "mask.none.k2 ",
    // A translation that puts an edge exactly on the 16-bit limit: pixman
    // keeps the box with no area, `Region::translate_clipped` drops it.
    "edge.exact ",
];

type R = (i16, i16, u16, u16);

const V_ADJ: &[R] = &[(0, 0, 10, 5), (0, 5, 10, 5)];
const OVL: &[R] = &[(0, 0, 10, 10), (0, 5, 10, 10)];
const MULTI: &[R] = &[
    (20, 20, 10, 10),
    (0, 0, 10, 10),
    (5, 5, 10, 10),
    (25, 0, 5, 30),
];
const ZERO: &[R] = &[(0, 0, 0, 10), (5, 5, 10, 0), (1, 1, 2, 2)];
const NEG: &[R] = &[(-10, -10, 15, 15), (-5, 0, 5, 5)];
const H_ADJ: &[R] = &[(0, 0, 5, 5), (5, 0, 5, 5)];
const ELL: &[R] = &[(0, 0, 20, 5), (0, 5, 5, 15)];
const BIG: &[R] = &[(0, 0, 30, 30)];
const HOLE: &[R] = &[(10, 10, 10, 10)];
const CLAMP: &[R] = &[(32760, 32760, 100, 100)];
const STRIPES: &[R] = &[(0, 0, 30, 2), (0, 4, 30, 2), (0, 8, 30, 2)];
const COLS: &[R] = &[(0, 0, 2, 10), (4, 0, 2, 10), (8, 0, 2, 10)];
const FRESH: &[R] = &[(10, 10, 10, 10), (90, 70, 20, 20)];
const EDGE: &[R] = &[(32760, 0, 7, 10)];
const EDGE_Y: &[R] = &[(0, 32760, 10, 7)];
const LOW: &[R] = &[(-32768, 0, 10, 10)];
const BANDS: &[R] = &[(0, 0, 10, 10), (32760, 0, 7, 10), (0, 10, 10, 10)];
const DIAG: &[R] = &[(32000, 0, 767, 10), (0, 32000, 10, 767)];
const CORNER: &[R] = &[(-10, -10, 30, 30)];
const DOT: &[R] = &[(0, 0, 1, 1)];
const FULL: &[R] = &[(-32768, -32768, 65535, 65535)];

const SHAPE_MAJOR: u8 = 141;

fn xfixes_u32s(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

struct Twin {
    state: ServerState,
    backend: RecordingBackend,
    peer: UnixStream,
    out: Vec<String>,
    next: u32,
}

fn rect_bytes(rects: &[R]) -> Vec<u8> {
    let mut b = Vec::with_capacity(rects.len() * 8);
    for &(x, y, w, h) in rects {
        b.extend_from_slice(&x.to_le_bytes());
        b.extend_from_slice(&y.to_le_bytes());
        b.extend_from_slice(&w.to_le_bytes());
        b.extend_from_slice(&h.to_le_bytes());
    }
    b
}

fn fmt_rects(bytes: &[u8]) -> String {
    let mut s = format!(" n={}", bytes.len() / 8);
    for r in bytes.chunks_exact(8) {
        let i = |at: usize| i16::from_le_bytes([r[at], r[at + 1]]);
        let u = |at: usize| u16::from_le_bytes([r[at], r[at + 1]]);
        s.push_str(&format!(" [{},{} {}x{}]", i(0), i(2), u(4), u(6)));
    }
    s
}

impl Twin {
    fn new() -> Self {
        let mut state = ServerState::new();
        let peer = install_client(&mut state, 1);
        state.xfixes_client_major.insert(1, 5);
        Self {
            state,
            backend: RecordingBackend::new(),
            peer,
            out: Vec::new(),
            next: 0x0020_0000,
        }
    }

    fn id(&mut self) -> u32 {
        self.next += 1;
        self.next
    }

    /// Run one request; returns the replies (header + data) it produced
    /// and prints any error the way the probe's `check` does.
    fn req(&mut self, what: &str, major: u8, minor: u8, body: &[u8]) -> Vec<Vec<u8>> {
        process_request(
            &mut self.state,
            &mut self.backend,
            ClientId(1),
            SequenceNumber(1),
            RequestHeader {
                opcode: major,
                data: minor,
                length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
            },
            body,
            None,
        )
        .expect("request");
        let bytes = read_all_or_buffered(&mut self.state, 1, &mut self.peer);
        let mut replies = Vec::new();
        let mut at = 0;
        while at + 32 <= bytes.len() {
            let p = &bytes[at..];
            let len = if p[0] == 1 {
                32 + 4 * le_u32(p, 4) as usize
            } else {
                32
            };
            match p[0] {
                0 => self.out.push(format!(
                    "{what} ERR code={} minor={}",
                    p[1],
                    u16::from_le_bytes([p[8], p[9]])
                )),
                1 => replies.push(p[..len].to_vec()),
                _ => {}
            }
            at += len;
        }
        replies
    }

    fn xf(&mut self, what: &str, minor: u8, body: &[u8]) -> Vec<Vec<u8>> {
        self.req(what, XFIXES_MAJOR_OPCODE, minor, body)
    }

    fn mk(&mut self, rects: &[R]) -> u32 {
        let id = self.id();
        let mut body = id.to_le_bytes().to_vec();
        body.extend(rect_bytes(rects));
        self.xf("create", x11xfixes::CREATE_REGION, &body);
        id
    }

    fn destroy(&mut self, region: u32) {
        self.xf("destroy", x11xfixes::DESTROY_REGION, &region.to_le_bytes());
    }

    fn fetch(&mut self, tag: &str, region: u32) {
        let replies = self.xf(tag, x11xfixes::FETCH_REGION, &region.to_le_bytes());
        let Some(r) = replies.first() else {
            self.out.push(format!("{tag} fetch ERR"));
            return;
        };
        let e = &r[8..16];
        let i = |at: usize| i16::from_le_bytes([e[at], e[at + 1]]);
        let u = |at: usize| u16::from_le_bytes([e[at], e[at + 1]]);
        self.out.push(format!(
            "{tag} ext=[{},{} {}x{}] {}",
            i(0),
            i(2),
            u(4),
            u(6),
            fmt_rects(&r[32..])
        ));
    }

    fn op3(&mut self, what: &str, minor: u8, a: u32, b: u32, d: u32) {
        let body = xfixes_u32s(&[a, b, d]);
        self.xf(what, minor, &body);
    }

    fn mkwin(&mut self) -> u32 {
        let id = self.id();
        self.state.resources.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: ResourceId(id),
                parent: ROOT_WINDOW,
                x: 10,
                y: 10,
                width: 100,
                height: 80,
                border_width: 3,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
        id
    }

    fn get_shape(&mut self, tag: &str, w: u32, kind: u8) {
        let mut body = w.to_le_bytes().to_vec();
        body.extend_from_slice(&[kind, 0, 0, 0]);
        let replies = self.req(tag, SHAPE_MAJOR, x11shape::GET_RECTANGLES, &body);
        let Some(r) = replies.first() else {
            self.out.push(format!("{tag} get ERR"));
            return;
        };
        let n = le_u32(r, 8) as usize;
        self.out.push(format!(
            "{tag}.k{kind} ordering={}{}",
            r[1],
            fmt_rects(&r[32..32 + 8 * n])
        ));
    }

    fn get_all(&mut self, tag: &str, w: u32) {
        for k in 0..=2 {
            self.get_shape(tag, w, k);
        }
        let replies = self.req(tag, SHAPE_MAJOR, x11shape::QUERY_EXTENTS, &w.to_le_bytes());
        let r = &replies[0];
        let i = |at: usize| i16::from_le_bytes([r[at], r[at + 1]]);
        let u = |at: usize| u16::from_le_bytes([r[at], r[at + 1]]);
        self.out.push(format!(
            "{tag}.extents bshaped={} cshaped={} b=[{},{} {}x{}] c=[{},{} {}x{}]",
            r[8],
            r[9],
            i(12),
            i(14),
            u(16),
            u(18),
            i(20),
            i(22),
            u(24),
            u(26)
        ));
    }

    #[allow(clippy::too_many_arguments)]
    fn rects(
        &mut self,
        what: &str,
        w: u32,
        op: u8,
        kind: u8,
        ordering: u8,
        off: (i16, i16),
        r: &[R],
    ) {
        let mut body = vec![op, kind, ordering, 0];
        body.extend_from_slice(&w.to_le_bytes());
        body.extend_from_slice(&off.0.to_le_bytes());
        body.extend_from_slice(&off.1.to_le_bytes());
        body.extend(rect_bytes(r));
        self.req(what, SHAPE_MAJOR, x11shape::RECTANGLES, &body);
    }

    fn offset(&mut self, what: &str, kind: u8, w: u32, dx: i16, dy: i16) {
        let mut body = vec![kind, 0, 0, 0];
        body.extend_from_slice(&w.to_le_bytes());
        body.extend_from_slice(&dx.to_le_bytes());
        body.extend_from_slice(&dy.to_le_bytes());
        self.req(what, SHAPE_MAJOR, x11shape::OFFSET, &body);
    }

    #[allow(clippy::too_many_arguments)]
    fn combine(
        &mut self,
        what: &str,
        op: u8,
        dkind: u8,
        skind: u8,
        dest: u32,
        off: (i16, i16),
        src: u32,
    ) {
        let mut body = vec![op, dkind, skind, 0];
        body.extend_from_slice(&dest.to_le_bytes());
        body.extend_from_slice(&off.0.to_le_bytes());
        body.extend_from_slice(&off.1.to_le_bytes());
        body.extend_from_slice(&src.to_le_bytes());
        self.req(what, SHAPE_MAJOR, x11shape::COMBINE, &body);
    }

    fn mask_none(&mut self, what: &str, kind: u8, w: u32) {
        let mut body = vec![x11shape::OP_SET, kind, 0, 0];
        body.extend_from_slice(&w.to_le_bytes());
        body.extend_from_slice(&[0; 8]);
        self.req(what, SHAPE_MAJOR, x11shape::MASK, &body);
    }

    fn translate(&mut self, region: u32, dx: i16, dy: i16) {
        let mut body = region.to_le_bytes().to_vec();
        body.extend_from_slice(&dx.to_le_bytes());
        body.extend_from_slice(&dy.to_le_bytes());
        self.xf("translate", x11xfixes::TRANSLATE_REGION, &body);
    }

    /// Fetch `region` united with `HOLE`: an emptied region's extents are
    /// stale in Xorg, the union's are not.
    fn fetch_union_hole(&mut self, tag: &str, region: u32) {
        let (h, d) = (self.mk(HOLE), self.mk(&[]));
        self.op3("union", x11xfixes::UNION_REGION, region, h, d);
        self.fetch(tag, d);
        self.destroy(h);
        self.destroy(d);
    }

    fn region_from_window(&mut self, tag: &str, w: u32, kind: u8) {
        let d = self.id();
        let mut body = xfixes_u32s(&[d, w]);
        body.extend_from_slice(&[kind, 0, 0, 0]);
        self.xf("from_window", x11xfixes::CREATE_REGION_FROM_WINDOW, &body);
        self.fetch(tag, d);
        self.destroy(d);
    }
}

use yserver_protocol::x11::{shape as x11shape, xfixes as x11xfixes};

fn xfixes_section(t: &mut Twin) {
    t.out.push("== xfixes create/fetch".into());
    let inputs: &[(&str, &[R])] = &[
        ("v_adj", V_ADJ),
        ("ovl", OVL),
        ("multi", MULTI),
        ("empty", &[]),
        ("zero", ZERO),
        ("neg", NEG),
        ("h_adj", H_ADJ),
        ("ell", ELL),
        ("clamp", CLAMP),
        ("stripes", STRIPES),
        ("cols", COLS),
    ];
    for (name, rects) in inputs {
        let r = t.mk(rects);
        t.fetch(&format!("create.{name}"), r);
        t.destroy(r);
    }

    t.out.push("== xfixes ops".into());
    let pairs: &[(&str, &[R], &[R])] = &[
        ("vadj_halves", &V_ADJ[..1], &V_ADJ[1..]),
        ("big_hole", BIG, HOLE),
        ("multi_ovl", MULTI, OVL),
        ("multi_neg", MULTI, NEG),
        ("ell_big", ELL, BIG),
        ("stripes_cols", STRIPES, COLS),
        ("empty_big", &[], BIG),
        ("hadj_halves", &H_ADJ[..1], &H_ADJ[1..]),
    ];
    for (name, a, b) in pairs {
        let (a, b, d) = (t.mk(a), t.mk(b), t.mk(&[]));
        t.op3("union", x11xfixes::UNION_REGION, a, b, d);
        t.fetch(&format!("union.{name}"), d);
        t.op3("intersect", x11xfixes::INTERSECT_REGION, a, b, d);
        t.fetch(&format!("intersect.{name}"), d);
        t.op3("subtract", x11xfixes::SUBTRACT_REGION, a, b, d);
        t.fetch(&format!("subtract.{name}"), d);
        t.op3("subtract", x11xfixes::SUBTRACT_REGION, b, a, d);
        t.fetch(&format!("subtract_rev.{name}"), d);
        for r in [a, b, d] {
            t.destroy(r);
        }
    }

    let (a, b) = (t.mk(BIG), t.mk(HOLE));
    t.op3("subtract_inplace", x11xfixes::SUBTRACT_REGION, a, b, a);
    t.fetch("subtract_inplace.big_hole", a);
    t.op3("union_inplace", x11xfixes::UNION_REGION, a, b, a);
    t.fetch("union_inplace.back", a);
    t.destroy(a);
    t.destroy(b);

    let inv: &[(&str, &[R], R)] = &[
        ("multi", MULTI, (0, 0, 40, 40)),
        ("neg", NEG, (-20, -20, 30, 30)),
        ("empty", &[], (5, 5, 10, 10)),
        ("hole", HOLE, (0, 0, 30, 30)),
        ("clamp", BIG, (32000, 32000, 2000, 2000)),
        ("outside", HOLE, (100, 100, 5, 5)),
    ];
    for (name, rects, bounds) in inv {
        let (a, d) = (t.mk(rects), t.mk(&[]));
        let mut body = a.to_le_bytes().to_vec();
        body.extend(rect_bytes(&[*bounds]));
        body.extend_from_slice(&d.to_le_bytes());
        t.xf("invert", x11xfixes::INVERT_REGION, &body);
        t.fetch(&format!("invert.{name}"), d);
        t.destroy(a);
        t.destroy(d);
    }

    let a = t.mk(MULTI);
    let translate = |t: &mut Twin, dx: i16, dy: i16| {
        let mut body = a.to_le_bytes().to_vec();
        body.extend_from_slice(&dx.to_le_bytes());
        body.extend_from_slice(&dy.to_le_bytes());
        t.xf("translate", x11xfixes::TRANSLATE_REGION, &body);
    };
    translate(t, 3, -4);
    t.fetch("translate.multi", a);
    translate(t, -30, -30);
    t.fetch("translate.multi_neg", a);
    let d = t.mk(&[]);
    t.xf("extents", x11xfixes::REGION_EXTENTS, &xfixes_u32s(&[a, d]));
    t.fetch("extents.multi", d);
    t.destroy(a);
    let a = t.mk(&[]);
    t.xf("extents", x11xfixes::REGION_EXTENTS, &xfixes_u32s(&[a, d]));
    t.fetch("extents.empty", d);
    t.destroy(a);
    let a = t.mk(V_ADJ);
    t.xf("copy", x11xfixes::COPY_REGION, &xfixes_u32s(&[a, d]));
    t.fetch("copy.v_adj", d);
    let mut body = d.to_le_bytes().to_vec();
    body.extend(rect_bytes(OVL));
    t.xf("set", x11xfixes::SET_REGION, &body);
    t.fetch("set.ovl", d);
    t.destroy(a);
    t.destroy(d);
}

#[allow(clippy::too_many_lines)]
fn shape_section(t: &mut Twin) {
    use x11shape::{
        KIND_BOUNDING as B, KIND_CLIP as C, KIND_INPUT as I, OP_INTERSECT, OP_INVERT, OP_SET,
        OP_SUBTRACT, OP_UNION, ORDERING_UNSORTED as U, ORDERING_Y_SORTED as YS,
        ORDERING_YX_BANDED as BAND, ORDERING_YX_SORTED as YX,
    };
    t.out.push("== shape unshaped".into());
    let w = t.mkwin();
    t.get_all("unshaped", w);
    t.region_from_window("fromwin.unshaped.b", w, B);
    t.region_from_window("fromwin.unshaped.c", w, C);

    t.out.push("== shape ops on unshaped windows".into());
    for (name, op) in [
        ("set", OP_SET),
        ("union", OP_UNION),
        ("intersect", OP_INTERSECT),
        ("subtract", OP_SUBTRACT),
        ("invert", OP_INVERT),
    ] {
        for k in 0..=2 {
            let v = t.mkwin();
            let tag = format!("fresh.{name}.on{k}");
            t.rects(&tag, v, op, k, U, (0, 0), FRESH);
            t.get_all(&tag, v);
        }
    }

    t.out.push("== shape set orderings".into());
    t.rects("set.multi", w, OP_SET, B, U, (0, 0), MULTI);
    t.get_all("set.multi", w);
    t.region_from_window("fromwin.multi.b", w, B);
    t.rects("set.v_adj.unsorted", w, OP_SET, B, U, (0, 0), V_ADJ);
    t.get_shape("set.v_adj.unsorted", w, B);
    t.rects("set.v_adj.banded", w, OP_SET, B, BAND, (0, 0), V_ADJ);
    t.get_shape("set.v_adj.banded", w, B);
    t.region_from_window("fromwin.v_adj.banded", w, B);
    t.rects("set.h_adj.banded", w, OP_SET, B, BAND, (0, 0), H_ADJ);
    t.get_shape("set.h_adj.banded", w, B);
    t.rects("set.h_adj.yxsorted", w, OP_SET, B, YX, (0, 0), H_ADJ);
    t.get_shape("set.h_adj.yxsorted", w, B);
    t.rects("set.multi.yxsorted_bad", w, OP_SET, B, YX, (0, 0), MULTI);
    t.rects("set.ovl.banded_bad", w, OP_SET, B, BAND, (0, 0), OVL);
    t.rects("set.ovl.ysorted", w, OP_SET, B, YS, (0, 0), OVL);
    t.get_shape("set.ovl.ysorted", w, B);
    t.rects("set.zero", w, OP_SET, B, U, (0, 0), ZERO);
    t.get_shape("set.zero", w, B);
    t.rects("set.empty", w, OP_SET, B, U, (0, 0), &[]);
    t.get_all("set.empty", w);
    t.rects("set.neg.off", w, OP_SET, B, U, (7, -3), NEG);
    t.get_shape("set.neg.off", w, B);

    t.out.push("== shape ops on a shaped window".into());
    t.rects("big", w, OP_SET, B, U, (0, 0), BIG);
    t.rects("sub.hole", w, OP_SUBTRACT, B, U, (0, 0), HOLE);
    t.get_shape("sub.hole", w, B);
    t.rects("union.hole", w, OP_UNION, B, U, (0, 0), HOLE);
    t.get_shape("union.hole", w, B);
    t.rects("union.multi", w, OP_UNION, B, U, (5, 5), MULTI);
    t.get_shape("union.multi", w, B);
    t.rects("intersect.stripes", w, OP_INTERSECT, B, U, (0, 0), STRIPES);
    t.get_shape("intersect.stripes", w, B);
    t.rects("union.stripes_gaps", w, OP_UNION, B, U, (0, 2), STRIPES);
    t.get_shape("union.stripes_gaps", w, B);
    t.rects("invert.big", w, OP_INVERT, B, U, (0, 0), BIG);
    t.get_shape("invert.big", w, B);
    t.rects("invert.ell", w, OP_INVERT, B, U, (0, 0), ELL);
    t.get_shape("invert.ell", w, B);
    t.rects("sub.all", w, OP_SUBTRACT, B, U, (-100, -100), BIG);
    t.get_shape("sub.all_offscreen", w, B);
    t.offset("offset", B, w, -4, 6);
    t.get_shape("offset.b", w, B);
    t.get_all("offset.all", w);

    t.out.push("== clip and input kinds".into());
    t.rects("clip.set", w, OP_SET, C, U, (0, 0), MULTI);
    t.rects("clip.sub", w, OP_SUBTRACT, C, U, (0, 0), HOLE);
    t.get_shape("clip.sub", w, C);
    t.region_from_window("fromwin.clip", w, C);
    t.rects("input.union_unshaped", w, OP_UNION, I, U, (0, 0), HOLE);
    t.get_shape("input.union_unshaped", w, I);
    t.rects("input.sub", w, OP_SUBTRACT, I, U, (0, 0), HOLE);
    t.get_shape("input.sub", w, I);
    t.offset("offset.c", C, w, 2, 2);
    t.get_all("offset.c", w);

    t.out.push("== combine from another window".into());
    let src = t.mkwin();
    t.rects("src.set", src, OP_SET, B, U, (0, 0), ELL);
    t.combine("combine.union", OP_UNION, B, B, w, (1, 1), src);
    t.get_shape("combine.union", w, B);
    let unshaped = t.mkwin();
    t.combine("combine.set_unshaped", OP_SET, B, C, w, (0, 0), unshaped);
    t.get_shape("combine.set_unshaped_clip", w, B);
    t.mask_none("mask.none", B, w);
    t.get_all("mask.none", w);
}

#[allow(clippy::too_many_lines)]
fn translate_section(t: &mut Twin) {
    use x11shape::{KIND_BOUNDING as B, OP_SET, ORDERING_UNSORTED as U};
    t.out.push("== translate limits".into());
    let a = t.mk(EDGE);
    t.translate(a, 5, 0);
    t.fetch("edge.right", a);
    t.translate(a, -5, 0);
    t.fetch("edge.right.back", a);
    t.destroy(a);
    let a = t.mk(EDGE_Y);
    t.translate(a, 0, 5);
    t.fetch("edge.down", a);
    t.destroy(a);
    let a = t.mk(LOW);
    t.translate(a, -5, 0);
    t.fetch("edge.left", a);
    t.translate(a, 5, 0);
    t.fetch("edge.left.back", a);
    t.destroy(a);
    let a = t.mk(EDGE);
    t.translate(a, 7, 0);
    t.fetch("edge.exact", a);
    t.destroy(a);
    let a = t.mk(EDGE);
    t.translate(a, 8, 0);
    t.fetch_union_hole("edge.gone", a);
    t.destroy(a);
    let a = t.mk(BANDS);
    t.fetch("bands", a);
    t.translate(a, 8, 0);
    t.fetch("bands.right", a);
    t.destroy(a);
    let a = t.mk(DIAG);
    t.translate(a, 1000, 1000);
    t.fetch_union_hole("diag.gone", a);
    t.destroy(a);
    let a = t.mk(CORNER);
    t.translate(a, -32760, -32760);
    t.fetch("corner.low", a);
    t.translate(a, 32760, 32760);
    t.fetch("corner.low.back", a);
    t.destroy(a);
    let a = t.mk(DOT);
    for _ in 0..65540 {
        t.translate(a, 32767, 0);
    }
    t.fetch_union_hole("repeat.right", a);
    t.destroy(a);
    let a = t.mk(FULL);
    t.fetch("full", a);
    for _ in 0..4 {
        t.translate(a, -32768, -32768);
    }
    t.fetch_union_hole("repeat.full", a);
    t.destroy(a);

    t.out.push("== shape offsets at the limits".into());
    let w = t.mkwin();
    t.rects("rect.off", w, OP_SET, B, U, (32760, 0), BIG);
    t.get_shape("rect.off", w, B);
    t.offset("offset", B, w, -32760, 0);
    t.get_shape("offset.back", w, B);
    t.offset("offset", B, w, 32765, 0);
    t.get_shape("offset.clip", w, B);
    t.offset("offset", B, w, 5, 0);
    t.get_shape("offset.gone", w, B);
    let src = t.mkwin();
    t.rects("src.set", src, OP_SET, B, U, (0, 0), ELL);
    t.combine("combine.off", OP_SET, B, B, w, (32760, 32760), src);
    t.get_shape("combine.off", w, B);
    let a = t.mk(CORNER);
    let mut body = w.to_le_bytes().to_vec();
    body.extend_from_slice(&[B, 0, 0, 0]);
    body.extend_from_slice(&(-32760i16).to_le_bytes());
    body.extend_from_slice(&(-32760i16).to_le_bytes());
    body.extend_from_slice(&a.to_le_bytes());
    t.xf(
        "xfixes_shape.off",
        x11xfixes::SET_WINDOW_SHAPE_REGION,
        &body,
    );
    t.get_shape("xfixes_shape.off", w, B);
    t.destroy(a);
    // A 30x30 depth-1 pixmap, all set in the probe; without a pixmap
    // mirror the dispatcher takes its bounds, the same region.
    let p = t.id();
    t.state.resources.create_pixmap(
        ClientId(1),
        CreatePixmapRequest {
            pixmap: ResourceId(p),
            drawable: ROOT_WINDOW,
            width: 30,
            height: 30,
            depth: 1,
        },
    );
    let mut body = vec![OP_SET, B, 0, 0];
    body.extend_from_slice(&w.to_le_bytes());
    body.extend_from_slice(&32760i16.to_le_bytes());
    body.extend_from_slice(&0i16.to_le_bytes());
    body.extend_from_slice(&p.to_le_bytes());
    t.req("mask.off", SHAPE_MAJOR, x11shape::MASK, &body);
    t.get_shape("mask.off", w, B);
}

/// Compare `out` with the golden lines up to (not including) the line
/// `until`, skipping [`KNOWN_DIVERGENT`] tags.
fn compare(out: &[String], until: Option<&str>) {
    let golden: Vec<&str> = GOLDEN
        .lines()
        .take_while(|l| until.is_none_or(|u| *l != u))
        .collect();
    let skip = |l: &str| KNOWN_DIVERGENT.iter().any(|t| l.starts_with(t));
    let mut diffs = Vec::new();
    for (i, g) in golden.iter().enumerate() {
        let ours = out.get(i).map_or("<missing>", String::as_str);
        if ours != *g && !skip(g) {
            diffs.push(format!("line {}:\n  xephyr  {g}\n  yserver {ours}", i + 1));
        }
    }
    if out.len() > golden.len() {
        diffs.push(format!(
            "extra lines from {}: {}",
            golden.len() + 1,
            out[golden.len()]
        ));
    }
    assert!(
        diffs.is_empty(),
        "{} divergences:\n{}",
        diffs.len(),
        diffs.join("\n")
    );
}

#[test]
fn xfixes_regions_match_xephyr() {
    let mut t = Twin::new();
    xfixes_section(&mut t);
    compare(&t.out, Some("== shape unshaped"));
}

#[test]
fn shape_regions_match_xephyr() {
    let mut t = Twin::new();
    xfixes_section(&mut t);
    shape_section(&mut t);
    compare(&t.out, Some("== translate limits"));
}

#[test]
fn translations_at_the_limits_match_xephyr() {
    let mut t = Twin::new();
    xfixes_section(&mut t);
    shape_section(&mut t);
    translate_section(&mut t);
    compare(&t.out, None);
}

/// Repeated translations by the largest offset end in the empty region
/// (pixman drops what leaves the 16-bit range) and cannot overflow.
#[test]
fn repeated_translation_cannot_overflow() {
    let mut t = Twin::new();
    let a = t.mk(DOT);
    for _ in 0..65540 {
        t.translate(a, 32767, 0);
    }
    assert!(t.state.xfixes_regions[&a].region.is_empty());
}

/// TranslateRegion clips at the 16-bit limit, as `pixman_region_translate`
/// does (`edge.right` in the golden).
#[test]
fn translate_clips_at_the_16_bit_limit() {
    let mut t = Twin::new();
    let a = t.mk(EDGE);
    t.translate(a, 5, 0);
    t.fetch("clipped", a);
    assert_eq!(t.out, ["clipped ext=[32765,0 2x10]  n=1 [32765,0 2x10]"]);
}
