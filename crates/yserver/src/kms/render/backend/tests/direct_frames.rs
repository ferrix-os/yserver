//! Direct frames on the card-less KMS platform, with a live renderer: a
//! whole-window present of a client's LINEAR DRI3 buffer is owed to the
//! window's backing and handed to the rootless host; whatever touches the
//! backing settles it first, and the buffer is idle only once nothing can
//! read it.
//!
//! The client's buffer is a dma-buf made with `/dev/udmabuf` over a memfd,
//! imported through the backend's DRI3 import and registered in the core as
//! `PixmapFromBuffers` registers it; its pixels are written through the
//! memfd, as the client's GPU would. Most presents enter where the core's
//! `execute_present_pixmap_copy` enters, `Backend::try_present_direct`;
//! two tests send `PixmapFromBuffers` and `PresentPixmap` through
//! `process_request`, as the drawing, reading and window requests are.

use std::{
    cell::RefCell,
    collections::HashSet,
    os::{fd::OwnedFd, unix::fs::FileExt},
    rc::Rc,
    time::{Duration, Instant},
};

use super::KmsBackend;
use crate::rootless::{
    display::{DisplayHost, HostScreen, HostWork},
    host::{CpuFrame, DirectBuffer, DirectFrame, FrameEvent, HandoffKind, RootlessHost},
};
use yserver_core::{
    backend::{
        Backend, BackendTopology, CompletedPresentEvent, Dri3ImportModifier,
        PresentScanoutCandidate, PresentWake,
    },
    core_loop::process_request,
    resources::ROOT_WINDOW,
    rootless::{RootlessEvent, ToplevelId},
    server::ServerState,
};
use yserver_protocol::x11::{ClientId, RequestHeader, ResourceId, SequenceNumber};

const CLIENT: u32 = 23;
const W: u16 = 64;
const H: u16 = 48;
const STRIDE: u32 = W as u32 * 4;
const LINEAR: u64 = crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR;
const RED: [u8; 4] = [0, 0, 0xff, 0xff];
const BLUE: [u8; 4] = [0xff, 0, 0, 0xff];
/// The fill drawn after a present, `0x00ff00` (BGRX in memory).
const GREEN: [u8; 3] = [0, 0xff, 0];
const GC: u32 = 0x0017_0001;

/// What the host was asked, and how it answers.
#[derive(Default)]
struct DirectHost {
    /// Takes no direct frames at all (a compositor that names no format).
    refuse: bool,
    /// Decline every direct frame (no buffer object yet, or refused).
    decline: bool,
    /// The generations of the buffers it was given.
    imported: Vec<u64>,
    /// Windows that get no `Ready` after a frame: hidden, say.
    silent: HashSet<ToplevelId>,
    /// CPU frames: the window and its pixels.
    cpu: Vec<(ToplevelId, Vec<u8>)>,
    /// Direct frames taken: the window and the buffer's generation.
    direct: Vec<(ToplevelId, u64)>,
    /// Direct frames offered, taken or not.
    offered: usize,
    forgotten: Vec<u64>,
    queued: HostWork,
}

impl RootlessHost for DirectHost {
    fn handoff(&self, _id: ToplevelId, _depth: u8) -> HandoffKind {
        HandoffKind::Cpu
    }

    fn create_surface(
        &mut self,
        _id: ToplevelId,
        _vk: &crate::kms::vk::device::VkContext,
    ) -> std::io::Result<ash::vk::SurfaceKHR> {
        Err(std::io::Error::other("CPU frames"))
    }

    fn submit_cpu(&mut self, frame: CpuFrame) {
        let CpuFrame {
            id, seq, pixels, ..
        } = frame;
        self.cpu.push((id, pixels.clone()));
        self.queued.frame(FrameEvent::Delivered { id, seq });
        if !self.silent.contains(&id) {
            self.queued.frame(FrameEvent::Ready { id });
        }
        self.queued.frame(FrameEvent::Released { id, seq, pixels });
    }

    fn first_frame(&mut self, _id: ToplevelId) {}

    fn on_events(&mut self, events: &[RootlessEvent]) {
        for event in events {
            if let RootlessEvent::Mapped { id, .. } = event {
                self.queued.frame(FrameEvent::SetupDone { id: *id });
            }
        }
    }

    fn takes_direct(&self, depth: u8) -> bool {
        !self.refuse && (depth == 24 || depth == 32)
    }

    fn import_direct(&mut self, buffer: DirectBuffer) {
        // The buffer as the client described it, a dma-buf that holds it.
        let layout = super::super::kms::direct_frames::Layout {
            offset: buffer.offset,
            stride: buffer.stride,
            width: buffer.width,
            height: buffer.height,
            depth: buffer.depth,
        };
        assert_eq!(
            (buffer.width, buffer.height, buffer.stride, buffer.depth),
            (W, H, STRIDE, 24)
        );
        assert_eq!(
            super::super::kms::direct_frames::check_buffer(&buffer.fd, layout),
            Ok(())
        );
        assert!(
            !self.imported.contains(&buffer.generation),
            "a buffer given twice"
        );
        self.imported.push(buffer.generation);
    }

    fn submit_direct(&mut self, frame: DirectFrame) -> bool {
        self.offered += 1;
        assert!(
            self.imported.contains(&frame.generation),
            "a frame of a buffer it was not given"
        );
        assert_eq!((frame.width, frame.height), (W, H));
        if self.decline {
            return false;
        }
        self.direct.push((frame.id, frame.generation));
        if !self.silent.contains(&frame.id) {
            self.queued.frame(FrameEvent::Ready { id: frame.id });
        }
        true
    }

    fn forget_direct(&mut self, generations: &[u64]) {
        self.forgotten.extend_from_slice(generations);
    }
}

impl DisplayHost for DirectHost {
    fn fd(&self) -> std::os::fd::RawFd {
        -1
    }

    fn dispatch(&mut self, _work: &mut HostWork) -> std::io::Result<()> {
        Ok(())
    }

    fn take_queued(&mut self, work: &mut HostWork) {
        work.append(std::mem::take(&mut self.queued));
    }

    fn screen(&self) -> Option<HostScreen> {
        None
    }
}

