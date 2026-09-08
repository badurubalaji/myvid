//! The hand-off between the decode thread and the render thread.
//!
//! One slot, one mutex, last frame wins. The decoder replaces what is there as
//! fast as the pipeline clock allows; the renderer reads whatever it finds when
//! it wakes. No queue and no back-pressure: if the GPU misses a frame, the next
//! one is already more correct than the one it missed.
//!
//! The slot holds the decoder's *own* buffer rather than a copy of it. A frame
//! therefore costs one allocation-free handoff instead of a full-resolution
//! memcpy, and the only copy in the whole path is the upload to the GPU.

use std::fmt;
use std::sync::{Arc, Mutex};

/// A decoded frame owned by whoever produced it.
///
/// Implementations hand out borrowed plane data, so this trait is what keeps
/// the zero-copy path available to any backend — not just GStreamer.
pub trait PlanarFrame: Send + Sync {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    /// Pixels and row stride for one plane, or `None` if it does not exist.
    fn plane(&self, index: usize) -> Option<(&[u8], u32)>;
}

#[derive(Default)]
pub struct Frame {
    source: Option<Box<dyn PlanarFrame>>,
    /// Bumped on every write so the renderer can skip an unchanged frame.
    generation: u64,
}

impl Frame {
    pub fn is_empty(&self) -> bool {
        self.source.is_none()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn width(&self) -> u32 {
        self.source.as_ref().map_or(0, |s| s.width())
    }

    pub fn height(&self) -> u32 {
        self.source.as_ref().map_or(0, |s| s.height())
    }

    pub fn plane(&self, index: usize) -> Option<(&[u8], u32)> {
        self.source.as_ref()?.plane(index)
    }

    /// Take ownership of a newly decoded frame, dropping the previous one.
    pub fn set(&mut self, source: Box<dyn PlanarFrame>) {
        self.source = Some(source);
        self.generation += 1;
    }
}

/// A cloneable handle to the shared frame slot.
#[derive(Clone, Default)]
pub struct FrameSlot(Arc<Mutex<Frame>>);

impl FrameSlot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write<R>(&self, f: impl FnOnce(&mut Frame) -> R) -> R {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    }

    pub fn read<R>(&self, f: impl FnOnce(&Frame) -> R) -> R {
        let guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f(&guard)
    }

    /// Aspect ratio of the last frame, if there is one.
    pub fn aspect(&self) -> Option<f32> {
        self.read(|f| (!f.is_empty()).then(|| f.width() as f32 / f.height() as f32))
    }

    /// Drop the current picture, so the last frame of the previous file does
    /// not linger behind the next one. Also releases the decoder's buffer.
    pub fn clear(&self) {
        self.write(|f| {
            f.source = None;
            f.generation = 0;
        });
    }
}

impl fmt::Debug for FrameSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (w, h, g) = self.read(|fr| (fr.width(), fr.height(), fr.generation));
        write!(f, "FrameSlot({w}x{h} gen {g})")
    }
}

impl PartialEq for FrameSlot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
