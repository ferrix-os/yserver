//! DRI3 buffers the renderer cannot import as they lie, mapped instead.
//!
//! A renderer without `VK_EXT_image_drm_format_modifier` (lavapipe where
//! there is no `/dev/udmabuf`) imports a LINEAR buffer with
//! `VK_IMAGE_TILING_LINEAR` at a row pitch and with padding of its own
//! choosing, so a buffer whose pitch is another would be read sheared, and
//! one the renderer would read past the end of fails to import (lavapipe
//! pads a LINEAR image's height: 974 rows ask for 976). Such a buffer is
//! mapped read-only, and its pixmap is an ordinary one filled from the
//! mapping each time it is read ([`KmsBackend::refresh_mapped_pixmap`]),
//! after the client's GPU work on it: a Present waits for the buffer's read
//! fence from the loop first (`arm_present_source_wait`).

use std::{
    io,
    os::fd::{AsRawFd as _, OwnedFd},
};

use ash::vk;
use yserver_core::backend::{Backend as _, PixmapHandle};

use crate::kms::render::{backend::KmsBackend, target::Dst};

/// A client's LINEAR DRI3 buffer, mapped read-only.
pub(crate) struct Dri3Mapped {
    pub(in crate::kms::render::backend) fd: OwnedFd,
    map: *mut libc::c_void,
    len: usize,
    offset: usize,
    stride: usize,
    width: u16,
    height: u16,
    depth: u8,
}

impl Dri3Mapped {
    /// The image's rows as they lie in the buffer: `stride` bytes apart,
    /// from `offset`, `height` of them.
    fn rows(&self) -> &[u8] {
        // SAFETY: `map` is a live read-only mapping of `len` bytes, and
        // `offset + stride * height <= len` was checked when it was made.
        let bytes = unsafe { std::slice::from_raw_parts(self.map.cast::<u8>(), self.len) };
        &bytes[self.offset..self.offset + self.stride * usize::from(self.height)]
    }

    /// The image, its rows packed to `width` pixels: an X `ZPixmap` at 32
    /// bits per pixel.
    fn packed(&self) -> Vec<u8> {
        let row = usize::from(self.width) * 4;
        let mut out = Vec::with_capacity(row * usize::from(self.height));
        let rows = self.rows();
        for y in 0..usize::from(self.height) {
            let start = y * self.stride;
            out.extend_from_slice(&rows[start..start + row]);
        }
        out
    }
}

// SAFETY: the mapping is read-only memory the value owns; nothing about it
// is tied to a thread.
unsafe impl Send for Dri3Mapped {}

impl Drop for Dri3Mapped {
    fn drop(&mut self) {
        // SAFETY: the mapping this value made and nothing else holds.
        unsafe {
            libc::munmap(self.map, self.len);
        }
    }
}

