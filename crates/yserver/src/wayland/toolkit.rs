//! [`Transport`] on Ferrix's client runtime, `compositor-toolkit`: std and
//! `libc` only, no libwayland. Its socket is polled by the core loop, and
//! each readiness is one non-blocking [`Client::dispatch`].

use std::{io, os::fd::RawFd, time::Duration};

use compositor_toolkit::{Client, SurfaceId, ToplevelOptions};
use yserver_core::rootless::{Damage, HostWindows, SizeLimits, ToplevelSpec};

use super::transport::{Event, Screen, Surface, Transport};

/// The toolkit's connection.
pub struct ToolkitTransport {
    client: Client,
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
        Ok(Self { client })
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
            other => {
                log::debug!("wayland: {other:?}");
                return None;
            }
        })
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
}
