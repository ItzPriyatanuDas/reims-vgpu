//! The crate's waits past their deadlines on a GPU (`super`): a decode, a
//! staged copy back's run, an encode and a JPEG decode into host memory,
//! each asked after with no time to run at all, so that each is still
//! running at its deadline. Run on the VA-API host, on the render node
//! `RVG_VAAPI_NODE` names (`/dev/dri/renderD128` where it is unset).

use std::sync::atomic::Ordering;
use std::time::Duration;

use reims_vgpu_video_core::jpeg::{
    JpegDecode, JpegDecoder, JpegError, JpegOutputFormat, JpegSubsampling, PixelRect,
};
use reims_vgpu_video_core::runtime::{Runtime, StdRuntime};
use reims_vgpu_video_core::surfaces::PlaneDestination;

use super::kept;
use super::tests::{KEPT_EVER, WAIT, within_5_s};
use crate::encode::{EncoderSettings, InputFrame, Plane, Profile};
use crate::va::display::{Display, Error};
use crate::va::encoder::VideoEncoder;
use crate::va::frame::Frame;
use crate::va::h264::H264Decoder;
use crate::va::jpeg::VaapiJpegDecoder;

/// The size the tests decode and encode at: a 4K frame takes the GPU
/// milliseconds.
pub(super) const SIZE: (u32, u32) = (3840, 2160);

/// How many tries a test makes for work still running when it is asked
/// after: it may be done before.
const TRIES: usize = 20;

/// The wait an encode's frame takes: long enough for its upload's map of
/// an idle surface, made on the display's call thread (1.3 ms on the RX
/// 7900 XTX, `crate::va::calls`), short of a 4K encode.
const ENCODE_WAIT: Duration = Duration::from_millis(4);

pub(super) fn display() -> Display {
    let node = std::env::var("RVG_VAAPI_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
    Display::open(node, StdRuntime::shared()).expect("a VA display")
}

/// Frame `n` of noise at [`SIZE`], NV12.
pub(super) fn noise(n: u64) -> Vec<u8> {
    let (width, height) = SIZE;
    let len = usize::try_from(u64::from(width) * u64::from(height) * 3 / 2).unwrap_or(0);
    let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ n.wrapping_mul(0x2545_f491_4f6c_dd1d);
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}

/// `bytes`, frame `n`, as the encoder takes it.
pub(super) fn input(bytes: &[u8], n: u64) -> InputFrame<'_> {
    let row = SIZE.0 as usize;
    let (luma, chroma) = bytes.split_at(row * SIZE.1 as usize);
    InputFrame {
        planes: [
            Plane {
                data: luma,
                bytes_per_row: row,
            },
            Plane {
                data: chroma,
                bytes_per_row: row,
            },
        ],
        pts: i64::try_from(n).unwrap_or(0) * 3000,
        duration: 3000,
        force_key_frame: false,
        id: n,
    }
}

/// An H.264 encoder at [`SIZE`] with no B pictures.
pub(super) fn encoder(display: &Display) -> VideoEncoder {
    let mut settings = EncoderSettings::new(Profile::H264High, SIZE.0, SIZE.1);
    settings.allow_frame_reordering = false;
    VideoEncoder::new(display, settings).expect("an H.264 encoder")
}

/// `count` frames of noise, H.264 by the driver's encoder: its avcC, and
/// its samples.
pub(super) fn h264(display: &Display, count: u64) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut encoder = encoder(display);
    let mut samples = Vec::new();
    for n in 0..count {
        let bytes = noise(n);
        let coded = encoder.encode(&input(&bytes, n)).expect("a frame taken");
        samples.extend(coded.into_iter().map(|c| c.expect("a sample").data));
    }
    samples.extend(
        encoder
            .flush()
            .into_iter()
            .map(|c| c.expect("a sample").data),
    );
    (encoder.configuration().to_vec(), samples)
}

