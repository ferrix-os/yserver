//! [`Transport`] on Ferrix's client runtime, `compositor-toolkit`: std and
//! `libc` only, no libwayland. Its socket is polled by the core loop, and
//! each readiness is one non-blocking [`Client::dispatch`].

use std::{
    io,
    os::fd::{AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd, RawFd},
    time::Duration,
};

use compositor_toolkit::{
    Client, ObjectId, SurfaceId, ToplevelOptions, Value,
    protocol::ext_data_control::{
        self as control, ext_data_control_device_v1 as device,
        ext_data_control_manager_v1 as manager, ext_data_control_offer_v1 as offer,
        ext_data_control_source_v1 as source,
    },
};
use yserver_core::rootless::{Damage, HostWindows, SizeLimits, ToplevelSpec};

use super::transport::{Event, Offer, Screen, Selection, Source, Surface, Transport};

/// The toolkit's connection.
pub struct ToolkitTransport {
    client: Client,
    /// `ext_data_control_v1`, once bound.
    data: Option<DataControl>,
}

/// The data-control objects, and the offers the compositor made with the
/// types each said it has.
struct DataControl {
    manager: ObjectId,
    device: ObjectId,
    offers: Vec<(ObjectId, Vec<String>)>,
}

impl ToolkitTransport {
    /// Connect to the compositor `WAYLAND_DISPLAY` names, and wait until it
    /// has described every screen.
    ///
    /// # Errors
    ///
    /// No `WAYLAND_DISPLAY`, a socket that refuses, or a compositor that
    /// closes the connection during the first round trips.
    pub fn connect() -> io::Result<Self> {
        let client = Client::connect().map_err(|error| {
            io::Error::new(io::ErrorKind::NotConnected, format!("wayland: {error}"))
        })?;
        Ok(Self { client, data: None })
    }

    fn translate(&mut self, events: Vec<compositor_toolkit::Event>) -> Vec<Event> {
        events
            .into_iter()
            .filter_map(|event| self.event(event))
            .collect()
    }

    fn event(&mut self, event: compositor_toolkit::Event) -> Option<Event> {
        use compositor_toolkit::Event as Tk;
        let surface = |id: SurfaceId| Surface(id.0);
        Some(match event {
            Tk::Frame { surface: id, .. } => Event::Frame(surface(id)),
            Tk::Configure {
                surface: id,
                width,
                height,
            } => Event::Configure {
                surface: surface(id),
                width,
                height,
            },
            Tk::CloseRequested(id) => Event::Close(surface(id)),
            Tk::Object {
                object,
                interface,
                opcode,
                args,
            } => return self.data_event(object, interface, opcode, args),
            other => {
                log::debug!("wayland: {other:?}");
                return None;
            }
        })
    }

    /// An event of a data-control object.
    fn data_event(
        &mut self,
        object: ObjectId,
        interface: &str,
        opcode: u16,
        args: Vec<Value>,
    ) -> Option<Event> {
        let data = self.data.as_mut()?;
        if object == data.device {
            let which = match opcode {
                device::event::DATA_OFFER => {
                    if let Some(made) = args.first().and_then(Value::as_object) {
                        data.offers.push((made, Vec::new()));
                    }
                    return None;
                }
                device::event::SELECTION => Selection::Clipboard,
                device::event::PRIMARY_SELECTION => Selection::Primary,
                device::event::FINISHED => {
                    log::warn!("wayland: the compositor ended the clipboard's device");
                    return None;
                }
                _ => return None,
            };
            let named = args
                .first()
                .and_then(Value::as_object)
                .filter(|named| !named.is_null());
            let types = named
                .and_then(|named| data.offers.iter().find(|(made, _)| *made == named))
                .map(|(_, types)| types.clone())
                .unwrap_or_default();
            return Some(Event::Selection {
                which,
                offer: named.map(|named| Offer(named.0)),
                types,
            });
        }
        if let Some((_, types)) = data.offers.iter_mut().find(|(made, _)| *made == object) {
            if opcode == offer::event::OFFER
                && let Some(mime) = args.first().and_then(Value::as_str)
            {
                types.push(mime.to_owned());
            }
            return None;
        }
        if interface != control::EXT_DATA_CONTROL_SOURCE_V1.name {
            return None;
        }
        match opcode {
            source::event::SEND => {
                let mime = args
                    .first()
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let Some(Value::Fd(fd)) = args.get(1) else {
                    return None;
                };
                // SAFETY: the toolkit hands each descriptor an event carried
                // to the program, which owns it from then on; nothing else
                // closes it.
                let fd = unsafe { OwnedFd::from_raw_fd(*fd) };
                Some(Event::Send {
                    source: Source(object.0),
                    mime,
                    fd,
                })
            }
            source::event::CANCELLED => Some(Event::Cancelled(Source(object.0))),
            _ => None,
        }
    }

