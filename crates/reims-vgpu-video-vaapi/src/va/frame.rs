//! A decoded frame, for every codec: its surface, held until the frame is
//! dropped, what the decoder knows of it, and its readback into caller
//! planes, NV12 for 8-bit frames and P010 for 10-bit ones.

use std::sync::Arc;

use super::display::Error;
use super::readback::ReadbackTimes;
use super::surface::{Rect, Surface, SurfaceFormat, SurfaceReadBack};
use super::wait::{self, Deadline};

/// A decoded frame: its surface, held until the frame is dropped, and
/// what the decoder knows of it.
pub struct Frame {
    pub(crate) surface: Arc<Surface>,
    pub(crate) id: u64,
    pub(crate) poc: i32,
    pub(crate) idr: bool,
    pub(crate) reference: bool,
    pub(crate) rect: Rect,
    pub(crate) coded_size: (u32, u32),
    pub(crate) full_range: bool,
    /// The deadline of the decode call that gave the frame, which its
    /// waits and read back share (`super::wait`).
    pub(crate) deadline: Deadline,
}

impl Frame {
    /// The id of the sample that carried the frame.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The frame's picture order count: its place in display order since
    /// the last IDR picture (H.264's `PicOrderCnt()`, HEVC's
    /// `PicOrderCntVal`).
    pub fn poc(&self) -> i32 {
        self.poc
    }

    /// Whether the frame is an IDR picture.
    pub fn is_idr(&self) -> bool {
        self.idr
    }

    /// Whether other frames may predict from this one.
    pub fn is_reference(&self) -> bool {
        self.reference
    }

    /// The width after the sequence's cropping.
    pub fn width(&self) -> u32 {
        self.rect.width
    }

    /// The height after the sequence's cropping.
    pub fn height(&self) -> u32 {
        self.rect.height
    }

    /// The decoded size before cropping.
    pub fn coded_size(&self) -> (u32, u32) {
        self.coded_size
    }

    /// Whether the samples use the full range (0 to 255 at 8 bits) rather
    /// than the video range (16 to 235): the VUI's `video_full_range_flag`
    /// (H.264 E.2.1, HEVC E.3.1), false without a VUI.
    pub fn is_full_range(&self) -> bool {
        self.full_range
    }

    /// The bits a sample holds: 8 for an NV12 frame, 10 for a P010 one.
    pub fn bit_depth(&self) -> u32 {
        match self.surface.format() {
            SurfaceFormat::Nv12 => 8,
            SurfaceFormat::P010 => 10,
        }
    }

    /// What the frames of this frame's surface pool came back through so
    /// far, and how long each part took.
    pub fn readback_times(&self) -> ReadbackTimes {
        self.surface.readback_times()
    }

    /// Waits until the frame is decoded.
    pub fn sync(&self) -> Result<(), Error> {
        wait::by(&self.deadline, || self.surface.sync())
    }

    /// Reads the frame back where the CPU reaches it, for the copies
    /// [`ReadBack`] makes: waits for the decode, and has the driver put the
    /// frame in host memory, so that those copies run on the CPU alone
    /// (reims-vgpu-video-core's `DecodedFrame::read_back`). The frame's
    /// pool reads no other frame back until the result drops.
    pub fn read_back(&self) -> Result<ReadBack<'_>, Error> {
        Ok(ReadBack {
            frame: self,
            surface: wait::by(&self.deadline, || self.surface.read_back())?,
        })
    }

    /// Copies an 8-bit frame, cropped, into NV12 planes, as
    /// [`ReadBack::copy_nv12_to`] does, reading it back first.
    pub fn copy_nv12_to(
        &self,
        y: &mut [u8],
        y_stride: usize,
        uv: &mut [u8],
        uv_stride: usize,
    ) -> Result<(), Error> {
        self.read_back()?.copy_nv12_to(y, y_stride, uv, uv_stride)
    }

    /// Copies a 10-bit frame, cropped, into P010 planes, as
    /// [`ReadBack::copy_p010_to`] does, reading it back first.
    pub fn copy_p010_to(
        &self,
        y: &mut [u8],
        y_stride: usize,
        uv: &mut [u8],
        uv_stride: usize,
    ) -> Result<(), Error> {
        self.read_back()?.copy_p010_to(y, y_stride, uv, uv_stride)
    }
}