/// Decodes `samples`' first, an IDR picture, and waits for it. A
/// decoder's first `vaEndPicture` waits for its answer on the display's
/// call thread, which starts the frame's deadline (`crate::va::buffer`), so
/// a test that shortens the wait once its frames are decoded decodes them
/// after this.
pub(super) fn begin(decoder: &mut H264Decoder, samples: &[Vec<u8>]) {
    let mut frames = decoder
        .decode(&samples[0], 1 << 40)
        .expect("a first decode");
    frames.extend(decoder.flush().expect("a flush"));
    for frame in &frames {
        frame.sync().expect("a first decode, the GPU free");
    }
}

/// Every frame `samples` decode into, flushed at the end.
pub(super) fn decoded(decoder: &mut H264Decoder, samples: &[Vec<u8>]) -> Vec<Frame> {
    let mut frames = Vec::new();
    for (id, sample) in (0u64..).zip(samples) {
        frames.extend(decoder.decode(sample, id).expect("a decode"));
    }
    frames.extend(decoder.flush().expect("a flush"));
    frames
}

pub(super) fn stalled<T>(result: &Result<T, Error>) -> bool {
    matches!(result, Err(Error::Stalled(_)))
}

/// The size of the tests' JPEGs, whose scan a build of the tests walks in
/// milliseconds.
pub(super) const JPEG_SIZE: (u32, u32) = (1920, 1080);

/// A baseline 4:2:0 JPEG of `width` by `height`, every sample mid-grey:
/// one quantization table of ones, Annex K's Huffman tables (none in the
/// stream), and each MCU four luma blocks and two chroma ones of a DC
/// difference of 0 and an end of block.
fn grey_jpeg(width: u16, height: u16) -> Vec<u8> {
    let mut out = vec![0xff, 0xd8];
    let segment = |out: &mut Vec<u8>, marker: u8, payload: &[u8]| {
        out.extend([0xff, marker]);
        out.extend(u16::try_from(payload.len() + 2).unwrap_or(0).to_be_bytes());
        out.extend(payload);
    };
    let mut dqt = vec![0u8];
    dqt.extend([1u8; 64]);
    segment(&mut out, 0xdb, &dqt);
    let mut sof = vec![8];
    sof.extend(height.to_be_bytes());
    sof.extend(width.to_be_bytes());
    sof.extend([3, 1, 0x22, 0, 2, 0x11, 0, 3, 0x11, 0]);
    segment(&mut out, 0xc0, &sof);
    segment(&mut out, 0xda, &[3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]);
    let mcus = usize::from(width.div_ceil(16)) * usize::from(height.div_ceil(16));
    for _ in 0..mcus {
        // Luma: DC "00", AC EOB "1010", four times; chroma: DC "00", AC EOB
        // "00", twice.
        out.extend([0x28, 0xa2, 0x8a, 0x00]);
    }
    out.extend([0xff, 0xd9]);
    out
}

/// A [`JPEG_SIZE`] decode into `format` at 1/`scale`: at full size into
/// `420f`, the decode writes the output itself; scaled or into `BGRA`, a
/// processing run does (`crate::va::jpeg`).
pub(super) struct Jpeg {
    decoder: VaapiJpegDecoder,
    data: Vec<u8>,
    format: JpegOutputFormat,
    scale: u32,
    luma: Vec<u8>,
    chroma: Vec<u8>,
}

impl Jpeg {
    pub(super) fn new(display: &Display, format: JpegOutputFormat, scale: u32) -> Self {
        let (width, height) = JPEG_SIZE;
        let pixels = width as usize * height as usize;
        Self {
            decoder: VaapiJpegDecoder::new(display.clone_ref()).expect("a JPEG decoder"),
            data: grey_jpeg(
                u16::try_from(width).expect("a JPEG's width"),
                u16::try_from(height).expect("a JPEG's height"),
            ),
            format,
            scale,
            luma: vec![0; pixels * 4],
            chroma: vec![0; pixels / 2],
        }
    }

    /// Has the next decodes made at 1/`scale`.
    pub(super) fn scale_by(&mut self, scale: u32) {
        self.scale = scale;
    }

