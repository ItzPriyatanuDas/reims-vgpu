//! Core's motion-estimation part on VTMotionEstimationSession (macOS 26 and
//! later): the guest's frames copied into buffers of the backend's own,
//! and the session's vectors handed to core as they are, since the host's
//! session runs the same processor as Apple's host, on the same grid, in
//! the same unit and sign.
//!
//! The frames go to VideoToolbox in ordinary pixel buffers this crate
//! makes, never over the guest's memory: the M3 Max's kernel panicked
//! when Apple's scaler wrote into an IOSurface over pages read-only for its
//! clients (measured twice, macOS 27.0.1, 2026-10-24).

use std::sync::Arc;

use reims_vgpu_video_core::backend::{BackendError, PixelFormat};
use reims_vgpu_video_core::fourcc::FourCc;
use reims_vgpu_video_core::motion::{
    BlockSize, MAX_SIDE, MotionEstimationBackend, MotionEstimationCapabilities, MotionEstimator,
    MotionEstimatorConfig, MotionField, MotionPicture, MotionVector, from_half_float,
};
use reims_vgpu_video_core::sync::Mutex;

use super::cf::{self, Cf, Owned, constant};
use super::ffi::core_foundation::{CFDictionaryRef, OSStatus};
use super::ffi::core_video::CVPixelBufferRef;
use super::ffi::motion_estimation::{
    _NSConcreteStackBlock, BLOCK_HAS_COPY_DISPOSE, MotionEstimation, VTMotionEstimationInfoFlags,
    VTMotionEstimationOutputHandler, VTMotionEstimationSessionRef, motion_estimation,
};
use super::pixel_buffer::{PixelBuffer, fourcc_text};
use super::session::Session;
use super::status::native_error;

/// `kCVPixelFormatType_TwoComponent16Half`, the vectors' format.
const VECTOR_FORMAT: FourCc = FourCc::new(*b"2C0h");

/// The size the backend's probe session is made at.
const PROBE_SIDE: u32 = 64;

/// Core's motion part on VideoToolbox: what the host's session takes, asked
/// once.
pub struct VideoToolboxMotionBackend {
    functions: MotionEstimation,
    capabilities: MotionEstimationCapabilities,
}

impl VideoToolboxMotionBackend {
    /// The part, where this macOS has the session and a processor for it:
    /// a 64x64 session is made, and its source attributes read. None, with
    /// the reason, where it has not.
    pub fn new() -> Result<Self, BackendError> {
        let functions = motion_estimation().ok_or_else(|| {
            BackendError::Unsupported("this macOS has no VTMotionEstimationSession".into())
        })?;
        let session = create_session(
            functions,
            &MotionEstimatorConfig {
                width: PROBE_SIDE,
                height: PROBE_SIDE,
                block: BlockSize::Sixteen,
                true_motion: false,
            },
        )?;
        let mut attributes: CFDictionaryRef = std::ptr::null();
        // SAFETY: a live session and a live out pointer.
        let status =
            unsafe { (functions.copy_source_attributes)(session.as_ptr(), &raw mut attributes) };
        // SAFETY: a Copy call's result, owned here, or NULL.
        let attributes = unsafe { Owned::take(attributes.cast()) };
        if status != 0 {
            return Err(native_error(
                "VTMotionEstimationSessionCopySourcePixelBufferAttributes",
                status,
            ));
        }
        let source_formats = attributes
            .as_ref()
            .map(|attributes| source_formats(attributes.cf()))
            .unwrap_or_default();
        if source_formats.is_empty() {
            return Err(BackendError::Unsupported(
                "the motion-estimation session takes none of core's formats".into(),
            ));
        }
        Ok(Self {
            functions,
            capabilities: MotionEstimationCapabilities {
                source_formats,
                // The sizes Apple's processor starts, which core holds a
                // guest to as well (core's `motion::MAX_AREA` too).
                max_width: MAX_SIDE,
                max_height: MAX_SIDE,
            },
        })
    }
}

/// The formats of core's that the session's source attributes list, in
/// their order: `PixelFormatType` is a number or an array of them.
fn source_formats(attributes: Cf<'_>) -> Vec<PixelFormat> {
    let Some(listed) = attributes.get(constant!(core_video::kCVPixelBufferPixelFormatTypeKey))
    else {
        return Vec::new();
    };
    let numbers = listed.i64().map_or_else(
        || listed.values().into_iter().filter_map(Cf::i64).collect(),
        |one| vec![one],
    );
    let mut formats = Vec::new();
    for number in numbers {
        let format = u32::try_from(number)
            .ok()
            .and_then(|code| PixelFormat::from_fourcc(FourCc(code)));
        if let Some(format) = format
            && !formats.contains(&format)
        {
            formats.push(format);
        }
    }
    formats
}