    fn other(error: compositor_toolkit::Error) -> io::Error {
        io::Error::other(format!("{error}"))
    }
}

impl HostWindows for ToolkitTransport {
    type Surface = Surface;

    fn create_toplevel(&mut self, spec: &ToplevelSpec<Surface>) -> io::Result<Surface> {
        // All sent before the first commit, where the compositor decides
        // whether a window floats.
        let options = ToplevelOptions {
            title: spec.title.clone(),
            app_id: spec.class.clone(),
            size: spec.size,
            parent: spec.parent.map(|parent| SurfaceId(parent.0)),
            min_size: spec.limits.min,
            max_size: spec.limits.max,
        };
        self.client
            .toplevel(&options)
            .map(|id| Surface(id.0))
            .map_err(Self::other)
    }

    fn destroy(&mut self, surface: Surface) {
        self.client.destroy(SurfaceId(surface.0));
    }

    fn set_title(&mut self, surface: Surface, title: &str) {
        self.client.set_title(SurfaceId(surface.0), title);
    }

    fn set_class(&mut self, surface: Surface, class: &str) {
        self.client.set_app_id(SurfaceId(surface.0), class);
    }

    fn set_parent(&mut self, surface: Surface, parent: Option<Surface>) {
        self.client.set_parent(
            SurfaceId(surface.0),
            parent.map(|parent| SurfaceId(parent.0)),
        );
    }

    fn set_size_limits(&mut self, surface: Surface, limits: SizeLimits) {
        self.client
            .set_size_limits(SurfaceId(surface.0), limits.min, limits.max);
    }

    fn ready(&self, surface: Surface) -> bool {
        self.client.size(SurfaceId(surface.0)).is_some()
    }

    fn draw(
        &mut self,
        surface: Surface,
        size: (u32, u32),
        opaque: bool,
        fill: &mut dyn FnMut(&mut [u8]) -> Option<Vec<Damage>>,
    ) -> io::Result<bool> {
        // X's 32-bit `ZPixmap` is `wl_shm`'s layout: `XRGB8888` below depth
        // 32 and premultiplied `ARGB8888` at it.
        self.client
            .draw_pixels(SurfaceId(surface.0), size, opaque, fill)
            .map_err(Self::other)
    }

    fn request_frame(&mut self, surface: Surface) {
        self.client.request_frame(SurfaceId(surface.0));
    }
}

impl Transport for ToolkitTransport {
    fn fd(&self) -> RawFd {
        self.client.as_raw_fd()
    }

    fn dispatch(&mut self) -> io::Result<Vec<Event>> {
        let events = self
            .client
            .dispatch(Some(Duration::ZERO))
            .map_err(|error| {
                io::Error::new(io::ErrorKind::ConnectionAborted, format!("{error}"))
            })?;
        Ok(self.translate(events))
    }

    fn roundtrip(&mut self) -> io::Result<Vec<Event>> {
        let events = self.client.roundtrip().map_err(Self::other)?;
        Ok(self.translate(events))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.client.flush().map_err(Self::other)
    }

