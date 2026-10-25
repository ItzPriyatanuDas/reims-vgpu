//! The JPEG engine: `jcap`, `jdec` and `jenc`, which the host session
//! answers itself.
//!
//! Apple's host checks the message and hands the rest to CMPhoto's
//! `CMPhotoParavirtualizedHostJPEGHardware*` functions, whose answers are
//! the contract here: this host reads the options as CMPhoto's do, works
//! out the region, scale and destination as its hardware takes them, and
//! decodes on the backend's [`JpegDecoder`]. What CMPhoto does is read in
//! the 26.7.1 guest's CMPhoto, and measured on the M3 Max
//! (docs/video/devices.md, "The JPEG engine").
//!
//! No backend call runs inside a lend of a guest surface: the source is
//! copied out of its read lend, the backend decodes into a buffer of the
//! host's on a thread of its own, and the output is copied into the
//! destination's write lend after, by the CPU alone. The delivering thread
//! waits for that thread 10 s at most, as a session's end waits for its
//! thread; past it, the decode is answered kIOReturnTimeout and its thread
//! is left running, detached, holding no guest surface (ours).

use std::sync::Arc;
use std::time::Duration;

use crate::host::mapped_surfaces::SurfaceHold;
use crate::host::names;
use crate::host::session_thread::{DetachedThreads, SessionThread, Unanswered};
use crate::jpeg::{
    JpegCapabilities, JpegDecode, JpegDecoder, JpegError, JpegOutputFormat, JpegSubsampling,
    PixelRect,
};
use crate::runtime::{self, Runtime};
use crate::status::{NO_ERROR, OsStatus, Status};
use crate::surfaces::PlaneDestination;
use crate::sync::Mutex;
use crate::wire::WireError;
use crate::wire::message::{Message, MessageFlags, MessageWriter};
use crate::wire::property_list::{dictionary, integer};

/// Which JPEG message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JpegOperation {
    /// `jcap`.
    Capabilities,
    /// `jdec`.
    Decode,
    /// `jenc`.
    Encode,
}

/// CMPhoto's status where it has no JPEG hardware: its capabilities'
/// status starts so and stays so when the hardware does not open, and
/// x86_64 CMPhoto's three host functions return it. The name is ours.
const NO_JPEG_HARDWARE: OsStatus = -16994;

/// CMPhoto's status for options it cannot read: no options, or a key
/// absent or not a number. The name is ours.
const BAD_OPTIONS: OsStatus = -16990;

/// kIOReturnError: Apple's JPEG hardware's answer for a destination format
/// it does not write (IOKit's `IOReturn.h`).
const IO_RETURN_ERROR: OsStatus = 0xe000_02bc_u32.cast_signed();
/// kIOReturnBadArgument: for a destination smaller than the decode, a crop
/// past the image, or a scale under 1/8. Apple's hardware answers it for a
/// decode under 24 blocks of area too, as its own limit: the backend's
/// decoder takes any.
const IO_RETURN_BAD_ARGUMENT: OsStatus = 0xe000_02c2_u32.cast_signed();
/// kIOReturnUnsupported: what this host answers for a decode its backend
/// cannot do that Apple's hardware does, such as a scale (ours).
const IO_RETURN_UNSUPPORTED: OsStatus = 0xe000_02c7_u32.cast_signed();
/// kIOReturnBadMedia: for a stream that does not decode, or whose size is
/// not the options'.
const IO_RETURN_BAD_MEDIA: OsStatus = 0xe000_02d1_u32.cast_signed();
/// kIOReturnDeviceError: for a stream not sampled as the options say, or
/// whose scan ends before its last MCU.
const IO_RETURN_DEVICE_ERROR: OsStatus = 0xe000_02e9_u32.cast_signed();

/// kIOReturnTimeout: ours, for a decode the backend has not finished in
/// [`JpegEngine::wait`]. Apple's host would wait for its hardware for
/// ever; its guest has stopped waiting long before, after 1.1 s.
const IO_RETURN_TIMEOUT: OsStatus = 0xe000_02d6_u32.cast_signed();
/// kIOReturnBusy: ours, for a decode while the process has as many
/// threads left detached as it keeps ([`DetachedThreads`]): a backend
/// that has stopped returning gets no more.
const IO_RETURN_BUSY: OsStatus = 0xe000_02d5_u32.cast_signed();