impl MotionEstimationBackend for VideoToolboxMotionBackend {
    fn capabilities(&self) -> MotionEstimationCapabilities {
        self.capabilities.clone()
    }

    fn open(
        &self,
        config: &MotionEstimatorConfig,
    ) -> Result<Box<dyn MotionEstimator>, BackendError> {
        let session = create_session(self.functions, config)?;
        Ok(Box::new(VideoToolboxMotionEstimator {
            functions: self.functions,
            session,
            config: *config,
            sources: None,
        }))
    }
}

/// A VTMotionEstimationSession of `config`'s size and block size, with
/// multi-pass search where the guest asked for true motion.
fn create_session(
    functions: MotionEstimation,
    config: &MotionEstimatorConfig,
) -> Result<Session, BackendError> {
    let size = cf::number_i64(i64::from(config.block.pixels()));
    let options = cf::dictionary(&[
        (functions.motion_vector_size, size.cf()),
        (
            functions.use_multi_pass_search,
            cf::boolean(config.true_motion),
        ),
    ]);
    let mut session: VTMotionEstimationSessionRef = std::ptr::null_mut();
    // SAFETY: a live options dictionary and out pointer.
    let status = unsafe {
        (functions.create)(
            std::ptr::null(),
            options.as_ptr().cast(),
            config.width,
            config.height,
            &raw mut session,
        )
    };
    // SAFETY: a Create call's result, a motion estimation session, which
    // `functions.invalidate` tears down.
    let session = unsafe { Session::take(session, functions.invalidate) };
    if status != 0 {
        return Err(native_error("VTMotionEstimationSessionCreate", status));
    }
    session.ok_or_else(|| {
        BackendError::Malfunction("VTMotionEstimationSessionCreate gave none".into())
    })
}

/// One estimator: a session of the guest's size, and the two source
/// buffers it fills for each pair, kept while the format stays. The
/// session goes first, invalidated, before the buffers it was handed.
struct VideoToolboxMotionEstimator {
    functions: MotionEstimation,
    session: Session,
    config: MotionEstimatorConfig,
    sources: Option<(PixelFormat, PixelBuffer, PixelBuffer)>,
}

impl VideoToolboxMotionEstimator {
    /// The reference's and the current frame's buffers, in `format`.
    fn sources(
        &mut self,
        format: PixelFormat,
    ) -> Result<(&mut PixelBuffer, &mut PixelBuffer), BackendError> {
        if self
            .sources
            .as_ref()
            .is_none_or(|(kept, _, _)| *kept != format)
        {
            let make = || {
                PixelBuffer::new(
                    format.fourcc().0,
                    self.config.width as usize,
                    self.config.height as usize,
                )
            };
            self.sources = Some((format, make()?, make()?));
        }
        let (_, reference, current) = self
            .sources
            .as_mut()
            .ok_or_else(|| BackendError::Malfunction("no source buffers".into()))?;
        Ok((reference, current))
    }
}