/// A client buffer: the pixmap's host XID, and the memfd its pixels are.
struct Buffer {
    pixmap: u32,
    client_pixmap: u32,
    pixels: std::fs::File,
}

struct Fixture {
    b: KmsBackend,
    state: ServerState,
    host: Rc<RefCell<DirectHost>>,
    _peer: std::os::unix::net::UnixStream,
    /// Completions and idles the core was given, by present id.
    completed: Vec<u64>,
    idled: Vec<u64>,
    next_present: u64,
    next_pixmap: u32,
}

/// A rootless server on a live renderer whose host takes direct frames;
/// `None` (the test skips) without Vulkan or `/dev/udmabuf`.
fn fixture() -> Option<Fixture> {
    if super::super::kms::direct_frames::tests::udmabuf(4096).is_none() {
        eprintln!("skip: no /dev/udmabuf to make a client's dma-buf with");
        return None;
    }
    let mut b = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skip: no Vk: {e}");
            return None;
        }
    };
    let mut state = BackendTopology::from_backend(&mut b).into_server_state();
    yserver_core::backend::install_backend_root_bindings(&mut state, &b);
    let peer = super::kbd_map_client_id(&mut state, CLIENT);
    let host = Rc::new(RefCell::new(DirectHost::default()));
    b.attach_rootless_host(host.clone()).expect("attach");
    b.start_rootless(&mut state).expect("rootless");
    Some(Fixture {
        b,
        state,
        host,
        _peer: peer,
        completed: Vec::new(),
        idled: Vec::new(),
        next_present: 1,
        next_pixmap: 0x0016_0001,
    })
}

impl Fixture {
    fn request(&mut self, opcode: u8, body: &[u8]) {
        self.request_with(opcode, 0, body, None);
    }

    /// A request with its header's data byte (an extension's minor
    /// opcode) and a descriptor passed with it.
    fn request_with(&mut self, opcode: u8, data: u8, body: &[u8], fd: Option<OwnedFd>) {
        process_request::process_request(
            &mut self.state,
            &mut self.b,
            ClientId(CLIENT),
            SequenceNumber(1),
            RequestHeader {
                opcode,
                data,
                length_units: u32::try_from(1 + body.len() / 4).expect("length"),
            },
            body,
            fd,
        )
        .expect("process_request");
        yserver_core::rootless::reconcile_and_dispatch(&mut self.state, &mut self.b);
    }

    /// A client's `W`×`H` LINEAR buffer at depth 24 made the client's
    /// pixmap `pixmap` with DRI3's `PixmapFromBuffers`, filled with `bgra`:
    /// the memfd its pixels are.
    fn pixmap_from_buffers(&mut self, pixmap: u32, window: u32, bgra: [u8; 4]) -> std::fs::File {
        let (dmabuf, memfd) = super::super::kms::direct_frames::tests::udmabuf_with_memfd(
            (u64::from(STRIDE) * u64::from(H)).next_multiple_of(4096),
        )
        .expect("udmabuf");
        let pixels = std::fs::File::from(memfd);
        pixels
            .write_all_at(&bgra.repeat(usize::from(W) * usize::from(H)), 0)
            .expect("fill");
        let mut body = vec![0_u8; 60];
        body[0..4].copy_from_slice(&pixmap.to_le_bytes());
        body[4..8].copy_from_slice(&window.to_le_bytes());
        body[8] = 1;
        body[12..14].copy_from_slice(&W.to_le_bytes());
        body[14..16].copy_from_slice(&H.to_le_bytes());
        body[16..20].copy_from_slice(&STRIDE.to_le_bytes());
        body[48] = 24;
        body[49] = 32;
        body[52..60].copy_from_slice(&LINEAR.to_le_bytes());
        // DRI3 (147) PixmapFromBuffers (7).
        self.request_with(147, 7, &body, Some(dmabuf));
        pixels
    }

    /// `PresentPixmap` of the client's `pixmap` to its `window` at
    /// `(x_off, 0)`, as soon as it can.
    fn present_pixmap_request(&mut self, window: u32, pixmap: u32, serial: u32, x_off: i16) {
        let mut body = vec![0_u8; 68];
        body[0..4].copy_from_slice(&window.to_le_bytes());
        body[4..8].copy_from_slice(&pixmap.to_le_bytes());
        body[8..12].copy_from_slice(&serial.to_le_bytes());
        body[20..22].copy_from_slice(&x_off.to_le_bytes());
        // Present (145) PresentPixmap (1).
        self.request_with(145, yserver_protocol::x11::present::PIXMAP, &body, None);
    }

    /// A mapped `W`×`H` top-level, shown (its first frame read back and
    /// delivered) and ready for the next: its id and host XID.
    fn window(&mut self, wid: u32, x: i16) -> (ToplevelId, u32) {
        let body = [
            wid.to_le_bytes().as_slice(),
            &ROOT_WINDOW.0.to_le_bytes(),
            &x.to_le_bytes(),
            &0i16.to_le_bytes(),
            &W.to_le_bytes(),
            &H.to_le_bytes(),
            &0u16.to_le_bytes(),
            &1u16.to_le_bytes(),
            &0u32.to_le_bytes(),
            // CWBackPixel black, CWBitGravity NorthWest: a resize keeps the
            // content.
            &0x0012u32.to_le_bytes(),
            &0u32.to_le_bytes(),
            &1u32.to_le_bytes(),
        ]
        .concat();
        self.request(1, &body);
        self.request(8, &wid.to_le_bytes());
        let id = self
            .state
            .rootless
            .as_ref()
            .and_then(|r| r.toplevel_id(ResourceId(wid)))
            .expect("a top-level");
        let host_xid = self
            .state
            .resources
            .window(ResourceId(wid))
            .and_then(|w| w.host_xid)
            .expect("a backend window")
            .as_raw();
        self.pump_until("the first frame", |f| {
            f.host.borrow().cpu.iter().any(|(at, _)| *at == id)
        });
        // Its `Ready` applied.
        self.pump();
        (id, host_xid)
    }

