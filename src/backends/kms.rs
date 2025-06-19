//! Backend for DRM/KMS for raw rendering directly to the screen.
//!
//! This strategy uses dumb buffers for rendering.

use drm::control::{
    connector, crtc,
    dumbbuffer::{DumbBuffer, DumbMapping},
    framebuffer, Device as CtrlDevice, Mode, PageFlipFlags, ResourceHandles,
};
use drm::{buffer::DrmFourcc, Device};

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle};
use tracing::{error, warn};

use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::os::unix::io::{AsFd, BorrowedFd};
use std::sync::Arc;

use crate::error::{InitError, SoftBufferError};
use crate::{backend_interface::*, error::SwResultExt};

/// The implementation of the `Context` type for the KMS backend.
///
/// This type wraps the file descriptor of the DRM card (i.e., the `/dev/dri/card0` file),
#[derive(Debug)]
pub(crate) struct KmsDisplayImpl<D: ?Sized> {
    /// The underlying raw device file descriptor.
    fd: BorrowedFd<'static>,

    /// Holds a reference to the display.
    _display: D,
}

impl<D: ?Sized> AsFd for KmsDisplayImpl<D> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd
    }
}
impl<D: ?Sized> Device for KmsDisplayImpl<D> {}
impl<D: ?Sized> CtrlDevice for KmsDisplayImpl<D> {}

impl<D: HasDisplayHandle + ?Sized> ContextInterface<D> for Arc<KmsDisplayImpl<D>> {
    fn new(display: D) -> Result<Self, InitError<D>>
    where
        D: Sized,
    {
        // Check if the display handle is a DRM one, panic otherwise
        let card_fd = match display.display_handle().unwrap().as_raw() {
            RawDisplayHandle::Drm(drm) => drm.fd,
            _ => panic!(),
        };

        // SAFETY: Invariants guaranteed by the user.
        let fd = unsafe { BorrowedFd::borrow_raw(card_fd) };

        Ok(Arc::new(KmsDisplayImpl {
            fd,
            _display: display,
        }))
    }
}

/// The implementation of the `Surface` type for the KMS backend.
#[derive(Debug)]
pub(crate) struct KmsImpl<D: ?Sized, W: ?Sized> {
    /// The display implementation.
    display: Arc<KmsDisplayImpl<D>>,

    /// The CRTC to render to.
    crtc: crtc::Info,

    /// The dumb buffer we're using as a buffer.
    buffers: Option<Buffers>,

    conn: connector::Info,

    mode: Mode,

    /// Window handle that we are keeping around.
    window: W,
}

impl<D: HasDisplayHandle + ?Sized, W: HasWindowHandle> SurfaceInterface<D, W> for KmsImpl<D, W> {
    type Context = Arc<KmsDisplayImpl<D>>;
    type Buffer<'a>
        = BufferImpl<'a, D, W>
    where
        Self: 'a;

    /// Create a new KMS backend.
    fn new(window: W, display: &Arc<KmsDisplayImpl<D>>) -> Result<Self, InitError<W>> {
        // For an overview of what this function does, and for a general introduction to the DRM/KMS api,
        // refer to https://manpages.debian.org/bookworm/libdrm-dev/drm-kms.7.en.html

        let res = display
            .resource_handles()
            .expect("Could not load normal resource ids.");

        let conn = find_connector(display, &res)?;
        let crtc = find_crtc(display, &res, &conn)?;
        // The first mode is always the one with the highest resolution (as stated by drm-kms(7))
        let mode = *conn.modes().first().expect("No modes found on connector");

        Ok(KmsImpl {
            buffers: None,
            crtc,
            conn,
            mode,

            window,
            display: Arc::clone(display),
        })
    }

    fn window(&self) -> &W {
        &self.window
    }

    fn resize(&mut self, width: NonZeroU32, height: NonZeroU32) -> Result<(), SoftBufferError> {
        assert_eq!(self.mode.size().0, u32::from(width) as u16);
        assert_eq!(self.mode.size().1, u32::from(height) as u16);

        let buf1 = SharedBuffer::new(&self.display, width, height)?;
        let buf2 = SharedBuffer::new(&self.display, width, height)?;
        let fb = buf1.fb;

        self.buffers = Some(Buffers {
            buffers: [buf1, buf2],
            first_is_front: true,
        });

        self.display
            .set_crtc(
                self.crtc.handle(),
                Some(fb),
                (0, 0),
                &[self.conn.handle()],
                Some(self.mode),
            )
            .unwrap();

        Ok(())
    }

    fn fetch(&mut self) -> Result<Vec<u32>, SoftBufferError> {
        unimplemented!()
    }