/// The largest output buffer the engine keeps for its next decode, so
/// that a decode after the first faults no new pages in: a 4000x3000
/// `BGRA` output is 48 MB. Ours.
const KEPT_OUTPUT_BYTES: usize = 64 << 20;

/// The name of a decode's thread.
const THREAD_NAME: &str = "reims-vgpu-video.jpeg";

/// The protocol of the capabilities: CMPhoto's builder writes 1, and its
/// reader needs the key.
const CAPABILITIES_VERSION: i64 = 1;

/// The JPEG engine, as the host session gives it to [`answer`].
pub(crate) struct JpegEngine<'a> {
    /// The backend's decoder.
    pub(crate) decoder: &'a Arc<dyn JpegDecoder>,
    /// How long a decode's thread is waited for: the session threads'
    /// bound.
    pub(crate) wait: Duration,
    /// The threads left detached in the process, which a decode's thread
    /// left running joins.
    pub(crate) detached: &'a Arc<DetachedThreads>,
    /// The buffer the last decode's output went into, kept for the next.
    pub(crate) output: &'a Mutex<Vec<u8>>,
    /// Where a decode's thread comes from, and the clock its wait and its
    /// timing are on.
    pub(crate) runtime: &'a Arc<dyn Runtime>,
}

/// The reply to `operation`'s `request`, with `surfaces` the message's.
/// It always has `,err`; `jcap`'s has `,jca` when there is an engine.
pub(crate) fn answer(
    request: &Message<'_>,
    operation: JpegOperation,
    surfaces: &[Arc<SurfaceHold>],
    engine: Option<&JpegEngine<'_>>,
) -> Result<Vec<u8>, WireError> {
    let mut reply = MessageWriter::reply_to(&request.header(), MessageFlags::REPLY_TO_GUEST);
    let status = match operation {
        JpegOperation::Capabilities if !surfaces.is_empty() => Status::Parameter.code(),
        JpegOperation::Capabilities => match engine {
            Some(engine) => {
                reply.property_list(
                    names::JPEG_CAPABILITIES,
                    Some(&capabilities(engine.decoder.capabilities())),
                )?;
                NO_ERROR
            }
            None => NO_JPEG_HARDWARE,
        },
        JpegOperation::Decode | JpegOperation::Encode => {
            match (request.dictionary(names::JPEG_OPTIONS), surfaces) {
                (Err(error), _) => error.status().code(),
                (Ok(options), [source, destination]) => match (operation, engine) {
                    (JpegOperation::Decode, Some(engine)) => {
                        decode(options.as_ref(), source, destination, engine)
                    }
                    // No JPEG encoder on any backend yet: the answer of a
                    // CMPhoto without the hardware.
                    _ => NO_JPEG_HARDWARE,
                },
                (Ok(_), _) => Status::Parameter.code(),
            }
        }
    };
    reply.s32(names::ERROR, status);
    Ok(reply.finish())
}

/// `,jca`, in the keys and types CMPhoto's builder writes: two numbers,
/// then four booleans.
fn capabilities(capabilities: JpegCapabilities) -> plist::Value {
    plist::Value::Dictionary(dictionary([
        ("Version", integer(CAPABILITIES_VERSION)),
        ("NumberOfCores", integer(i64::from(capabilities.cores))),
        (
            "SupportsInterchangeFormats",
            plist::Value::Boolean(capabilities.interchange_formats),
        ),
        (
            "SupportsMissingEOI",
            plist::Value::Boolean(capabilities.missing_eoi),
        ),
        (
            "SupportsRSTLogging",
            plist::Value::Boolean(capabilities.restart_logging),
        ),
        (
            "Supports16BitQuantizationTables",
            plist::Value::Boolean(capabilities.sixteen_bit_quantization_tables),
        ),
    ]))
}

/// The options of a decode, as CMPhoto's host function reads them: each
/// key a number, every one but `InputSize` and
/// `CropRect` required. `InputSize` is not kept: Apple's host decoded the
/// whole stream with it absent, and with it at 1000 of a 93 KB stream, so
/// this host reads the whole source to the stream's EOI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DecodeOptions {
    width: i64,
    height: i64,
    subsampling: i64,
    mcus_across: i64,
    mcus_down: i64,
    decode_width: i64,
    decode_height: i64,
    /// In MCUs: x, y, width, height.
    crop: Option<[u64; 4]>,
}