    /// A client's `W`×`H` LINEAR buffer at depth 24, imported as DRI3's
    /// `PixmapFromBuffers` imports it, filled with `bgra`.
    fn buffer(&mut self, bgra: [u8; 4]) -> Buffer {
        self.buffer_at(bgra, 0)
    }

    fn buffer_at(&mut self, bgra: [u8; 4], offset: u32) -> Buffer {
        let size = u64::from(offset) + u64::from(STRIDE) * u64::from(H);
        let (dmabuf, memfd) = super::super::kms::direct_frames::tests::udmabuf_with_memfd(
            size.next_multiple_of(4096),
        )
        .expect("udmabuf");
        let handle = Backend::dri3_import_pixmap(
            &mut self.b,
            dmabuf,
            W,
            H,
            STRIDE,
            offset,
            Dri3ImportModifier::Explicit(LINEAR),
            24,
            32,
        )
        .expect("the DRI3 import");
        let client_pixmap = self.next_pixmap;
        self.next_pixmap += 1;
        self.state.resources.create_pixmap(
            ClientId(CLIENT),
            yserver_protocol::x11::CreatePixmapRequest {
                pixmap: ResourceId(client_pixmap),
                drawable: ROOT_WINDOW,
                width: W,
                height: H,
                depth: 24,
            },
        );
        let _ = self
            .state
            .resources
            .set_pixmap_host_xid(ResourceId(client_pixmap), handle);
        let buffer = Buffer {
            pixmap: handle.as_raw(),
            client_pixmap,
            pixels: std::fs::File::from(memfd),
        };
        let bytes = bgra.repeat(usize::from(W) * usize::from(H));
        buffer
            .pixels
            .write_all_at(&bytes, u64::from(offset))
            .expect("fill");
        buffer
    }

    fn candidate(&self, src: u32, window: u32) -> PresentScanoutCandidate {
        PresentScanoutCandidate {
            client_id: CLIENT,
            client_pid: None,
            present_id: self.next_present,
            crtc_id: 0,
            crtc_epoch: 0,
            src_pixmap_xid: src,
            dst_window_xid: window,
            variable_refresh: false,
            src_host_xid: src,
            paint_dst_host_xid: window,
            completion_dst_host_xid: window,
            src_width: W,
            src_height: H,
            x_off: 0,
            y_off: 0,
            valid_region_xid: 0,
            update_region_xid: 0,
            update_is_full: true,
            explicit_sync: false,
            options: 0,
        }
    }

    /// Present `candidate` as the core executes it; its id, and whether it
    /// went direct.
    fn present_as(&mut self, candidate: PresentScanoutCandidate) -> (u64, bool) {
        let present_id = self.next_present;
        self.next_present += 1;
        let event = CompletedPresentEvent {
            client_id: ClientId(CLIENT),
            serial: u32::try_from(present_id).expect("serial"),
            host_xid: candidate.src_host_xid,
            dst_host_xid: candidate.paint_dst_host_xid,
            options: candidate.options,
            present_id,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        };
        let taken =
            Backend::try_present_direct(&mut self.b, candidate, event).expect("try_present_direct");
        (present_id, taken)
    }

    fn present(&mut self, buffer: &Buffer, window: u32) -> u64 {
        let candidate = self.candidate(buffer.pixmap, window);
        let (id, taken) = self.present_as(candidate);
        assert!(
            taken,
            "a whole-window present of a LINEAR buffer goes direct"
        );
        id
    }

    /// One loop iteration's share: what the host queued, the presenter's
    /// tick, and the completions and idles the core would drain.
    fn pump(&mut self) {
        self.b.rootless_service(&mut self.state);
        self.b.rootless_tick();
        for event in Backend::drain_completed_present_events(&mut self.b) {
            self.completed.push(event.present_id);
        }
        for event in Backend::drain_retired_present_idle_events(&mut self.b) {
            self.idled.push(event.present_id);
        }
    }