    fn screen(&self) -> Option<Screen> {
        let output = self
            .client
            .outputs()
            .into_iter()
            .find(|output| output.done)?;
        let (width, height) = output.logical_size();
        let width = u16::try_from(width).ok().filter(|&width| width > 0)?;
        let height = u16::try_from(height).ok().filter(|&height| height > 0)?;
        Some(Screen {
            size: (width, height),
            refresh_mhz: u32::try_from(output.refresh_mhz).unwrap_or(0),
        })
    }

    fn keyboard_layout(&self) -> Option<(String, String)> {
        let (layout, _) = self.client.keyboard_layout()?;
        Some((layout.name.to_owned(), layout.variant.to_owned()))
    }

    fn drag(&mut self, surface: Surface, edges: u32) {
        self.client.drag_window(SurfaceId(surface.0), edges);
    }

    fn bind_data_control(&mut self) -> bool {
        let client = &mut self.client;
        let bound = (|| {
            let seat = client.seat()?;
            let (manager, _) = client
                .bind(&control::EXT_DATA_CONTROL_MANAGER_V1, 1, None)
                .ok()?;
            let device = client.new_object(&control::EXT_DATA_CONTROL_DEVICE_V1, 1);
            client
                .request(
                    manager,
                    manager::request::GET_DATA_DEVICE,
                    &[Value::NewId(device), Value::Object(seat)],
                )
                .ok()?;
            // An offer's types follow the event that makes it in the same
            // read.
            client.adopt_new_ids(
                device,
                device::event::DATA_OFFER,
                &control::EXT_DATA_CONTROL_OFFER_V1,
                1,
            );
            Some(DataControl {
                manager,
                device,
                offers: Vec::new(),
            })
        })();
        self.data = bound;
        self.data.is_some()
    }

    fn create_source(&mut self, types: &[&str]) -> io::Result<Source> {
        let manager = self
            .data
            .as_ref()
            .map(|data| data.manager)
            .ok_or_else(|| io::Error::other("no ext_data_control_v1"))?;
        let made = self
            .client
            .new_object(&control::EXT_DATA_CONTROL_SOURCE_V1, 1);
        self.client
            .request(
                manager,
                manager::request::CREATE_DATA_SOURCE,
                &[Value::NewId(made)],
            )
            .map_err(Self::other)?;
        for mime in types {
            self.client
                .request(
                    made,
                    source::request::OFFER,
                    &[Value::Str(Some((*mime).to_owned()))],
                )
                .map_err(Self::other)?;
        }
        Ok(Source(made.0))
    }

    fn destroy_source(&mut self, held: Source) {
        let _ = self
            .client
            .request(ObjectId(held.0), source::request::DESTROY, &[]);
    }

    fn set_selection(&mut self, which: Selection, held: Option<Source>) {
        let Some(device) = self.data.as_ref().map(|data| data.device) else {
            return;
        };
        let opcode = match which {
            Selection::Clipboard => device::request::SET_SELECTION,
            Selection::Primary => device::request::SET_PRIMARY_SELECTION,
        };
        let _ = self.client.request(
            device,
            opcode,
            &[Value::Object(
                held.map_or(ObjectId::NULL, |held| ObjectId(held.0)),
            )],
        );
    }

    fn receive(&mut self, held: Offer, mime: &str, fd: BorrowedFd<'_>) -> io::Result<()> {
        self.client
            .request(
                ObjectId(held.0),
                offer::request::RECEIVE,
                &[Value::Str(Some(mime.to_owned())), Value::Fd(fd.as_raw_fd())],
            )
            .and_then(|()| self.client.flush())
            .map_err(Self::other)
    }

    fn release_offer(&mut self, held: Offer) {
        let _ = self
            .client
            .request(ObjectId(held.0), offer::request::DESTROY, &[]);
        if let Some(data) = self.data.as_mut() {
            data.offers.retain(|(made, _)| made.0 != held.0);
        }
    }
}