impl DecodeOptions {
    /// The options, or `None` where CMPhoto answers [`BAD_OPTIONS`].
    fn read(options: &plist::Dictionary) -> Option<Self> {
        let number = |key: &str| options.get(key).and_then(number);
        // Flags is required but changes nothing this host does: CMPhoto
        // passes it to its hardware, and 1 to 256 changed nothing measured.
        number("Flags")?;
        let crop = match options.get("CropRect") {
            None => None,
            Some(rect) => Some(crop_rect(rect)?),
        };
        Some(Self {
            width: number("Width")?,
            height: number("Height")?,
            subsampling: number("Subsampling")?,
            mcus_across: number("MCUsHor")?,
            mcus_down: number("MCUsVer")?,
            decode_width: number("DecodeWidth")?,
            decode_height: number("DecodeHeight")?,
            crop,
        })
    }
}

/// A CFNumber's value as CMPhoto reads it into an integer: an integer as
/// it is, a real truncated toward zero, as `CFNumberGetValue` truncates
/// one.
fn number(value: &plist::Value) -> Option<i64> {
    match value {
        plist::Value::Integer(integer) => integer.as_signed(),
        plist::Value::Real(real) => Some(truncate_to_i64(*real)),
        _ => None,
    }
}

/// `real` truncated toward zero into an `i64`, held to its range, NaN 0.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "a float to an integer: `as` truncates toward zero and saturates, NaN to 0"
)]
fn truncate_to_i64(real: f64) -> i64 {
    real as i64
}

/// `CropRect`, a CGRect's dictionary, as CMPhoto reads it
/// (`CGRectMakeWithDictionaryRepresentation`, then each value rounded and
/// made unsigned). `None` where it does not read.
fn crop_rect(rect: &plist::Value) -> Option<[u64; 4]> {
    let rect = rect.as_dictionary()?;
    let mut values = [0u64; 4];
    for (value, key) in values.iter_mut().zip(["X", "Y", "Width", "Height"]) {
        let real = match rect.get(key)? {
            plist::Value::Real(real) => *real,
            // A CGFloat holds it as a double.
            plist::Value::Integer(integer) => nearest_f64(integer.as_signed()?),
            _ => return None,
        };
        *value = round_to_u64(real);
    }
    Some(values)
}

/// The nearest `f64` to `integer`, as a CGFloat holds it.
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "an integer to a float, the nearest one"
)]
fn nearest_f64(integer: i64) -> f64 {
    integer as f64
}

/// `real` rounded half away from zero, then held to `u64`'s range, NaN
/// 0, as CMPhoto makes a crop's value unsigned.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a float to an integer: `as` saturates, NaN to 0"
)]
fn round_to_u64(real: f64) -> u64 {
    real.round() as u64
}