    /// Pump until `done`, for at most two seconds (a readback or a copy on
    /// the GPU finishes by then).
    fn pump_until(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !done(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.pump();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A few iterations, so whatever would happen has.
    fn settle_down(&mut self) {
        for _ in 0..20 {
            self.pump();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// The window's pixel at `(x, y)` as `GetImage` reads it, BGR.
    fn pixel(&mut self, window: u32, x: i16, y: i16) -> [u8; 3] {
        // `Backend::get_image`, less the reply's 32-byte header.
        let bytes = self
            .b
            .get_image_pixels_for_tests(window, 2, x, y, 1, 1, !0)
            .expect("get_image")
            .expect("pixels");
        [bytes[0], bytes[1], bytes[2]]
    }

    /// A green rectangle drawn into `drawable` (the client's XID) through
    /// the core.
    fn fill_green(&mut self, drawable: u32, (x, y, w, h): (i16, i16, u16, u16)) {
        let body = [
            GC.to_le_bytes().as_slice(),
            &drawable.to_le_bytes(),
            // GCForeground.
            &0x0004u32.to_le_bytes(),
            &0x0000_ff00u32.to_le_bytes(),
        ]
        .concat();
        self.request(55, &body);
        let body = [
            drawable.to_le_bytes().as_slice(),
            &GC.to_le_bytes(),
            &x.to_le_bytes(),
            &y.to_le_bytes(),
            &w.to_le_bytes(),
            &h.to_le_bytes(),
        ]
        .concat();
        self.request(70, &body);
        self.request(60, &GC.to_le_bytes());
    }

    fn handed(&self, id: ToplevelId) -> usize {
        self.host
            .borrow()
            .direct
            .iter()
            .filter(|(at, _)| *at == id)
            .count()
    }

    fn generation(&self, id: ToplevelId) -> u64 {
        self.host
            .borrow()
            .direct
            .iter()
            .rev()
            .find(|(at, _)| *at == id)
            .map(|(_, generation)| *generation)
            .expect("a direct frame")
    }

    fn release(&mut self, generation: u64) {
        self.host
            .borrow_mut()
            .queued
            .frame(FrameEvent::DirectReleased { generation });
    }

    fn idles_of(&self, present_id: u64) -> usize {
        self.idled.iter().filter(|&&id| id == present_id).count()
    }

    fn owed_anything(&self) -> bool {
        self.b.pstate.direct_frames.is_owed_anything()
    }

    /// The last CPU frame of `id`'s pixel at `(x, y)`, BGR.
    fn shown(&self, id: ToplevelId, x: usize, y: usize) -> Option<[u8; 3]> {
        let host = self.host.borrow();
        let (_, pixels) = host.cpu.iter().rev().find(|(at, _)| *at == id)?;
        let at = (y * usize::from(W) + x) * 4;
        Some([pixels[at], pixels[at + 1], pixels[at + 2]])
    }
}

fn wid(n: u32) -> u32 {
    0x0015_0000 + n
}

/// (a) Present direct, then draw into the window, then read it: the draw is
/// there, on the frame it follows, and that frame is not put over it after.
/// Before, a draw after a direct present landed on the frame before it, and
/// the host kept showing the client's buffer over the draw.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_draw_after_a_direct_frame_lands_on_it_and_stays() {
    let Some(mut f) = fixture() else { return };
    let (id, window) = f.window(wid(1), 0);
    let red = f.buffer(RED);
    f.present(&red, window);
    f.pump();
    assert_eq!(f.handed(id), 1, "the host was handed the client's buffer");
    assert!(f.owed_anything());

    f.fill_green(wid(1), (10, 10, 20, 20));
    assert!(!f.owed_anything(), "the draw settled the frame first");
    assert_eq!(f.pixel(window, 15, 15), GREEN, "the draw");
    assert_eq!(f.pixel(window, 2, 2), RED[..3], "on the frame it follows");

    // The window is read back and shown with the draw; nothing puts the
    // frame back over it.
    f.pump_until("the window read back", |f| {
        f.shown(id, 15, 15) == Some(GREEN)
    });
    assert_eq!(f.shown(id, 2, 2), Some([0, 0, 0xff]));
    f.settle_down();
    assert_eq!(f.pixel(window, 15, 15), GREEN);
    assert_eq!(f.handed(id), 1, "no direct frame after the draw");
}

/// A `GetImage` of a window owed a frame the host was never handed (the
/// window busy) reads the frame, not the window before it.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_read_of_a_window_owed_a_frame_reads_the_frame() {
    let Some(mut f) = fixture() else { return };
    let (id, window) = f.window(wid(1), 0);
    f.host.borrow_mut().silent.insert(id);
    let red = f.buffer(RED);
    f.present(&red, window);
    f.pump();
    let blue = f.buffer(BLUE);
    f.present(&blue, window);
    f.pump();
    assert_eq!(f.handed(id), 1, "the window waits for the first's Ready");
    assert_eq!(f.pixel(window, 5, 5), BLUE[..3]);
    assert!(!f.owed_anything());
}

/// (b) The host gives a handed buffer back while the backing is still owed
/// it: no idle until it is settled, and the settle copies the frame.
/// Before, the release alone idled it, and the client could draw its next
/// frame into it before the backing copied it.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_buffer_given_back_while_still_owed_is_not_idle_until_settled() {
    let Some(mut f) = fixture() else { return };
    let (id, window) = f.window(wid(1), 0);
    let red = f.buffer(RED);
    let present = f.present(&red, window);
    f.pump();
    let generation = f.generation(id);
    f.release(generation);
    f.pump_until("the completion", |f| f.completed.contains(&present));
    f.settle_down();
    assert_eq!(f.idles_of(present), 0, "the backing may still copy it");

    assert_eq!(
        f.pixel(window, 30, 30),
        RED[..3],
        "the settle copied the frame"
    );
    f.pump_until("the idle", |f| f.idles_of(present) == 1);
    f.settle_down();
    assert_eq!(f.idles_of(present), 1, "once");
}

/// A buffer the host still holds is idle only once it is given back, even
/// after a newer present replaced it; and it is not handed again before.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_replaced_buffer_is_idle_once_the_host_gives_it_back() {
    let Some(mut f) = fixture() else { return };
    let (id, window) = f.window(wid(1), 0);
    let red = f.buffer(RED);
    let blue = f.buffer(BLUE);
    let first = f.present(&red, window);
    f.pump();
    let red_generation = f.generation(id);
    let second = f.present(&blue, window);
    f.pump();
    assert_eq!(f.handed(id), 2);
    f.pump_until("the completions", |f| {
        f.completed.contains(&first) && f.completed.contains(&second)
    });
    f.settle_down();
    assert_eq!(f.idles_of(first), 0, "the host holds it");
    // Red again while the host holds it: owed, not handed, so one release
    // still answers one hand-over; the window is read back instead.
    let third = f.present(&red, window);
    f.pump_until("the read back", |f| f.shown(id, 1, 1) == Some([0, 0, 0xff]));
    assert_eq!(f.handed(id), 2);
    f.release(red_generation);
    f.pump_until("the idles", |f| {
        f.idles_of(first) == 1 && f.idles_of(third) == 1
    });
    assert_eq!(f.idles_of(second), 0, "blue is still held");
}

/// (c) A window whose host never answers does not delay another window's
/// frames, and the Present clock does not wait for either.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_window_the_host_never_answers_holds_up_no_other() {
    let Some(mut f) = fixture() else { return };
    let (hidden, hidden_xid) = f.window(wid(1), 0);
    let (shown, shown_xid) = f.window(wid(2), 100);
    f.host.borrow_mut().silent.insert(hidden);
    let red = f.buffer(RED);
    let blue = f.buffer(BLUE);
    let red2 = f.buffer(RED);
    f.present(&red, hidden_xid);
    f.pump();
    // Owed again, to a window waiting for a `Ready` that never comes.
    f.present(&red2, hidden_xid);
    let clock = |f: &Fixture| {
        f.b.pstate
            .rootless
            .as_ref()
            .expect("rootless")
            .clock
            .sample(crate::rootless::present_clock::monotonic_us())
            .0
    };
    let msc = clock(&f);
    let other = f.present(&blue, shown_xid);
    // The loop comes round at once to hand it over.
    assert!(
        f.b.rootless_next_wakeup()
            .is_some_and(|at| at <= Instant::now())
    );
    f.pump();
    assert_eq!(f.handed(shown), 1, "handed in the same tick");
    assert_eq!(f.handed(hidden), 1, "the hidden window's second waits");
    f.pump_until("the other window's completion", |f| {
        f.completed.contains(&other)
    });
    std::thread::sleep(Duration::from_millis(40));
    assert!(clock(&f) > msc, "the Present clock runs on");
}