    fn buffer_mut(&mut self) -> Result<BufferImpl<'_, D, W>, SoftBufferError> {
        let buffers = self.buffers.as_mut().expect("Need to call resize first...");

        let front = if buffers.first_is_front {
            &mut buffers.buffers[0]
        } else {
            &mut buffers.buffers[1]
        };
        buffers.first_is_front = !buffers.first_is_front;

        let mapping = self
            .display
            .map_dumb_buffer(&mut front.db)
            .swbuf_err("Failed to map dumb buffer")?;

        Ok(BufferImpl {
            fb: front.fb,
            display: &self.display,
            _window: PhantomData,
            crtc: self.crtc.handle(),
            mapping,
        })
    }
}

/// Find a display connector on which to render.
///
/// Right now, this selects the first connector that has a display currently attached.
fn find_connector<D: HasDisplayHandle + ?Sized, W: HasWindowHandle>(
    display: &Arc<KmsDisplayImpl<D>>,
    res: &ResourceHandles,
) -> Result<connector::Info, InitError<W>> {
    let mut coninfo: Vec<connector::Info> = res
        .connectors()
        .iter()
        .flat_map(|con| display.get_connector(*con, true))
        .filter(|con| con.state() == connector::State::Connected)
        .collect();

    if coninfo.len() == 0 {
        error!("No DRM connector found. Did you plug a display in?");
        return Err(InitError::Failure(SoftBufferError::PlatformError(
            Some(String::from("No connected DRM connector found")),
            None,
        )));
    }

    if coninfo.len() > 1 {
        warn!("More than one connected DRM connector found. Using the first one");
    }

    Ok(coninfo.swap_remove(0))
}

/// Find a CRTC that can be used with the provided connector.
fn find_crtc<D: HasDisplayHandle + ?Sized, W: HasWindowHandle>(
    display: &Arc<KmsDisplayImpl<D>>,
    res: &ResourceHandles,
    conn: &connector::Info,
) -> Result<crtc::Info, InitError<W>> {
    for enc in conn.encoders() {
        let enc = display.get_encoder(*enc).unwrap();

        if let Some(crtc) = res.filter_crtcs(enc.possible_crtcs()).first() {
            return Ok(display.get_crtc(*crtc).swbuf_err("Failed to get CRTC")?);
        }
    }

    Err(SoftBufferError::PlatformError(Some("No compatible CRTC found".into()), None).into())
}

/// The buffer implementation.
pub(crate) struct BufferImpl<'a, D: ?Sized, W: ?Sized> {
    crtc: crtc::Handle,
    fb: framebuffer::Handle,
    mapping: DumbMapping<'a>,

    /// The display implementation.
    display: &'a KmsDisplayImpl<D>,

    /// Window reference.
    _window: PhantomData<&'a mut W>,
}

impl<D: ?Sized, W: ?Sized> BufferInterface for BufferImpl<'_, D, W> {
    #[inline]
    fn pixels(&self) -> &[u32] {
        bytemuck::cast_slice(self.mapping.as_ref())
    }

    #[inline]
    fn pixels_mut(&mut self) -> &mut [u32] {
        bytemuck::cast_slice_mut(self.mapping.as_mut())
    }

    #[inline]
    fn age(&self) -> u8 {
        2
    }

    #[inline]
    fn present_with_damage(self, _damage: &[crate::Rect]) -> Result<(), SoftBufferError> {
        self.display
            .page_flip(self.crtc, self.fb, PageFlipFlags::EVENT, None)
            .unwrap();

        Ok(())
    }

    #[inline]
    fn present(self) -> Result<(), SoftBufferError> {
        self.present_with_damage(&[])
    }
}

#[derive(Debug)]
struct Buffers {
    /// The involved set of buffers.
    buffers: [SharedBuffer; 2],

    /// Whether to use the first buffer or the second buffer as the front buffer.
    first_is_front: bool,
}

/// The combined frame buffer and dumb buffer.
#[derive(Debug)]
struct SharedBuffer {
    /// The frame buffer.
    fb: framebuffer::Handle,

    /// The dumb buffer.
    db: DumbBuffer,
}

impl SharedBuffer {
    /// Create a new buffer set.
    pub(crate) fn new<D: ?Sized>(
        display: &KmsDisplayImpl<D>,
        width: NonZeroU32,
        height: NonZeroU32,
    ) -> Result<Self, SoftBufferError> {
        let db = display
            .create_dumb_buffer((width.get(), height.get()), DrmFourcc::Xrgb8888, 32)
            .swbuf_err("failed to create dumb buffer")?;
        let fb = display
            .add_framebuffer(&db, 24, 32)
            .swbuf_err("failed to add framebuffer")?;

        Ok(SharedBuffer { fb, db })
    }
}