    pub(super) fn decode(&mut self) -> Result<(), JpegError> {
        let (width, height) = JPEG_SIZE;
        let request = JpegDecode {
            data: &self.data,
            width,
            height,
            subsampling: JpegSubsampling::Yuv420,
            region: PixelRect {
                x: 0,
                y: 0,
                width,
                height,
            },
            scale: self.scale,
            format: self.format,
        };
        let (out_width, out_height) = request.output_size();
        let row = out_width as usize;
        let rows = out_height as usize;
        let mut planes = match self.format {
            JpegOutputFormat::Bgra => vec![PlaneDestination {
                bytes: &mut self.luma,
                bytes_per_row: row * 4,
                rows: out_height,
            }],
            _ => vec![
                PlaneDestination {
                    bytes: &mut self.luma[..row * rows],
                    bytes_per_row: row,
                    rows: out_height,
                },
                PlaneDestination {
                    bytes: &mut self.chroma,
                    bytes_per_row: row,
                    rows: out_height.div_ceil(2),
                },
            ],
        };
        self.decoder.decode(&request, &mut planes)
    }
}

pub(super) fn malfunction(result: &Result<(), JpegError>) -> bool {
    matches!(result, Err(JpegError::Malfunction(_)))
}

/// A decode of `jpeg` once what its engine kept has left core's count:
/// asked again within 5 s, since core stops counting a work as it starts
/// to let it go (`device_wait`), and a decode made before the last of it
/// is dropped fails at once as stalled, of an earlier decode. The last
/// answer.
pub(super) fn decodes_again(display: &Display, jpeg: &mut Jpeg) -> Result<(), JpegError> {
    assert!(within_5_s(|| kept(display) == 0), "still kept");
    let mut last = Ok(());
    within_5_s(|| {
        last = jpeg.decode();
        last.is_ok()
    });
    last
}

/// A decode still running at its deadline fails as stalled; the frame's
/// pool fails each later wait, and each surface asked of it, at once, and
/// the pool, then the decoder's context, are kept until the GPU is done.
#[test]
#[ignore = "needs a VA-API GPU: run on the VA-API host"]
fn a_decode_still_running_at_its_deadline_is_kept_until_it_is_done() {
    let display = display();
    let (avcc, samples) = h264(&display, 1);
    for _ in 0..TRIES {
        let mut decoder = H264Decoder::new(&display, &avcc).expect("a decoder");
        begin(&mut decoder, &samples);
        let kept_before = KEPT_EVER.load(Ordering::SeqCst);
        let frames = decoded(&mut decoder, &samples);
        WAIT.set(Some(Duration::ZERO));
        let missed = frames[0].sync();
        WAIT.set(None);
        if missed.is_ok() {
            continue;
        }
        assert!(stalled(&missed), "{missed:?}");
        let start = StdRuntime.now();
        let again = frames[0].sync();
        let later = decoder.decode(&samples[0], 1);
        assert!(stalled(&again), "{again:?}");
        assert!(stalled(&later), "{:?}", later.map(|f| f.len()));
        assert!(StdRuntime.elapsed_since(start) < Duration::from_millis(5));
        drop(frames);
        drop(decoder);
        let ever = KEPT_EVER.load(Ordering::SeqCst);
        assert_eq!(ever, kept_before + 2, "the pool and the context");
        assert!(within_5_s(|| kept(&display) == 0), "still kept");
        return;
    }
    panic!("every decode was done before it was asked after, in {TRIES} tries");
}