/// (d) A frame the host declines is settled and read back.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_declined_frame_is_settled_and_read_back() {
    let Some(mut f) = fixture() else { return };
    let (id, window) = f.window(wid(1), 0);
    f.host.borrow_mut().decline = true;
    let red = f.buffer(RED);
    let present = f.present(&red, window);
    f.pump_until("the read back", |f| f.shown(id, 3, 3) == Some([0, 0, 0xff]));
    assert_eq!(f.host.borrow().offered, 1);
    assert!(!f.owed_anything());
    f.pump_until("the idle", |f| f.idles_of(present) == 1);
}

/// (e) A window destroyed with a frame handed: nothing is owed, the idle
/// waits for the host's release and goes once, and the buffer going later
/// adds none and has the host destroy its object.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_window_destroyed_with_a_frame_handed_idles_its_buffer_once() {
    let Some(mut f) = fixture() else { return };
    let (id, window) = f.window(wid(1), 0);
    let red = f.buffer(RED);
    let present = f.present(&red, window);
    f.pump();
    let generation = f.generation(id);
    f.request(4, &wid(1).to_le_bytes());
    f.settle_down();
    assert!(!f.owed_anything());
    assert_eq!(f.idles_of(present), 0, "the host holds it");
    f.release(generation);
    f.pump_until("the idle", |f| f.idles_of(present) == 1);
    f.request(54, &red.client_pixmap.to_le_bytes());
    f.settle_down();
    assert_eq!(f.idles_of(present), 1);
    assert_eq!(f.host.borrow().forgotten, [generation]);
}

/// (e) A window resized with a frame owed ends as one whose frame was
/// copied: the resize settles the frame before it carries or drops the
/// content, and nothing puts the frame back afterwards.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_window_resized_with_a_frame_owed_ends_as_a_copied_one() {
    let Some(mut f) = fixture() else { return };
    let (_, direct) = f.window(wid(1), 0);
    let (_, copied) = f.window(wid(2), 100);
    let red = f.buffer(RED);
    f.present(&red, direct);
    f.pump();
    // The other window gets the same frame, from another buffer, by the
    // core's copy.
    let red2 = f.buffer(RED);
    let candidate = PresentScanoutCandidate {
        options: 0x2,
        ..f.candidate(red2.pixmap, copied)
    };
    let (_, taken) = f.present_as(candidate);
    assert!(!taken);
    Backend::copy_area(&mut f.b, None, red2.pixmap, copied, 0, 0, 0, 0, W, H).expect("copy");
    assert!(f.owed_anything());
    for window in [wid(1), wid(2)] {
        // ConfigureWindow: width 80.
        let body = [
            window.to_le_bytes().as_slice(),
            &0x0004u16.to_le_bytes(),
            &0u16.to_le_bytes(),
            &80u32.to_le_bytes(),
        ]
        .concat();
        f.request(12, &body);
    }
    assert!(!f.owed_anything());
    f.settle_down();
    for (x, y) in [(2, 2), (40, 30), (70, 10)] {
        assert_eq!(f.pixel(direct, x, y), f.pixel(copied, x, y), "({x}, {y})");
    }
}

/// (e) A window unmapped with a frame owed that was never handed: settled,
/// and its buffer idle.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_window_unmapped_with_a_frame_owed_settles_it() {
    let Some(mut f) = fixture() else { return };
    let (id, window) = f.window(wid(1), 0);
    f.host.borrow_mut().silent.insert(id);
    let red = f.buffer(RED);
    let blue = f.buffer(BLUE);
    let first = f.present(&red, window);
    f.pump();
    let second = f.present(&blue, window);
    f.pump();
    assert_eq!(f.handed(id), 1);
    f.request(10, &wid(1).to_le_bytes());
    f.pump_until("the idle", |f| f.idles_of(second) == 1);
    assert!(!f.owed_anything());
    assert_eq!(f.idles_of(first), 0, "the host holds the first");
}

/// (e) A server reset with one window's frame handed and another's owed:
/// nothing survives it -- no frame owed, no buffer kept, every idle out
/// once, the host's objects destroyed.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_reset_with_frames_owed_and_handed_leaves_nothing() {
    let Some(mut f) = fixture() else { return };
    let (shown, shown_xid) = f.window(wid(1), 0);
    let (hidden, hidden_xid) = f.window(wid(2), 100);
    f.host.borrow_mut().silent.insert(hidden);
    let red = f.buffer(RED);
    let blue = f.buffer(BLUE);
    let green = f.buffer([0, 0xff, 0, 0xff]);
    let handed = f.present(&red, shown_xid);
    let first = f.present(&blue, hidden_xid);
    f.pump();
    let owed = f.present(&green, hidden_xid);
    assert_eq!((f.handed(shown), f.handed(hidden)), (1, 1));
    assert!(f.owed_anything());

    yserver_core::core_loop::reset::force_destroy_all_clients(&mut f.state, &mut f.b);
    yserver_core::rootless::reconcile_and_dispatch(&mut f.state, &mut f.b);
    f.pump_until("every idle", |f| {
        [handed, first, owed].iter().all(|&id| f.idles_of(id) == 1)
    });
    f.settle_down();
    assert!(!f.owed_anything());
    assert!(f.b.pstate.direct_frames.layout(red.pixmap).is_none());
    // The two the host was given; the third it never had.
    let mut imported = f.host.borrow().imported.clone();
    let mut forgotten = f.host.borrow().forgotten.clone();
    imported.sort_unstable();
    forgotten.sort_unstable();
    assert_eq!((imported.len(), forgotten), (2, imported));
    for id in [handed, first, owed] {
        assert_eq!(f.idles_of(id), 1);
    }
}