/// `jdec`: the status of the decode of `source`'s JPEG into
/// `destination`.
fn decode(
    options: Option<&plist::Dictionary>,
    source: &SurfaceHold,
    destination: &SurfaceHold,
    engine: &JpegEngine<'_>,
) -> OsStatus {
    let Some(options) = options.and_then(DecodeOptions::read) else {
        return BAD_OPTIONS;
    };
    let Some(subsampling) = JpegSubsampling::from_code(options.subsampling) else {
        // A number CMPhoto never sends: the hardware's answer for a
        // subsampling that is not the stream's (ours, for an unknown one).
        return IO_RETURN_DEVICE_ERROR;
    };
    let (Ok(width), Ok(height)) = (u32::try_from(options.width), u32::try_from(options.height))
    else {
        return IO_RETURN_BAD_MEDIA;
    };
    let Some(region) = region(&options, subsampling, width, height) else {
        return IO_RETURN_BAD_ARGUMENT;
    };
    let Some(scale) = scale(region, options.decode_width, options.decode_height) else {
        return IO_RETURN_BAD_ARGUMENT;
    };
    let layout = destination.surface.layout();
    let Some(format) = JpegOutputFormat::from_fourcc(layout.pixel_format) else {
        return IO_RETURN_ERROR;
    };
    if layout.planes.len().max(1) != format.plane_count() {
        return IO_RETURN_ERROR;
    }
    if engine.detached.are_full() {
        log::error!(
            "jdec refused: {} threads are left detached in this process",
            engine.detached.count()
        );
        return IO_RETURN_BUSY;
    }
    // Apple's host copies the whole source first, and this one decodes all
    // of it, to the stream's EOI (`DecodeOptions`). The copy is what the
    // backend reads, after the lend.
    let mut data = Vec::new();
    let read = source.surface.read(&mut |bytes| data = bytes.to_vec());
    if let Err(error) = read {
        // CMPhoto's answer for a surface whose memory it cannot reach.
        log::warn!("jdec: the source's bytes do not read: {error}");
        return BAD_OPTIONS;
    }
    let source_bytes = data.len();
    let job = Job {
        data,
        width,
        height,
        subsampling,
        region,
        scale,
        format,
    };
    let (output_width, output_height) = job.request().output_size();
    if layout.width < output_width || layout.height < output_height {
        return IO_RETURN_BAD_ARGUMENT;
    }
    let shapes = output_planes(format, output_width, output_height);
    let failed = |error: &JpegError| {
        log::warn!(
            "jdec of {source_bytes} bytes, {width}x{height} {subsampling:?} into {format:?}: {error}"
        );
        status(error)
    };
    // The destination is lent first, and nothing written, to check that
    // the host can write it and that its planes hold the output: so the
    // backend is asked only for a decode the host can deliver, as when it
    // wrote into the destination itself, and the host's buffer for the
    // output is no larger than the guest's planes.
    let mut checked = Ok(true);
    let lent = destination.surface.write(&mut |memory| {
        checked = layout
            .split(memory)
            .map(|planes| planes_hold(&planes, &shapes));
    });
    if let Err(error) = lent {
        log::warn!("jdec: the destination cannot be written: {error}");
        return BAD_OPTIONS;
    }
    match checked {
        Ok(true) => {}
        // As for a destination smaller than the decode: the surface's size
        // is the layout's, and its planes are the guest's.
        Ok(false) => {
            log::warn!("jdec: the destination's planes do not hold {output_width}x{output_height}");
            return IO_RETURN_BAD_ARGUMENT;
        }
        Err(error) => return failed(&JpegError::Malfunction(error.to_string())),
    }
    let started = engine.runtime.now();
    let output = match decode_off_the_lends(engine, job, &shapes) {
        Ok(output) => output,
        Err(Failed::Backend(error)) => return failed(&error),
        Err(Failed::Engine(status)) => return status,
    };
    let decoded = engine.runtime.elapsed_since(started);
    let mut copied = Ok(());
    let written = destination.surface.write(&mut |memory| {
        copied = layout
            .split(memory)
            .map(|mut planes| copy_output(&output, &shapes, &mut planes));
    });
    engine.keep(output);
    if let Err(error) = written {
        log::warn!("jdec: the destination cannot be written: {error}");
        return BAD_OPTIONS;
    }
    if let Err(error) = copied {
        return failed(&JpegError::Malfunction(error.to_string()));
    }
    log::debug!(
        "jdec of {source_bytes} bytes into {output_width}x{output_height} {format:?}: \
         decoded off the lends in {:.2} ms, copied into the destination in {:.2} ms",
        decoded.as_secs_f64() * 1000.0,
        (engine
            .runtime
            .elapsed_since(started)
            .saturating_sub(decoded))
        .as_secs_f64()
            * 1000.0
    );
    NO_ERROR
}

/// One decode, owned, for its thread: what a [`JpegDecode`] borrows.
struct Job {
    data: Vec<u8>,
    width: u32,
    height: u32,
    subsampling: JpegSubsampling,
    region: PixelRect,
    scale: u32,
    format: JpegOutputFormat,
}

impl Job {
    fn request(&self) -> JpegDecode<'_> {
        JpegDecode {
            data: &self.data,
            width: self.width,
            height: self.height,
            subsampling: self.subsampling,
            region: self.region,
            scale: self.scale,
            format: self.format,
        }
    }
}

/// Why a decode off the lends has no output.
enum Failed {
    /// The backend's error, or the host's own as a malfunction.
    Backend(JpegError),
    /// The engine's own answer: the bound passed.
    Engine(OsStatus),
}

impl JpegEngine<'_> {
    /// Keeps `output` for the next decode, unless it is larger than
    /// [`KEPT_OUTPUT_BYTES`].
    fn keep(&self, output: Vec<u8>) {
        if output.capacity() <= KEPT_OUTPUT_BYTES {
            *self.output.lock() = output;
        }
    }
}

