// macOS screen-capture backend.
//
// Primary path is ScreenCaptureKit (`crate::screencapturekit`), which is the
// only capture API that still works on macOS 14+ (CGDisplayStream is deprecated
// and fails outright on recent macOS / virtual displays). If ScreenCaptureKit
// is unavailable or fails to start, we fall back to the legacy CGDisplayStream
// backend (`crate::quartz`) so older systems keep working.
//
// Both paths surface a single public `Capturer`/`Display`/`PixelBuffer` type so
// the rest of the app (video_service, etc.) is agnostic to which one is active.

use crate::{quartz, screencapturekit as sck, Frame, Pixfmt};
use hbb_common::log;
use std::sync::{Arc, Mutex, TryLockError};
use std::{io, time::Duration};

pub struct Display(quartz::Display);

impl Display {
    pub fn primary() -> io::Result<Display> {
        Ok(Display(quartz::Display::primary()))
    }

    pub fn all() -> io::Result<Vec<Display>> {
        Ok(quartz::Display::online()
            .map_err(|_| io::Error::from(io::ErrorKind::Other))?
            .into_iter()
            .map(Display)
            .collect())
    }

    pub fn width(&self) -> usize {
        self.0.width()
    }

    pub fn height(&self) -> usize {
        self.0.height()
    }

    pub fn scale(&self) -> f64 {
        self.0.scale()
    }

    pub fn name(&self) -> String {
        self.0.id().to_string()
    }

    pub fn is_online(&self) -> bool {
        self.0.is_online()
    }

    pub fn origin(&self) -> (i32, i32) {
        let o = self.0.bounds().origin;
        (o.x as _, o.y as _)
    }

    pub fn is_primary(&self) -> bool {
        self.0.is_primary()
    }

    fn id(&self) -> u32 {
        self.0.id()
    }
}

// Legacy CGDisplayStream capturer + its callback-delivered frame slot.
struct QuartzBackend {
    inner: quartz::Capturer,
    frame: Arc<Mutex<Option<quartz::Frame>>>,
}

enum Backend {
    Sck(sck::Capturer),
    Quartz(QuartzBackend),
}

pub struct Capturer {
    backend: Backend,
    width: usize,
    height: usize,
    // BGRA bytes of the most recent frame; `PixelBuffer` borrows this.
    data: Vec<u8>,
    stride: usize,
    // Previous raw bytes, kept for a cheap "did anything change" comparison.
    saved_raw_data: Vec<u8>,
}

impl Capturer {
    pub fn new(display: Display) -> io::Result<Capturer> {
        let width = display.width();
        let height = display.height();

        // Prefer ScreenCaptureKit (works on macOS 12.3+, including macOS 26).
        match sck::Capturer::new(display.id(), width, height, false, 60) {
            Ok(c) => {
                log::info!(
                    "scrap: capturing via ScreenCaptureKit ({}x{})",
                    width,
                    height
                );
                return Ok(Capturer {
                    backend: Backend::Sck(c),
                    width,
                    height,
                    data: Vec::new(),
                    stride: 0,
                    saved_raw_data: Vec::new(),
                });
            }
            Err(e) => {
                log::warn!(
                    "scrap: ScreenCaptureKit unavailable ({e}); falling back to CGDisplayStream"
                );
            }
        }

        // Fallback: legacy CGDisplayStream.
        let frame = Arc::new(Mutex::new(None));
        let f = frame.clone();
        let inner = quartz::Capturer::new(
            display.0,
            width,
            height,
            quartz::PixelFormat::Argb8888,
            Default::default(),
            move |inner| {
                if let Ok(mut f) = f.lock() {
                    *f = Some(inner);
                }
            },
        )
        .map_err(|_| io::Error::from(io::ErrorKind::Other))?;

        Ok(Capturer {
            backend: Backend::Quartz(QuartzBackend { inner, frame }),
            width,
            height,
            data: Vec::new(),
            stride: 0,
            saved_raw_data: Vec::new(),
        })
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }
}

// A frame pulled from whichever backend is active, before it is normalised into
// `self.data`. Kept in its own scope so the backend borrow ends before we touch
// the other `Capturer` fields.
enum RawFrame {
    Sck(sck::Frame),
    Quartz(quartz::Frame),
}

impl crate::TraitCapturer for Capturer {
    fn frame<'a>(&'a mut self, _timeout: Duration) -> io::Result<Frame<'a>> {
        let raw = match &mut self.backend {
            Backend::Sck(c) => match c.take_frame() {
                Some(f) => RawFrame::Sck(f),
                None => return Err(io::ErrorKind::WouldBlock.into()),
            },
            Backend::Quartz(q) => match q.frame.try_lock() {
                Ok(mut handle) => match handle.take() {
                    Some(f) => RawFrame::Quartz(f),
                    None => return Err(io::ErrorKind::WouldBlock.into()),
                },
                Err(TryLockError::WouldBlock) => return Err(io::ErrorKind::WouldBlock.into()),
                Err(TryLockError::Poisoned(..)) => return Err(io::ErrorKind::Other.into()),
            },
        };

        match raw {
            RawFrame::Sck(fr) => {
                crate::would_block_if_equal(&mut self.saved_raw_data, &fr.data)?;
                // SCK is configured to output exactly the display's pixel size,
                // so keep width/height fixed and only adopt the (possibly padded)
                // row stride from the captured buffer.
                self.stride = fr.stride;
                self.data = fr.data;
            }
            RawFrame::Quartz(mut fr) => {
                crate::would_block_if_equal(&mut self.saved_raw_data, fr.inner())?;
                fr.surface_to_bgra(self.height);
                self.stride = fr.stride();
                let bgra: &[u8] = &fr;
                self.data.resize(bgra.len(), 0);
                self.data.copy_from_slice(bgra);
            }
        }

        Ok(Frame::PixelBuffer(PixelBuffer {
            data: &self.data,
            width: self.width,
            height: self.height,
            stride: self.stride,
        }))
    }

    #[cfg(feature = "vram")]
    fn device(&self) -> crate::AdapterDevice {
        Default::default()
    }

    #[cfg(feature = "vram")]
    fn set_output_texture(&mut self, _texture: bool) {}
}

pub struct PixelBuffer<'a> {
    data: &'a [u8],
    width: usize,
    height: usize,
    stride: usize,
}

impl<'a> crate::TraitPixelBuffer for PixelBuffer<'a> {
    fn data(&self) -> &[u8] {
        self.data
    }

    fn width(&self) -> usize {
        self.width
    }

    fn height(&self) -> usize {
        self.height
    }

    fn stride(&self) -> Vec<usize> {
        vec![self.stride]
    }

    fn pixfmt(&self) -> Pixfmt {
        Pixfmt::BGRA
    }
}