/// (f) What an X client controls cannot put the host's window system in a
/// position to refuse: a present that does not stand for the whole window
/// is copied, and a buffer that is not a large enough dma-buf is never
/// kept for direct frames.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_present_or_buffer_that_does_not_qualify_is_copied() {
    let Some(mut f) = fixture() else { return };
    let (id, window) = f.window(wid(1), 0);
    let red = f.buffer(RED);
    let whole = f.candidate(red.pixmap, window);
    let cases = [
        PresentScanoutCandidate {
            update_is_full: false,
            ..whole
        },
        PresentScanoutCandidate { x_off: 1, ..whole },
        PresentScanoutCandidate {
            valid_region_xid: 0x0042_0001,
            ..whole
        },
        PresentScanoutCandidate {
            explicit_sync: true,
            ..whole
        },
        PresentScanoutCandidate {
            // PresentOptionCopy.
            options: 0x2,
            ..whole
        },
        PresentScanoutCandidate {
            src_width: W - 1,
            ..whole
        },
    ];
    for candidate in cases {
        let (_, taken) = f.present_as(candidate);
        assert!(!taken, "{candidate:?}");
    }
    // A buffer whose layout runs past the end of its dma-buf: refused at
    // the import's record, so its presents are copied.
    let (dmabuf, _memfd) =
        super::super::kms::direct_frames::tests::udmabuf_with_memfd(4096).expect("udmabuf");
    let short = Backend::dri3_import_pixmap(
        &mut f.b,
        dmabuf,
        W,
        H,
        STRIDE,
        0,
        Dri3ImportModifier::Explicit(LINEAR),
        24,
        32,
    );
    if let Ok(short) = short {
        assert!(f.b.pstate.direct_frames.layout(short.as_raw()).is_none());
        let (_, taken) = f.present_as(f.candidate(short.as_raw(), window));
        assert!(!taken);
    }
    // A buffer whose layout was not named LINEAR is not kept either.
    let (dmabuf, _memfd) = super::super::kms::direct_frames::tests::udmabuf_with_memfd(
        u64::from(STRIDE) * u64::from(H),
    )
    .expect("udmabuf");
    let implicit = Backend::dri3_import_pixmap(
        &mut f.b,
        dmabuf,
        W,
        H,
        STRIDE,
        0,
        Dri3ImportModifier::Implicit {
            size: STRIDE * u32::from(H),
        },
        24,
        32,
    )
    .expect("an implicit import");
    assert!(f.b.pstate.direct_frames.layout(implicit.as_raw()).is_none());
    f.settle_down();
    assert_eq!(f.handed(id), 0);
}

/// A window with a child mapped inside it is not covered by the window
/// presented to, and its frames are copied.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_window_with_another_inside_it_takes_no_direct_frames() {
    let Some(mut f) = fixture() else { return };
    let (_, window) = f.window(wid(1), 0);
    let child = wid(3);
    let body = [
        child.to_le_bytes().as_slice(),
        &wid(1).to_le_bytes(),
        &4i16.to_le_bytes(),
        &4i16.to_le_bytes(),
        &8u16.to_le_bytes(),
        &8u16.to_le_bytes(),
        &0u16.to_le_bytes(),
        &1u16.to_le_bytes(),
        &0u32.to_le_bytes(),
        &0u32.to_le_bytes(),
    ]
    .concat();
    f.request(1, &body);
    f.request(8, &child.to_le_bytes());
    let red = f.buffer(RED);
    let (_, taken) = f.present_as(f.candidate(red.pixmap, window));
    assert!(!taken);
}

/// A drawn-into presented buffer: the backing owed it gets the frame as it
/// was presented, before the draw changes the buffer.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_draw_into_the_presented_buffer_settles_the_window_first() {
    let Some(mut f) = fixture() else { return };
    let (id, window) = f.window(wid(1), 0);
    f.host.borrow_mut().silent.insert(id);
    let red = f.buffer(RED);
    f.present(&red, window);
    f.pump();
    let blue = f.buffer(BLUE);
    f.present(&blue, window);
    f.fill_green(blue.client_pixmap, (0, 0, W, H));
    assert!(!f.owed_anything());
    assert_eq!(f.pixel(window, 1, 1), BLUE[..3], "the frame as presented");
}

/// A present that qualifies, then one that does not (a part of the
/// window): the second is copied over the first, which is settled before
/// it.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_copied_present_after_a_direct_one_draws_over_it() {
    let Some(mut f) = fixture() else { return };
    let (id, window) = f.window(wid(1), 0);
    let red = f.buffer(RED);
    let blue = f.buffer(BLUE);
    f.present(&red, window);
    f.pump();
    let partial = PresentScanoutCandidate {
        update_is_full: false,
        ..f.candidate(blue.pixmap, window)
    };
    let (_, taken) = f.present_as(partial);
    assert!(!taken);
    // The core's copy of the update region, as `execute_present_pixmap_copy`
    // records it.
    Backend::copy_area(&mut f.b, None, blue.pixmap, window, 0, 0, 0, 0, 16, 16).expect("copy");
    assert!(!f.owed_anything());
    assert_eq!(f.pixel(window, 4, 4), BLUE[..3], "the copy");
    assert_eq!(f.pixel(window, 40, 40), RED[..3], "over the direct frame");
    f.pump_until("the window read back", |f| {
        f.shown(id, 4, 4) == Some([0xff, 0, 0]) && f.shown(id, 40, 40) == Some([0, 0, 0xff])
    });
}

/// A window made again (a popup moved to another parent, say) while the
/// host shows a direct frame of it: it shows nothing now, so the frame is
/// settled and the window read back whole with it.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_window_made_again_while_its_frame_is_out_is_read_back_with_it() {
    let Some(mut f) = fixture() else { return };
    let (id, window) = f.window(wid(1), 0);
    let red = f.buffer(RED);
    f.present(&red, window);
    f.pump();
    assert_eq!(f.handed(id), 1);
    let frames = f.host.borrow().cpu.len();
    {
        let mut host = f.host.borrow_mut();
        host.queued.frame(FrameEvent::Recreated { id });
        host.queued.frame(FrameEvent::SetupDone { id });
    }
    f.pump_until("the window read back", |f| {
        f.host.borrow().cpu.len() > frames
    });
    assert_eq!(f.shown(id, 30, 30), Some([0, 0, 0xff]));
    assert!(!f.owed_anything());
    assert_eq!(f.pixel(window, 30, 30), RED[..3]);
}