/// The backend's decode of `job` into a buffer of the host's, laid out as
/// `shapes` with no padding, on a thread of its own and with no lend held,
/// waited [`JpegEngine::wait`] at most. The buffer is the last decode's,
/// not cleared: the backend writes all of the output.
fn decode_off_the_lends(
    engine: &JpegEngine<'_>,
    job: Job,
    shapes: &[(u64, u32)],
) -> Result<Vec<u8>, Failed> {
    let length = shapes
        .iter()
        .try_fold(0u64, |sum, &(row, rows)| {
            row.checked_mul(u64::from(rows))?.checked_add(sum)
        })
        .and_then(|length| usize::try_from(length).ok())
        .ok_or_else(|| Failed::Backend(JpegError::Malfunction("the output is too large".into())))?;
    let mut output = std::mem::take(&mut *engine.output.lock());
    output.resize(length, 0);
    let decoder = Arc::clone(engine.decoder);
    let shapes = shapes.to_vec();
    let (answer, answered) = runtime::answer();
    let thread = SessionThread::spawn(
        engine.runtime,
        THREAD_NAME,
        "a JPEG decode",
        engine.wait,
        engine.detached,
        move || {
            let mut output = output;
            let result = decoder.decode(
                &job.request(),
                &mut output_destinations(&mut output, &shapes),
            );
            answer.give((output, result));
        },
    )
    .map_err(|error| {
        log::error!("cannot start a JPEG decode's thread: {error}");
        Failed::Backend(JpegError::Malfunction(error.to_string()))
    })?;
    match thread.answer(&answered) {
        Ok((output, Ok(()))) => Ok(output),
        Ok((output, Err(error))) => {
            engine.keep(output);
            Err(Failed::Backend(error))
        }
        Err(Unanswered::Detached) => Err(Failed::Engine(IO_RETURN_TIMEOUT)),
        Err(Unanswered::Ended) => Err(Failed::Backend(JpegError::Malfunction(
            "the decode's thread panicked".into(),
        ))),
    }
}

/// The rows and the bytes of a row each plane of a `width` by `height`
/// output in `format` needs, as [`JpegDecoder::decode`] promises a
/// backend.
fn output_planes(format: JpegOutputFormat, width: u32, height: u32) -> Vec<(u64, u32)> {
    let pairs = u64::from(width.div_ceil(2));
    let width = u64::from(width);
    match format {
        JpegOutputFormat::Biplanar420 => vec![(width, height), (pairs * 2, height.div_ceil(2))],
        JpegOutputFormat::Biplanar422 => vec![(width, height), (pairs * 2, height)],
        JpegOutputFormat::Biplanar444 => vec![(width, height), (width * 2, height)],
        JpegOutputFormat::Yuyv | JpegOutputFormat::Uyvy => vec![(pairs * 4, height)],
        JpegOutputFormat::Bgra => vec![(width * 4, height)],
    }
}

/// `output`, the host's buffer, as the planes `shapes` lays out, one
/// after another with no padding. `output` holds them all.
fn output_destinations<'a>(
    mut output: &'a mut [u8],
    shapes: &[(u64, u32)],
) -> Vec<PlaneDestination<'a>> {
    let mut planes = Vec::with_capacity(shapes.len());
    for &(row, rows) in shapes {
        let row = usize::try_from(row).unwrap_or(usize::MAX);
        let length = row.saturating_mul(usize::try_from(rows).unwrap_or(usize::MAX));
        let (bytes, rest) = output.split_at_mut(length.min(output.len()));
        planes.push(PlaneDestination {
            bytes,
            bytes_per_row: row,
            rows,
        });
        output = rest;
    }
    planes
}