/// A staged copy back's run still running at its deadline fails as
/// stalled, with no other way back tried, and the pool, with the staging
/// memory the run writes, is kept until the GPU is done.
#[test]
#[ignore = "needs a VA-API GPU: run on the VA-API host"]
fn a_copy_back_still_running_at_its_deadline_is_kept_until_it_is_done() {
    let display = display();
    let (avcc, samples) = h264(&display, 1);
    for _ in 0..TRIES {
        let mut decoder = H264Decoder::new(&display, &avcc).expect("a decoder");
        begin(&mut decoder, &samples);
        let kept_before = KEPT_EVER.load(Ordering::SeqCst);
        let frames = decoded(&mut decoder, &samples);
        // The surface's own wait: the frame's deadline, which its waits
        // share, is left to its read back.
        frames[0].surface.sync().expect("the decode");
        WAIT.set(Some(Duration::ZERO));
        let missed = frames[0].read_back().map(|_| ());
        WAIT.set(None);
        if missed.is_ok() {
            continue;
        }
        assert!(stalled(&missed), "{missed:?}");
        assert!(stalled(&frames[0].read_back().map(|_| ())));
        drop(frames);
        drop(decoder);
        let ever = KEPT_EVER.load(Ordering::SeqCst);
        assert_eq!(ever, kept_before + 2, "the pool and the context");
        assert!(within_5_s(|| kept(&display) == 0), "still kept");
        return;
    }
    panic!("every copy back was done before it was asked after, in {TRIES} tries");
}

/// A JPEG decode into host memory of ours (at full size into `420f`, its
/// output written by the decode itself) still running at its deadline
/// fails as a malfunction, and the surface it writes, with its context, is
/// kept until the GPU is done; then the engine decodes again.
#[test]
#[ignore = "needs a VA-API GPU: run on the VA-API host"]
fn a_jpeg_decode_still_running_at_its_deadline_is_kept_until_it_is_done() {
    let display = display();
    let mut jpeg = Jpeg::new(&display, JpegOutputFormat::Biplanar420, 1);
    jpeg.decode().expect("a decode, given the time");
    for _ in 0..TRIES {
        let kept_before = KEPT_EVER.load(Ordering::SeqCst);
        WAIT.set(Some(Duration::ZERO));
        let missed = jpeg.decode();
        WAIT.set(None);
        if missed.is_ok() {
            continue;
        }
        assert!(malfunction(&missed), "{missed:?}");
        let ever = KEPT_EVER.load(Ordering::SeqCst);
        assert_eq!(ever, kept_before + 1, "the surface and its context");
        decodes_again(&display, &mut jpeg).expect("a decode, the GPU done");
        return;
    }
    panic!("every JPEG decode was done before it was asked after, in {TRIES} tries");
}

/// An encode still running at its deadline fails its picture as stalled;
/// each later frame fails at once, and the session is kept until the GPU
/// is done with the picture's coded buffer. The frame's deadline starts at
/// its upload's map of the input surface, made on the display's call
/// thread ([`ENCODE_WAIT`]); a try whose map is not made by then is not
/// taken, and the next try is made.
#[test]
#[ignore = "needs a VA-API GPU: run on the VA-API host"]
fn an_encode_still_running_at_its_deadline_is_kept_until_it_is_done() {
    let display = display();
    let first = noise(0);
    let second = noise(1);
    for _ in 0..TRIES {
        let mut encoder = encoder(&display);
        let kept_before = KEPT_EVER.load(Ordering::SeqCst);
        WAIT.set(Some(ENCODE_WAIT));
        let coded = encoder.encode(&input(&first, 0));
        WAIT.set(None);
        let Ok(coded) = coded else { continue };
        let missed = match &coded[..] {
            [Ok(_)] => continue,
            [Err(failed)] => failed.error.as_ref(),
            other => panic!("{} results for one frame", other.len()),
        };
        assert!(matches!(missed, Some(Error::Stalled(_))), "{missed:?}");
        let start = StdRuntime.now();
        let later = encoder.encode(&input(&second, 1));
        assert!(stalled(&later), "{:?}", later.map(|c| c.len()));
        assert!(StdRuntime.elapsed_since(start) < Duration::from_millis(5));
        let ever = KEPT_EVER.load(Ordering::SeqCst);
        assert_eq!(ever, kept_before + 1, "the session");
        drop(encoder);
        assert!(within_5_s(|| kept(&display) == 0), "still kept");
        return;
    }
    panic!("every encode was done before it was asked after, in {TRIES} tries");
}
