//! The shared frame buffer between the decode process and the renderer.
//!
//! Process isolation costs a copy: the decoder's own buffer lives in its address
//! space, so a frame has to be written somewhere both sides can see. This is
//! that place — an anonymous `memfd`, mapped writable by the decoder and
//! read-only by the player, holding a small ring of frames.
//!
//! The ring is what makes the copy safe without locking. The decoder writes the
//! next slot while the player reads the last one, and only laps it if the player
//! stalls for several frames — at which point the picture is stale anyway.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;

use anyhow::{Context, Result};
use rustix::fs::MemfdFlags;
use rustix::mm::{MapFlags, ProtFlags};

use super::frame::PlanarFrame;
use super::protocol::Layout;

/// Enough slots that a late reader is never overwritten mid-read, and few
/// enough that the buffer stays a few megabytes.
pub const SLOTS: u32 = 3;

/// A mapping of the shared buffer. Owns the mapping for its lifetime.
pub struct Surface {
    base: *mut u8,
    len: usize,
    layout: Layout,
    /// Held so the mapping outlives any descriptor juggling.
    _memfd: OwnedFd,
}

// The mapping is a plain byte range; the ring discipline, not the type system,
// keeps writer and reader apart.
unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            let _ = rustix::mm::munmap(self.base as *mut _, self.len);
        }
    }
}

impl Surface {
    /// Create a buffer for this layout, writable by the caller.
    pub fn create(layout: Layout) -> Result<Self> {
        let memfd = rustix::fs::memfd_create("myvid-frames", MemfdFlags::CLOEXEC)
            .context("creating the shared frame buffer")?;
        rustix::fs::ftruncate(&memfd, layout.total_bytes() as u64)
            .context("sizing the shared frame buffer")?;

        Self::map(memfd, layout, true)
    }

    /// Adopt a buffer received from the decoder, read-only.
    pub fn adopt(memfd: OwnedFd, layout: Layout) -> Result<Self> {
        Self::map(memfd, layout, false)
    }

    fn map(memfd: OwnedFd, layout: Layout, writable: bool) -> Result<Self> {
        let len = layout.total_bytes();
        let prot = if writable {
            ProtFlags::READ | ProtFlags::WRITE
        } else {
            ProtFlags::READ
        };

        let base = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                len,
                prot,
                MapFlags::SHARED,
                &memfd,
                0,
            )
        }
        .context("mapping the shared frame buffer")?;

        Ok(Surface {
            base: base as *mut u8,
            len,
            layout,
            _memfd: memfd,
        })
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self._memfd.as_fd()
    }

    fn slot_offset(&self, slot: u32) -> usize {
        (slot % self.layout.slots) as usize * self.layout.slot_bytes as usize
    }

    /// Copy one frame's planes into a slot. Decoder side.
    ///
    /// # Panics
    /// Never; a plane larger than the slot is truncated rather than overrunning
    /// the mapping, which is the safe failure for a mismatched layout.
    pub fn write(&self, slot: u32, luma: &[u8], chroma: &[u8]) {
        let offset = self.slot_offset(slot);
        let luma_len = self.layout.luma_bytes().min(luma.len());
        let chroma_len = self.layout.chroma_bytes().min(chroma.len());

        unsafe {
            let base = self.base.add(offset);
            std::ptr::copy_nonoverlapping(luma.as_ptr(), base, luma_len);
            std::ptr::copy_nonoverlapping(
                chroma.as_ptr(),
                base.add(self.layout.luma_bytes()),
                chroma_len,
            );
        }
    }
}

/// One frame, read directly out of a slot in the shared buffer.
pub struct SharedFrame {
    surface: Arc<Surface>,
    slot: u32,
}

impl SharedFrame {
    pub fn new(surface: Arc<Surface>, slot: u32) -> Self {
        Self { surface, slot }
    }
}

impl PlanarFrame for SharedFrame {
    fn width(&self) -> u32 {
        self.surface.layout.width
    }

    fn height(&self) -> u32 {
        self.surface.layout.height
    }

    fn plane(&self, index: usize) -> Option<(&[u8], u32)> {
        let layout = self.surface.layout;
        let offset = self.surface.slot_offset(self.slot);

        let (start, len, stride) = match index {
            0 => (offset, layout.luma_bytes(), layout.y_stride),
            1 => (
                offset + layout.luma_bytes(),
                layout.chroma_bytes(),
                layout.uv_stride,
            ),
            _ => return None,
        };

        let plane = unsafe { std::slice::from_raw_parts(self.surface.base.add(start), len) };
        Some((plane, stride))
    }
}