/// One buffer presented to two windows (a popup of the same client, say):
/// the host holds it for the first, so the second is not handed it but
/// read back with it.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn one_buffer_presented_to_two_windows_goes_direct_to_one() {
    let Some(mut f) = fixture() else { return };
    let (first, first_xid) = f.window(wid(1), 0);
    let (second, second_xid) = f.window(wid(2), 100);
    let red = f.buffer(RED);
    f.present(&red, first_xid);
    f.pump();
    f.present(&red, second_xid);
    f.pump_until("the second read back", |f| {
        f.shown(second, 10, 10) == Some([0, 0, 0xff])
    });
    assert_eq!((f.handed(first), f.handed(second)), (1, 0));
    assert_eq!(f.host.borrow().offered, 1);
}

/// (b) A present through the requests a client sends: DRI3's
/// `PixmapFromBuffers` of a LINEAR buffer and `PresentPixmap` of it to the
/// window, the whole of it, is handed to the host; the buffer's descriptor
/// is the host's from then on, the server keeps none.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_present_through_the_requests_goes_direct() {
    let Some(mut f) = fixture() else { return };
    let (id, window) = f.window(wid(1), 0);
    let pixmap: u32 = 0x0016_1001;
    let _pixels = f.pixmap_from_buffers(pixmap, wid(1), RED);
    let host = f
        .state
        .resources
        .pixmap(ResourceId(pixmap))
        .and_then(|p| p.host_xid)
        .expect("imported")
        .as_raw();
    assert!(f.b.pstate.direct_frames.layout(host).is_some(), "kept");
    f.present_pixmap_request(wid(1), pixmap, 1, 0);
    f.pump_until("the frame handed", |f| f.handed(id) == 1);
    assert_eq!(f.host.borrow().imported.len(), 1);
    assert_eq!(f.b.pstate.direct_frames.kept(), 0, "no descriptor kept");
    assert_eq!(f.pixel(window, 5, 5), RED[..3]);
}

/// (b, f) A `PresentPixmap` of the same buffer that does not stand for the
/// whole window (offset by a pixel) is copied, through the requests too.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_present_through_the_requests_that_does_not_qualify_is_copied() {
    let Some(mut f) = fixture() else { return };
    let (id, window) = f.window(wid(1), 0);
    let pixmap: u32 = 0x0016_1001;
    let _pixels = f.pixmap_from_buffers(pixmap, wid(1), RED);
    f.present_pixmap_request(wid(1), pixmap, 1, 1);
    f.settle_down();
    assert!(!f.owed_anything());
    assert_eq!(f.host.borrow().offered, 0);
    assert_eq!(f.handed(id), 0);
    assert_eq!(
        f.pixel(window, 0, 5),
        [0, 0, 0],
        "the copy starts a pixel in"
    );
    assert_eq!(f.pixel(window, 1, 5), RED[..3]);
}

/// (c) A frame owed to a window the host never gets ready (hidden) is
/// settled once `OWED_FOR` passed, and its buffer goes idle: the client
/// gets it back without the host.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_frame_owed_to_a_window_never_ready_is_settled_after_the_deadline() {
    let Some(mut f) = fixture() else { return };
    let (id, window) = f.window(wid(1), 0);
    f.host.borrow_mut().silent.insert(id);
    let red = f.buffer(RED);
    let blue = f.buffer(BLUE);
    f.present(&red, window);
    f.pump();
    let second = f.present(&blue, window);
    f.pump_until("the completion", |f| f.completed.contains(&second));
    f.settle_down();
    assert!(f.owed_anything(), "kept for the window's Ready");
    assert_eq!(f.idles_of(second), 0);
    let deadline = f.b.rootless_next_wakeup().expect("a deadline");
    assert!(deadline > Instant::now() + Duration::from_millis(500));
    // A second later, without waiting for it.
    f.b.pstate
        .direct_frames
        .age_for_test(super::super::kms::direct_frames::OWED_FOR);
    assert!(
        f.b.rootless_next_wakeup()
            .is_some_and(|at| at <= Instant::now())
    );
    f.pump_until("the idle", |f| f.idles_of(second) == 1);
    assert!(!f.owed_anything());
    assert_eq!(f.handed(id), 1, "settled, not handed");
    assert_eq!(f.pixel(window, 5, 5), BLUE[..3]);
}

/// Finding: a window a client redirected below a top-level has storage of
/// its own, which no host window shows: a present to it is copied, not
/// owed a frame nothing would ever hand over.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_present_to_a_window_a_client_redirected_is_copied() {
    let Some(mut f) = fixture() else { return };
    let (_, _) = f.window(wid(1), 0);
    let child = wid(3);
    let body = [
        child.to_le_bytes().as_slice(),
        &wid(1).to_le_bytes(),
        &0i16.to_le_bytes(),
        &0i16.to_le_bytes(),
        &W.to_le_bytes(),
        &H.to_le_bytes(),
        &0u16.to_le_bytes(),
        &1u16.to_le_bytes(),
        &0u32.to_le_bytes(),
        &0u32.to_le_bytes(),
    ]
    .concat();
    f.request(1, &body);
    f.request(8, &child.to_le_bytes());
    // Composite (144) RedirectWindow (1), Automatic.
    let body = [child.to_le_bytes().as_slice(), &[0, 0, 0, 0]].concat();
    f.request_with(144, 1, &body, None);
    let child_host = f
        .state
        .resources
        .window(ResourceId(child))
        .and_then(|w| w.host_xid)
        .expect("a backend window")
        .as_raw();
    let red = f.buffer(RED);
    let (_, taken) = f.present_as(f.candidate(red.pixmap, child_host));
    assert!(!taken);
    // The same window unredirected fills its top-level, and is taken.
    let body = [child.to_le_bytes().as_slice(), &[0, 0, 0, 0]].concat();
    // Composite UnredirectWindow (3).
    f.request_with(144, 3, &body, None);
    let (_, taken) = f.present_as(f.candidate(red.pixmap, child_host));
    assert!(taken, "a child that alone covers its top-level");
}