impl MotionEstimator for VideoToolboxMotionEstimator {
    fn estimate(
        &mut self,
        reference: &MotionPicture<'_>,
        current: &MotionPicture<'_>,
    ) -> Result<MotionField, BackendError> {
        if reference.pixel_format != current.pixel_format {
            return Err(BackendError::BadData(format!(
                "a {:?} reference with a {:?} current frame",
                reference.pixel_format, current.pixel_format
            )));
        }
        let width = self.config.width;
        let format = reference.pixel_format;
        let (reference_buffer, current_buffer) = self.sources(format)?;
        reference_buffer.fill_420(reference.planes, format, width)?;
        current_buffer.fill_420(current.planes, format, width)?;
        let (reference_buffer, current_buffer) =
            (reference_buffer.as_ptr(), current_buffer.as_ptr());
        let delivered = Arc::new(Mutex::new(None));
        let mut handler = HandlerBlock::new(&delivered);
        // SAFETY: a live session and two live buffers of its size; the
        // handler is a block the session may copy, whose copy holds its own
        // reference to `delivered`.
        let status = unsafe {
            (self.functions.estimate)(
                self.session.as_ptr(),
                reference_buffer,
                current_buffer,
                0,
                std::ptr::null(),
                handler.as_handler(),
            )
        };
        if status != 0 {
            return Err(native_error(
                "VTMotionEstimationSessionEstimateMotionVectors",
                status,
            ));
        }
        // SAFETY: a live session; this waits until the handler has run.
        let status = unsafe { (self.functions.complete_frames)(self.session.as_ptr()) };
        if status != 0 {
            return Err(native_error(
                "VTMotionEstimationSessionCompleteFrames",
                status,
            ));
        }
        drop(handler);
        let output = delivered.lock().take();
        match output {
            None => Err(BackendError::Malfunction(
                "the motion-estimation session completed without its output".into(),
            )),
            Some((status, _)) if status != 0 => Err(native_error(
                "VTMotionEstimationSession's output handler",
                status,
            )),
            Some((_, None)) => Err(BackendError::Malfunction(
                "the motion-estimation session gave no vectors".into(),
            )),
            Some((_, Some(vectors))) => read_field(&vectors, &self.config),
        }
    }
}

/// The session's vectors over the picture: the block size it was made
/// with, ceil(W / 16) x ceil(H / 16) 16x16 blocks of them, left of and
/// above the grid's padding, as two half floats each, x then y.
fn read_field(
    vectors: &PixelBuffer,
    config: &MotionEstimatorConfig,
) -> Result<MotionField, BackendError> {
    if vectors.format() != VECTOR_FORMAT.0 {
        return Err(BackendError::Malfunction(format!(
            "motion vectors in {}",
            fourcc_text(vectors.format())
        )));
    }
    let block = config.block.pixels();
    let per_block = 16 / block;
    let blocks = |side: u32| side.div_ceil(16).checked_mul(per_block);
    let (Some(columns), Some(rows)) = (blocks(config.width), blocks(config.height)) else {
        return Err(BackendError::Malfunction(format!(
            "a {}x{} field of {block}x{block} blocks",
            config.width, config.height
        )));
    };
    let (buffer_width, buffer_height) = vectors.size();
    if (buffer_width as u64) < u64::from(columns) || (buffer_height as u64) < u64::from(rows) {
        return Err(BackendError::Malfunction(format!(
            "a {buffer_width}x{buffer_height} vector buffer for {columns}x{rows} blocks"
        )));
    }
    vectors.read(|planes| {
        let Some((bytes, geometry)) = planes.first() else {
            return Err(BackendError::Malfunction(
                "a vector buffer with no memory".into(),
            ));
        };
        let short = || BackendError::Malfunction("a short vector buffer".into());
        let line_length = (columns as usize).checked_mul(4).ok_or_else(short)?;
        let mut field = Vec::with_capacity((columns as usize).saturating_mul(rows as usize));
        for row in 0..rows as usize {
            let line = row
                .checked_mul(geometry.bytes_per_row)
                .and_then(|start| bytes.get(start..start.checked_add(line_length)?))
                .ok_or_else(short)?;
            for pair in line.as_chunks::<4>().0 {
                field.push(MotionVector {
                    x: from_half_float(u16::from_le_bytes([pair[0], pair[1]])),
                    y: from_half_float(u16::from_le_bytes([pair[2], pair[3]])),
                });
            }
        }
        Ok(MotionField {
            block_size: block,
            columns,
            rows,
            vectors: field,
        })
    })?
}

/// What the handler is given, where the estimator reads it.
type Delivered = Mutex<Option<(OSStatus, Option<PixelBuffer>)>>;

/// The output handler, as a block (clang's "Block Implementation
/// Specification"): the blocks ABI's literal, then what it captures, one
/// reference to where the output goes. It is made on the stack, and its
/// copy and dispose helpers give a copy VideoToolbox keeps a reference of
/// its own, so that the output never outlives its place.
#[repr(C)]
struct HandlerBlock {
    isa: *const std::ffi::c_void,
    flags: i32,
    reserved: i32,
    invoke: unsafe extern "C" fn(
        block: *mut HandlerBlock,
        status: OSStatus,
        info_flags: VTMotionEstimationInfoFlags,
        additional_info: CFDictionaryRef,
        motion_vectors: CVPixelBufferRef,
    ),
    descriptor: *const HandlerDescriptor,
    /// One strong reference of an `Arc<Delivered>`, the block's own.
    delivered: *const Delivered,
}