impl KmsBackend {
    /// Whether the renderer's own LINEAR layout of a `width` x `height`
    /// image in `format` is `offset` and `pitch` -- what an import with
    /// `VK_IMAGE_TILING_LINEAR` reads the client's buffer as -- and the
    /// memory it asks for that image fits in the client's buffer of
    /// `buffer_len` bytes.
    ///
    /// The second half matters as much as the first. Lavapipe pads a
    /// LINEAR image's height (478 rows ask for 480, 974 for 976), so a
    /// 1920x974 image asks for 7,495,680 bytes, more than anv's 7680 x 974
    /// buffer rounded to a page, 7,483,392: the import failed with
    /// `VK_ERROR_OUT_OF_DEVICE_MEMORY`, the client's `PixmapFromBuffers` got
    /// BadAlloc, and Mesa's swapchain creation failed. Such a buffer is
    /// mapped and copied as it is read instead.
    pub(in crate::kms::render::backend) fn linear_layout_is(
        vk: &crate::kms::vk::device::VkContext,
        format: vk::Format,
        (width, height): (u16, u16),
        offset: u32,
        pitch: u32,
        buffer_len: Option<u64>,
    ) -> bool {
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::ImageCreateInfo::default()
            .push_next(&mut external)
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: u32::from(width),
                height: u32::from(height),
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::LINEAR)
            .usage(crate::kms::vk::dri3::CLIENT_IMPORT_USAGE)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: a valid create-info; the image is destroyed below and
        // never bound or used.
        let Ok(image) = (unsafe { vk.device.create_image(&info, None) }) else {
            return false;
        };
        // SAFETY: `image` is a live LINEAR image of this device.
        let layout = unsafe {
            vk.device.get_image_subresource_layout(
                image,
                vk::ImageSubresource {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    array_layer: 0,
                },
            )
        };
        // SAFETY: as above.
        let needs = unsafe { vk.device.get_image_memory_requirements(image) }.size;
        // SAFETY: as above; nothing else holds it.
        unsafe { vk.device.destroy_image(image, None) };
        let fits = Self::linear_import_fits(needs, offset, buffer_len);
        if !fits {
            log::info!(
                "DRI3: a {width}x{height} LINEAR image needs {needs} bytes of the renderer, \
                 more than the client's buffer of {} past offset {offset}",
                buffer_len.unwrap_or(0)
            );
        }
        layout.offset == u64::from(offset) && layout.row_pitch == u64::from(pitch) && fits
    }

    /// Whether memory of `needs` bytes, bound at `offset`, lies inside a
    /// client's buffer of `buffer_len` bytes ([`Self::linear_layout_is`]).
    /// A buffer whose length is unknown is taken to fit.
    pub(in crate::kms::render::backend) fn linear_import_fits(
        needs: u64,
        offset: u32,
        buffer_len: Option<u64>,
    ) -> bool {
        buffer_len.is_none_or(|len| u64::from(offset).saturating_add(needs) <= len)
    }

    /// The length of the file `fd` names: a dmabuf's is its buffer's size
    /// (Linux sets `i_size`).
    pub(in crate::kms::render::backend) fn fd_len(fd: &OwnedFd) -> Option<u64> {
        // SAFETY: fstat into a zeroed stat on a descriptor we hold.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        (unsafe { libc::fstat(fd.as_raw_fd(), &raw mut stat) } == 0)
            .then(|| u64::try_from(stat.st_size).ok())
            .flatten()
            .filter(|&len| len > 0)
    }

    /// A DRI3 buffer mapped, and an ordinary pixmap for it filled from the
    /// mapping ([`Dri3Mapped`]).
    pub(in crate::kms::render::backend) fn dri3_import_mapped(
        &mut self,
        fd: OwnedFd,
        (width, height): (u16, u16),
        stride: u32,
        offset: u32,
        depth: u8,
    ) -> io::Result<PixmapHandle> {
        let row = usize::from(width) * 4;
        let stride = stride as usize;
        let offset = offset as usize;
        if stride < row {
            return Err(io::Error::other(format!(
                "DRI3 import: stride {stride} is below a row of {width} pixels"
            )));
        }
        let len = stride
            .checked_mul(usize::from(height))
            .and_then(|rows| rows.checked_add(offset))
            .ok_or_else(|| io::Error::other("DRI3 import: size"))?;
        // A buffer smaller than its stated layout is refused rather than
        // mapped past its end.
        if Self::fd_len(&fd).is_some_and(|size| size < len as u64) {
            return Err(io::Error::other(format!(
                "DRI3 import: a buffer of {:?} bytes for a {len}-byte layout",
                Self::fd_len(&fd)
            )));
        }
        // SAFETY: a shared read-only mapping of the client's buffer, kept by
        // `Dri3Mapped` and unmapped by its drop.
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(io::Error::other(format!(
                "DRI3 import: mapping the buffer: {}",
                io::Error::last_os_error()
            )));
        }
        let mapped = Dri3Mapped {
            fd,
            map,
            len,
            offset,
            stride,
            width,
            height,
            depth,
        };
        let handle = self.create_pixmap(None, depth, width, height)?;
        log::info!(
            "DRI3: pixmap 0x{:x} ({width}x{height}, stride {stride}) is mapped and copied as \
             it is read: the renderer imports LINEAR only at its own pitch and size",
            handle.as_raw()
        );
        let _ = self.pstate.dri3_mapped.insert(handle.as_raw(), mapped);
        self.refresh_mapped_pixmap(handle.as_raw());
        Ok(handle)
    }

    /// Fill a mapped DRI3 buffer's pixmap from the mapping. No wait here: a
    /// Present waited for the client's GPU work on the buffer before it
    /// executes (`arm_present_source_wait`); a CopyArea reads it as an
    /// imported buffer's is read, under implicit sync.
    pub(in crate::kms::render::backend) fn refresh_mapped_pixmap(&mut self, host_xid: u32) {
        let Some(mapped) = self.pstate.dri3_mapped.get(&host_xid) else {
            return;
        };
        let Some(id) = self.store.lookup(host_xid) else {
            return;
        };
        // Straight from the mapping into the upload, at the client's stride:
        // the upload's source rows are `stride / 4` pixels apart, and the
        // copy is clamped to the pixmap, so the padding is never read. A
        // stride that is no whole number of pixels is repacked first.
        let packed;
        let (bytes, row_pixels) = match u16::try_from(mapped.stride / 4) {
            Ok(row_pixels) if mapped.stride % 4 == 0 => (mapped.rows(), row_pixels),
            _ => {
                packed = mapped.packed();
                (packed.as_slice(), mapped.width)
            }
        };
        let extent = vk::Extent2D {
            width: u32::from(row_pixels),
            height: u32::from(mapped.height),
        };
        let depth = mapped.depth;
        if let Err(err) = self.engine.put_image(
            &mut self.store,
            &mut self.platform.gpu,
            Dst::server_internal(id),
            vk::Offset2D::default(),
            extent,
            bytes,
            depth,
        ) {
            log::warn!("DRI3: filling mapped pixmap 0x{host_xid:x}: {err:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A LINEAR import binds the renderer's whole memory requirement in the
    /// client's buffer, so a requirement past the buffer's end must send it
    /// down the mapped path (a 1920x974 swapchain image's import failed with
    /// `VK_ERROR_OUT_OF_DEVICE_MEMORY`).
    #[test]
    fn a_linear_import_must_fit_in_the_clients_buffer() {
        // anv's 1920x974 buffer, a page-rounded 7680 x 974.
        let anv = Some(7_483_392);
        assert!(KmsBackend::linear_import_fits(7_480_320, 0, anv));
        assert!(!KmsBackend::linear_import_fits(7_680 * 976, 0, anv));
        assert!(!KmsBackend::linear_import_fits(7_480_320, 4_096, anv));
        // Unknown length: the import is tried.
        assert!(KmsBackend::linear_import_fits(u64::MAX, 0, None));
    }
}