/// Finding: an `IncludeInferiors` stroke draws into each inferior with
/// storage of its own through a walk that only looks; a backing owed a
/// frame is settled first, so the stroke lands on the frame and the frame
/// is not copied over it after. The rootless server's own redirect of a
/// top-level takes no part in the scene, so the walk from the root passes
/// over it; the test has the top-level take part, as a window a client
/// redirected Automatic does, to reach it.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_stroke_through_the_roots_inferiors_lands_on_the_frame() {
    let Some(mut f) = fixture() else { return };
    let (_, window) = f.window(wid(1), 0);
    let leaf = f.b.store.lookup(window).expect("a leaf");
    f.b.store.set_scene_participating(leaf, true);
    let red = f.buffer(RED);
    f.present(&red, window);
    f.pump();
    // A GC with foreground green and IncludeInferiors.
    let body = [
        GC.to_le_bytes().as_slice(),
        &ROOT_WINDOW.0.to_le_bytes(),
        // GCForeground | GCSubwindowMode.
        &0x8004u32.to_le_bytes(),
        &0x0000_ff00u32.to_le_bytes(),
        &1u32.to_le_bytes(),
    ]
    .concat();
    f.request(55, &body);
    // PolyRectangle (67) on the root: x 10, y 10, 20 × 20.
    let body = [
        ROOT_WINDOW.0.to_le_bytes().as_slice(),
        &GC.to_le_bytes(),
        &10i16.to_le_bytes(),
        &10i16.to_le_bytes(),
        &20u16.to_le_bytes(),
        &20u16.to_le_bytes(),
    ]
    .concat();
    f.request(67, &body);
    assert!(!f.owed_anything(), "the stroke settled the frame first");
    f.settle_down();
    assert_eq!(f.pixel(window, 10, 15), GREEN, "the stroke");
    assert_eq!(f.pixel(window, 15, 15), RED[..3], "on the frame");
}

/// Finding: a window's named pixmap is its backing, read by its XID as a
/// tile or a background: a frame owed to it is settled first, so the tile
/// is the frame, not the window before it.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_named_window_pixmap_read_as_a_tile_or_background_is_the_frame() {
    let Some(mut f) = fixture() else { return };
    let (_, window) = f.window(wid(1), 0);
    let (_, other) = f.window(wid(2), 100);
    // Composite (144) NameWindowPixmap (6).
    let named: u32 = 0x0016_2001;
    let body = [wid(1).to_le_bytes().as_slice(), &named.to_le_bytes()].concat();
    f.request_with(144, 6, &body, None);
    let red = f.buffer(RED);
    f.present(&red, window);
    f.pump();
    assert!(f.owed_anything());
    // A tiled fill of the other window with it.
    let body = [
        GC.to_le_bytes().as_slice(),
        &wid(2).to_le_bytes(),
        // GCFillStyle (Tiled) | GCTile.
        &0x0500u32.to_le_bytes(),
        &1u32.to_le_bytes(),
        &named.to_le_bytes(),
    ]
    .concat();
    f.request(55, &body);
    let body = [
        wid(2).to_le_bytes().as_slice(),
        &GC.to_le_bytes(),
        &0i16.to_le_bytes(),
        &0i16.to_le_bytes(),
        &8u16.to_le_bytes(),
        &8u16.to_le_bytes(),
    ]
    .concat();
    f.request(70, &body);
    assert!(!f.owed_anything(), "the tile read settled the frame first");
    assert_eq!(f.pixel(other, 4, 4), RED[..3], "the tile is the frame");

    // As a background, after the next frame.
    let blue = f.buffer(BLUE);
    f.present(&blue, window);
    assert!(f.owed_anything());
    // ChangeWindowAttributes (2): CWBackPixmap; ClearArea (61) of all.
    let body = [
        wid(2).to_le_bytes().as_slice(),
        &0x0001u32.to_le_bytes(),
        &named.to_le_bytes(),
    ]
    .concat();
    f.request(2, &body);
    let body = [
        wid(2).to_le_bytes().as_slice(),
        &0i16.to_le_bytes(),
        &0i16.to_le_bytes(),
        &0u16.to_le_bytes(),
        &0u16.to_le_bytes(),
    ]
    .concat();
    f.request(61, &body);
    assert!(
        !f.owed_anything(),
        "the background read settled the frame first"
    );
    assert_eq!(
        f.pixel(other, 20, 20),
        BLUE[..3],
        "the background is the frame"
    );
}

/// Finding: reading the presented buffer (`GetImage` of it, `CopyArea` from
/// it) settles nothing; drawing into it does.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn only_a_draw_into_the_presented_buffer_settles_its_frame() {
    let Some(mut f) = fixture() else { return };
    let (_, window) = f.window(wid(1), 0);
    let (_, other) = f.window(wid(2), 100);
    let red = f.buffer(RED);
    f.present(&red, window);
    f.pump();
    assert_eq!(f.pixel(red.pixmap, 3, 3), RED[..3]);
    Backend::copy_area(&mut f.b, None, red.pixmap, other, 0, 0, 0, 0, 8, 8).expect("copy");
    assert!(
        f.owed_anything(),
        "reads of the buffer leave the frame owed"
    );
    f.fill_green(red.client_pixmap, (0, 0, 4, 4));
    assert!(!f.owed_anything(), "a draw into it settles the frame first");
    assert_eq!(f.pixel(window, 2, 2), RED[..3], "the frame as presented");
}

/// Finding: where the host takes no direct frames (a compositor that names
/// no usable format), no buffer is kept and no frame owed: every present
/// is copied.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn a_host_that_takes_no_direct_frames_keeps_nothing() {
    let Some(mut f) = fixture() else { return };
    f.host.borrow_mut().refuse = true;
    let (id, window) = f.window(wid(1), 0);
    let red = f.buffer(RED);
    assert!(f.b.pstate.direct_frames.layout(red.pixmap).is_none());
    assert_eq!(f.b.pstate.direct_frames.kept(), 0);
    let (_, taken) = f.present_as(f.candidate(red.pixmap, window));
    assert!(!taken);
    assert!(!f.owed_anything());
    assert_eq!(f.handed(id), 0);
}