/// A frame read back where the CPU reaches it ([`Frame::read_back`]), kept
/// there until dropped; its copies run on the CPU alone.
pub struct ReadBack<'a> {
    frame: &'a Frame,
    surface: SurfaceReadBack<'a>,
}

impl ReadBack<'_> {
    /// Copies an 8-bit frame, cropped, into NV12 planes: `y` with
    /// `y_stride` bytes a row, and the interleaved Cb/Cr plane `uv` with
    /// `uv_stride`, [`Frame::height`] / 2 rows.
    pub fn copy_nv12_to(
        &self,
        y: &mut [u8],
        y_stride: usize,
        uv: &mut [u8],
        uv_stride: usize,
    ) -> Result<(), Error> {
        self.copy_rect(
            SurfaceFormat::Nv12,
            self.frame.rect,
            y,
            y_stride,
            uv,
            uv_stride,
        )
    }

    /// Copies a 10-bit frame, cropped, into P010 planes: each sample a
    /// little-endian 16-bit word with its 10 bits at the top, `y` with
    /// `y_stride` bytes a row and the interleaved Cb/Cr plane `uv` with
    /// `uv_stride`, [`Frame::height`] / 2 rows. CoreVideo's `x420` and
    /// `xf20` are laid out so.
    pub fn copy_p010_to(
        &self,
        y: &mut [u8],
        y_stride: usize,
        uv: &mut [u8],
        uv_stride: usize,
    ) -> Result<(), Error> {
        self.copy_rect(
            SurfaceFormat::P010,
            self.frame.rect,
            y,
            y_stride,
            uv,
            uv_stride,
        )
    }

    /// [`ReadBack::copy_nv12_to`] or [`ReadBack::copy_p010_to`], by the
    /// frame's depth, into planes of `y_rows` and `uv_rows` rows that may
    /// be smaller than the frame: they take its top left, as much as fits.
    /// A guest's format description may be smaller than its stream, and
    /// the guest's buffers are the description's size.
    pub fn copy_fitting_to(
        &self,
        (y, y_stride, y_rows): (&mut [u8], usize, usize),
        (uv, uv_stride, uv_rows): (&mut [u8], usize, usize),
    ) -> Result<(), Error> {
        let frame = self.frame;
        let format = frame.surface.format();
        let sample = format.bytes_per_sample();
        let fit = |bytes: usize| u32::try_from(bytes / sample.max(1)).unwrap_or(u32::MAX);
        // A chroma row holds a pair for every two columns, the last one too
        // where the width is odd, so an even width fits where its bytes do.
        let chroma_columns = fit(uv_stride) & !1;
        let rect = Rect {
            width: frame
                .rect
                .width
                .min(fit(y_stride))
                .min(chroma_columns.max(1)),
            height: (frame.rect.height)
                .min(u32::try_from(y_rows).unwrap_or(u32::MAX))
                .min(u32::try_from(uv_rows).unwrap_or(u32::MAX).saturating_mul(2)),
            ..frame.rect
        };
        self.copy_rect(format, rect, y, y_stride, uv, uv_stride)
    }

    fn copy_rect(
        &self,
        format: SurfaceFormat,
        rect: Rect,
        y: &mut [u8],
        y_stride: usize,
        uv: &mut [u8],
        uv_stride: usize,
    ) -> Result<(), Error> {
        if self.frame.surface.format() != format {
            return Err(Error::Unsupported(format!(
                "reading a {} frame back as {}",
                self.frame.surface.format().name(),
                format.name()
            )));
        }
        self.surface
            .copy_planes_to(rect, y, y_stride, uv, uv_stride)
    }
}