/// The block's descriptor, with its copy and dispose helpers.
#[repr(C)]
struct HandlerDescriptor {
    reserved: usize,
    size: usize,
    copy: unsafe extern "C" fn(destination: *mut HandlerBlock, source: *const HandlerBlock),
    dispose: unsafe extern "C" fn(block: *const HandlerBlock),
}

static HANDLER_DESCRIPTOR: HandlerDescriptor = HandlerDescriptor {
    reserved: 0,
    size: size_of::<HandlerBlock>(),
    copy: copy_handler,
    dispose: dispose_handler,
};

impl HandlerBlock {
    /// A stack block that delivers into `delivered`, holding a reference
    /// to it until dropped.
    fn new(delivered: &Arc<Delivered>) -> Self {
        Self {
            // SAFETY: the address of the runtime's stack block class, all a
            // block's isa holds.
            isa: unsafe { _NSConcreteStackBlock.as_ptr().cast() },
            flags: BLOCK_HAS_COPY_DISPOSE,
            reserved: 0,
            invoke: invoke_handler,
            descriptor: &raw const HANDLER_DESCRIPTOR,
            delivered: Arc::into_raw(delivered.clone()),
        }
    }

    /// The block, as the session takes it.
    fn as_handler(&mut self) -> VTMotionEstimationOutputHandler {
        (&raw mut *self).cast()
    }
}

impl Drop for HandlerBlock {
    fn drop(&mut self) {
        // SAFETY: the stack block's own reference, from `new`, given back
        // once. A copy the runtime made holds its own, which its dispose
        // helper gives back.
        unsafe { Arc::decrement_strong_count(self.delivered) };
    }
}

/// The block's body: the status, and a reference of its own to the
/// vectors' buffer, put where the estimator reads them.
unsafe extern "C" fn invoke_handler(
    block: *mut HandlerBlock,
    status: OSStatus,
    _info_flags: VTMotionEstimationInfoFlags,
    _additional_info: CFDictionaryRef,
    motion_vectors: CVPixelBufferRef,
) {
    // SAFETY: the runtime calls a block with the block itself, the stack
    // one or a copy, each holding a reference to `delivered`.
    let delivered = unsafe { &*(*block).delivered };
    // SAFETY: VideoToolbox lends the buffer, NULL or live, for the call.
    let vectors = unsafe { PixelBuffer::retain(motion_vectors) };
    *delivered.lock() = Some((status, vectors));
}

/// The copy helper: the runtime has copied the block's bytes into the
/// destination, whose reference to `delivered` is taken here.
unsafe extern "C" fn copy_handler(_destination: *mut HandlerBlock, source: *const HandlerBlock) {
    // SAFETY: `source` is a live block holding a reference, so the count
    // is at least one.
    unsafe { Arc::increment_strong_count((*source).delivered) };
}