/// Copies the output, laid out in `output` as [`output_destinations`]
/// lays it, into the top-left corner of `planes`, which hold it
/// ([`planes_hold`]): the CPU's copy alone. What lies outside the corner
/// is left as it was.
fn copy_output(output: &[u8], shapes: &[(u64, u32)], planes: &mut [PlaneDestination<'_>]) {
    let mut from = output;
    for (&(row, rows), plane) in shapes.iter().zip(planes) {
        let row = usize::try_from(row).unwrap_or(usize::MAX);
        let length = row.saturating_mul(usize::try_from(rows).unwrap_or(usize::MAX));
        let (this, rest) = from.split_at(length.min(from.len()));
        from = rest;
        if plane.bytes_per_row == row {
            if let Some(to) = plane.bytes.get_mut(..this.len()) {
                to.copy_from_slice(this);
            }
            continue;
        }
        for (index, source) in this.chunks_exact(row.max(1)).enumerate() {
            let Some(at) = index.checked_mul(plane.bytes_per_row) else {
                break;
            };
            let Some(to) = at
                .checked_add(row)
                .and_then(|end| plane.bytes.get_mut(at..end))
            else {
                break;
            };
            to.copy_from_slice(source);
        }
    }
}

/// Whether `planes` hold an output whose planes are `shapes`: each plane
/// with the output's rows and each row's bytes.
fn planes_hold(planes: &[PlaneDestination<'_>], shapes: &[(u64, u32)]) -> bool {
    planes.len() == shapes.len()
        && planes.iter().zip(shapes).all(|(plane, &(row, rows))| {
            u64::try_from(plane.bytes_per_row).is_ok_and(|bytes| bytes >= row) && plane.rows >= rows
        })
}

/// The status Apple's host gives for the like of `error`.
fn status(error: &JpegError) -> OsStatus {
    match error {
        JpegError::BadData(_)
        | JpegError::UnsupportedStream(_)
        | JpegError::SizeMismatch { .. } => IO_RETURN_BAD_MEDIA,
        // Measured: a scan cut short and closed with its EOI is
        // kIOReturnDeviceError on Apple's hardware, where one without its
        // EOI is kIOReturnBadMedia.
        JpegError::SubsamplingMismatch | JpegError::ScanEndsEarly | JpegError::Malfunction(_) => {
            IO_RETURN_DEVICE_ERROR
        }
        JpegError::UnsupportedFormat(_) => IO_RETURN_ERROR,
        JpegError::UnsupportedScale(_) => IO_RETURN_UNSUPPORTED,
    }
}

/// The part of the image the options ask for, in pixels: all of it, or the
/// crop's MCUs, cut at the image's edges. A crop of all the MCUs is no crop,
/// as CMPhoto reads it. `None` for a crop that is empty or past the
/// options' MCUs, which the hardware refuses (measured: a crop past the
/// image is kIOReturnBadArgument; the exact bound is ours), and for an
/// image of no width or height, which has nothing to decode: Apple's
/// hardware refuses it as under its least area, and the refusal is kept
/// here, as it asks no capability of the backend (ours).
fn region(
    options: &DecodeOptions,
    subsampling: JpegSubsampling,
    width: u32,
    height: u32,
) -> Option<PixelRect> {
    if width == 0 || height == 0 {
        return None;
    }
    let whole = PixelRect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let Some([x, y, across, down]) = options.crop else {
        return Some(whole);
    };
    let (mcus_across, mcus_down) = (
        u64::try_from(options.mcus_across).ok()?,
        u64::try_from(options.mcus_down).ok()?,
    );
    if [x, y, across, down] == [0, 0, mcus_across, mcus_down] {
        return Some(whole);
    }
    let (end_across, end_down) = (x.checked_add(across)?, y.checked_add(down)?);
    if across == 0 || down == 0 || end_across > mcus_across || end_down > mcus_down {
        return None;
    }
    let (mcu_width, mcu_height) = subsampling.mcu_size();
    let left = u32::try_from(x).ok()?.checked_mul(mcu_width)?;
    let top = u32::try_from(y).ok()?.checked_mul(mcu_height)?;
    if left >= width || top >= height {
        return None;
    }
    let right = u32::try_from(end_across)
        .ok()?
        .checked_mul(mcu_width)?
        .min(width);
    let bottom = u32::try_from(end_down)
        .ok()?
        .checked_mul(mcu_height)?
        .min(height);
    Some(PixelRect {
        x: left,
        y: top,
        width: right.checked_sub(left)?,
        height: bottom.checked_sub(top)?,
    })
}

/// The scale the hardware picks for a decode to `decode_width` by
/// `decode_height`: the least of 1, 2, 4 and 8 at which the region, its
/// size divided down, fits either way. `None` where none does. From what
/// was measured on Apple's host: 320x240, 500x375 and 160x120 of a 640x480
/// image decode at 1/2, 1/2 and 1/4, 320x480 at 1, 167x106 and 166x105 of
/// a 333x211 one at 1/2, and 40x30 is refused. The rule is ours.
fn scale(region: PixelRect, decode_width: i64, decode_height: i64) -> Option<u32> {
    [1u32, 2, 4, 8].into_iter().find(|&scale| {
        i64::from(region.width / scale) <= decode_width
            || i64::from(region.height / scale) <= decode_height
    })
}

#[cfg(test)]
mod tests;