/// The dispose helper: a copy the runtime frees gives its reference back.
unsafe extern "C" fn dispose_handler(block: *const HandlerBlock) {
    // SAFETY: the copy's reference, taken by `copy_handler`, given back
    // once, as the runtime disposes of a copy once.
    unsafe { Arc::decrement_strong_count((*block).delivered) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use reims_vgpu_video_conformance::motion::texture::moved_luma;
    use reims_vgpu_video_core::backend::Backend;
    use reims_vgpu_video_core::surfaces::PlaneSource;

    const WIDTH: u32 = 256;
    const HEIGHT: u32 = 128;

    /// A `420v` picture of the texture moved by (`dx`, `dy`), grey chroma.
    fn picture(dx: f64, dy: f64) -> (Vec<u8>, Vec<u8>) {
        let (width, height) = (WIDTH as usize, HEIGHT as usize);
        (
            moved_luma(width, height, width, dx, dy),
            vec![128; width * height / 2],
        )
    }

    /// The picture's two planes, as core lends them.
    fn planes((luma, chroma): &(Vec<u8>, Vec<u8>)) -> [PlaneSource<'_>; 2] {
        [
            PlaneSource {
                bytes: luma,
                bytes_per_row: WIDTH as usize,
                rows: HEIGHT,
            },
            PlaneSource {
                bytes: chroma,
                bytes_per_row: WIDTH as usize,
                rows: HEIGHT / 2,
            },
        ]
    }

    fn estimate(block: BlockSize, shift: (f64, f64)) -> Result<MotionField, BackendError> {
        let backend = VideoToolboxMotionBackend::new()?;
        let mut estimator = backend.open(&MotionEstimatorConfig {
            width: WIDTH,
            height: HEIGHT,
            block,
            true_motion: false,
        })?;
        let (still, moved) = (picture(0.0, 0.0), picture(shift.0, shift.1));
        let (still, moved) = (planes(&still), planes(&moved));
        estimator.estimate(
            &MotionPicture {
                pixel_format: PixelFormat::Nv12VideoRange,
                planes: &still,
            },
            &MotionPicture {
                pixel_format: PixelFormat::Nv12VideoRange,
                planes: &moved,
            },
        )
    }

    /// The share of the field's vectors that are (`x`, `y`).
    fn share(field: &MotionField, x: f32, y: f32) -> f64 {
        let hits = field
            .vectors
            .iter()
            .filter(|vector| vector.x == x && vector.y == y)
            .count();
        hits as f64 / field.vectors.len().max(1) as f64
    }

    /// The host has the session (macOS 26 and later) and a processor for
    /// it, which takes core's 8-bit formats.
    #[test]
    fn this_mac_estimates_motion_from_420v() -> Result<(), BackendError> {
        let backend = VideoToolboxMotionBackend::new()?;
        let capabilities = backend.capabilities();
        assert!(
            capabilities
                .source_formats
                .contains(&PixelFormat::Nv12VideoRange),
            "{capabilities:?}"
        );
        Ok(())
    }

    /// Content moved by (+8, +4) gives (-8, -4) in pixels on 16x16 blocks,
    /// one a block over the picture alone, as Apple's host gave it (122 of
    /// 128).
    #[test]
    fn a_moved_texture_gives_the_way_back_on_16x16_blocks() -> Result<(), BackendError> {
        let field = estimate(BlockSize::Sixteen, (8.0, 4.0))?;
        assert_eq!((field.block_size, field.columns, field.rows), (16, 16, 8));
        assert!(share(&field, -8.0, -4.0) >= 0.9, "{:?}", field.vectors);
        Ok(())
    }

    /// The same on 4x4 blocks: four a side of each 16x16 block.
    #[test]
    fn a_moved_texture_gives_the_way_back_on_4x4_blocks() -> Result<(), BackendError> {
        let field = estimate(BlockSize::Four, (3.0, -2.0))?;
        assert_eq!((field.block_size, field.columns, field.rows), (4, 64, 32));
        assert!(
            share(&field, -3.0, 2.0) >= 0.9,
            "{:?}",
            share(&field, -3.0, 2.0)
        );
        Ok(())
    }

    /// The conformance suite's motion cases, through the host session as a
    /// guest asks (`conformance hardware motion-check`).
    #[test]
    fn the_conformance_motion_cases_pass() -> Result<(), BackendError> {
        let backend = Backend {
            motion: Some(Arc::new(VideoToolboxMotionBackend::new()?)),
            ..Backend::none("VideoToolbox motion")
        };
        for case in reims_vgpu_video_conformance::motion::check::cases() {
            let (passed, line) =
                reims_vgpu_video_conformance::motion::check::check(&backend, &case);
            assert!(passed, "{line}");
        }
        Ok(())
    }

    /// A pair of different formats is refused before VideoToolbox sees it.
    #[test]
    fn frames_of_two_formats_are_refused() -> Result<(), BackendError> {
        let backend = VideoToolboxMotionBackend::new()?;
        let mut estimator = backend.open(&MotionEstimatorConfig {
            width: WIDTH,
            height: HEIGHT,
            block: BlockSize::Sixteen,
            true_motion: false,
        })?;
        let (luma, chroma) = picture(0.0, 0.0);
        let planes = [
            PlaneSource {
                bytes: &luma,
                bytes_per_row: WIDTH as usize,
                rows: HEIGHT,
            },
            PlaneSource {
                bytes: &chroma,
                bytes_per_row: WIDTH as usize,
                rows: HEIGHT / 2,
            },
        ];
        let refused = estimator.estimate(
            &MotionPicture {
                pixel_format: PixelFormat::Nv12VideoRange,
                planes: &planes,
            },
            &MotionPicture {
                pixel_format: PixelFormat::Nv12FullRange,
                planes: &planes,
            },
        );
        assert!(
            matches!(refused, Err(BackendError::BadData(_))),
            "{refused:?}"
        );
        Ok(())
    }
}
