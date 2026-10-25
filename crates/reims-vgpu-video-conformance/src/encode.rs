//! The encode stand-in: raw frames encoded the way a guest's VideoToolbox
//! would encode them through the host session, on any backend's encoders,
//! and what the guest got back: the samples, their times and flags, and
//! each configuration record the host sent in `Efds`.
//! [`crate::encode_check`] checks them; [`run_args`] is the `encode`
//! command of the suite's command line ([`crate::cli`]), which writes them
//! out:
//!
//! ```sh
//! conformance DEVICE encode INPUT WxH PREFIX [--codec avc1|hvc1]
//!     [--format 420v|420f|x420|xf20] [--fps N] [--frames N] [--timescale T]
//!     [--set KEY=VALUE]... [--set-at FRAME:KEY=VALUE]... [--force FRAME]...
//!     [--before-start]
//! ```
//!
//! INPUT is raw frames, NV12 for `420v` and `420f`, P010 for `x420` and
//! `xf20`. A value is a boolean (`true`, `false`), a number, numbers
//! separated by commas (an array, as `DataRateLimits` takes), or a string.
//!
//! A stand-in for the guest's encoder drives a `HostSession` as core's
//! encoder tests do: `+enc` with no encoder ID, `esta` (the properties go
//! after it, as a client's set after VTCompressionSessionCreate do, or
//! before it with `--before-start`), one `efrm` per frame with its surface,
//! `ecfr`, `einv` and `-enc`. A second thread plays the guest's handler: it
//! answers `Epba`, makes the format description `Efds` asks for, gathers
//! each `Eemt`, and lets each surface go at its `rels`. A third delivers
//! queue 0 in order, as the device does.
//!
//! The command line writes PREFIX.mp4 (with the first record), PREFIX.264
//! or PREFIX.265 (Annex B, each record's parameter sets before the samples
//! it covers), PREFIX.log (one line per sample), and a summary to stderr,
//! with core's and the backend's log from the level `RVGV_LOG` names up.

pub use stand_in::{
    EmittedSample, EncodeRun, FrameReply, FrameSource, Options, SetAnswer, attachment,
    encode_frames, parameter_sets, run_args,
};

mod stand_in {
    use std::collections::HashMap;
    use std::fs::File;
    use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Sender};
    use std::time::Duration;

    use reims_vgpu_video_codec::encode::nal::length_prefixed_units;
    use reims_vgpu_video_codec::h264::avcc::DecoderConfiguration;
    use reims_vgpu_video_core::backend::Backend;
    use reims_vgpu_video_core::fourcc::FourCc;
    use reims_vgpu_video_core::host::{GuestSink, HostSession};
    use reims_vgpu_video_core::plist;
    use reims_vgpu_video_core::runtime::{Runtime, StdRuntime};
    use reims_vgpu_video_core::surfaces::{
        GuestSurface, MappingId, PlaneLayout, SurfaceError, SurfaceLayout, Surfaces,
    };
    use reims_vgpu_video_core::sync::Mutex;
    use reims_vgpu_video_core::wire::image_description::{
        AVC_CONFIGURATION, HEVC_CONFIGURATION, ImageDescription,
    };
    use reims_vgpu_video_core::wire::message::{
        GuestUuid, MediaTime, Message, MessageFlags, MessageHeader, MessageWriter, ValueType,
        read_header,
    };
    use reims_vgpu_video_core::wire::sample_buffer::{SampleBuffer, SerializedValue};
    use reims_vgpu_video_core::wire::surface_boxes::read_surface_release;

    use crate::mp4_write as mp4;

    const fn key(name: &[u8; 4]) -> FourCc {
        FourCc::new(*name)
    }
    const CODEC: FourCc = key(b",cod");
    const VERSION: FourCc = key(b",ver");
    const ERROR: FourCc = key(b",err");
    const GUEST_INFO: FourCc = key(b",gin");
    const DIMENSIONS: FourCc = key(b",vdi");
    const FRAME: FourCc = key(b",frm");
    const PRESENTATION_TIME: FourCc = key(b",pts");
    const DURATION: FourCc = key(b",dur");
    const FRAME_PROPERTIES: FourCc = key(b",fpr");
    const UUID: FourCc = key(b",uui");
    const ATTRIBUTES: FourCc = key(b",pba");
    const INFO: FourCc = key(b",inf");
    const ENCODE_INFO: FourCc = key(b",efl");
    const PROPERTY_KEY: FourCc = key(b",key");
    const PROPERTY_VALUE: FourCc = key(b",val");
    const SAMPLE: FourCc = key(b",sbf");
    const WIDTH: FourCc = key(b",wdt");
    const HEIGHT: FourCc = key(b",hgt");
    const EXTENSIONS: FourCc = key(b",ext");
    const CREATED: FourCc = key(b"=fds");
    /// The guest encoder's timeouts: 10 s for `efrm` and `ecfr`, 1.1 s for
    /// the rest.
    const FRAME_WAIT: Duration = Duration::from_secs(10);
    const WAIT: Duration = Duration::from_millis(1100);
    /// Padding past each 64-byte-aligned row of the guest's buffers: a read
    /// at the wrong stride shows.
    const ROW_PADDING: usize = 128;

    /// What to encode and how: the command line's options (module doc).
    #[derive(Clone, Debug)]
    pub struct Options {
        /// The device the backend opens on.
        pub node: String,
        /// The raw frames' file.
        pub input: String,
        /// The frames' width.
        pub width: u32,
        /// The frames' height.
        pub height: u32,
        /// Where the outputs go, before their extensions.
        pub prefix: String,
        /// `avc1` or `hvc1`.
        pub codec: [u8; 4],
        /// The frames' pixel format: `420v`, `420f`, `x420` or `xf20`.
        pub format: [u8; 4],
        /// Frames a second.
        pub fps: u32,
        /// At most this many frames.
        pub frames: Option<u32>,
        /// The time stamps' ticks a second.
        pub timescale: i32,
        /// Properties set before the first frame.
        pub set: Vec<(String, plist::Value)>,
        /// Properties set before a frame.
        pub set_at: Vec<(u32, String, plist::Value)>,
        /// Frames sent with `ForceKeyFrame`.
        pub force: Vec<u32>,
        /// Whether `set` goes before `esta` rather than after it.
        pub before_start: bool,
    }

    impl Options {
        /// `width` by `height` frames of `codec` from `format`, at 30 a
        /// second on a 600 Hz clock, with nothing set.
        pub fn new(codec: [u8; 4], format: [u8; 4], width: u32, height: u32) -> Self {
            Options {
                node: String::new(),
                input: String::new(),
                width,
                height,
                prefix: String::new(),
                codec,
                format,
                fps: 30,
                frames: None,
                timescale: 600,
                set: Vec::new(),
                set_at: Vec::new(),
                force: Vec::new(),
                before_start: false,
            }
        }

        /// The bytes of one sample: 2 for `x420` and `xf20`, else 1.
        pub fn sample_bytes(&self) -> usize {
            if matches!(&self.format, b"x420" | b"xf20") {
                2
            } else {
                1
            }
        }

        /// The bytes of one raw frame: a luma plane and an interleaved
        /// chroma plane half as tall, whose rows hold a Cb/Cr pair for
        /// every two luma samples, the last one too when the width is odd
        /// (ffmpeg's `nv12` and `p010le`).
        pub fn frame_size(&self) -> usize {
            let (width, height) = (
                usize::try_from(self.width).unwrap_or(0),
                usize::try_from(self.height).unwrap_or(0),
            );
            (width * height + width.div_ceil(2) * 2 * height.div_ceil(2)) * self.sample_bytes()
        }

        /// The tick count a frame lasts.
        pub fn tick(&self) -> i64 {
            i64::from(self.timescale) / i64::from(self.fps.max(1))
        }
    }

    fn value_of(text: &str) -> plist::Value {
        match text {
            "true" => return plist::Value::Boolean(true),
            "false" => return plist::Value::Boolean(false),
            _ => {}
        }
        if let Ok(number) = text.parse::<i64>() {
            return plist::Value::Integer(number.into());
        }
        if let Ok(number) = text.parse::<f64>() {
            return plist::Value::Real(number);
        }
        if text.contains(',') {
            let numbers: Option<Vec<plist::Value>> = text
                .split(',')
                .map(|part| match value_of(part) {
                    value @ (plist::Value::Integer(_) | plist::Value::Real(_)) => Some(value),
                    _ => None,
                })
                .collect();
            if let Some(numbers) = numbers {
                return plist::Value::Array(numbers);
            }
        }
        plist::Value::String(text.to_owned())
    }

    fn property(text: &str) -> Result<(String, plist::Value), String> {
        let (key, value) = text
            .split_once('=')
            .ok_or_else(|| format!("{text}: not KEY=VALUE"))?;
        Ok((key.to_owned(), value_of(value)))
    }

    fn options(args: &[String]) -> Result<Options, String> {
        let mut args = args.iter().cloned();
        let mut next = |what: &str| args.next().ok_or_else(|| format!("missing {what}"));
        let node = next("NODE")?;
        let input = next("INPUT")?;
        let size = next("WxH")?;
        let prefix = next("PREFIX")?;
        let (width, height) = size
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
            .ok_or_else(|| format!("{size}: not WxH"))?;
        let mut options = Options {
            node,
            input,
            width,
            height,
            prefix,
            codec: *b"avc1",
            format: *b"420v",
            fps: 30,
            frames: None,
            timescale: 600,
            set: Vec::new(),
            set_at: Vec::new(),
            force: Vec::new(),
            before_start: false,
        };
        let four = |text: String| -> Result<[u8; 4], String> {
            <[u8; 4]>::try_from(text.as_bytes()).map_err(|_| format!("{text}: not four bytes"))
        };
        let number = |text: String| text.parse::<u32>().map_err(|e| format!("{text}: {e}"));
        while let Ok(flag) = next("an option") {
            match flag.as_str() {
                "--codec" => options.codec = four(next("--codec")?)?,
                "--format" => options.format = four(next("--format")?)?,
                "--fps" => options.fps = number(next("--fps")?)?,
                "--frames" => options.frames = Some(number(next("--frames")?)?),
                "--timescale" => {
                    options.timescale =
                        i32::try_from(number(next("--timescale")?)?).map_err(|e| e.to_string())?;
                }
                "--set" => options.set.push(property(&next("--set")?)?),
                "--set-at" => {
                    let text = next("--set-at")?;
                    let (frame, rest) = text
                        .split_once(':')
                        .ok_or_else(|| format!("{text}: not FRAME:KEY=VALUE"))?;
                    let (key, value) = property(rest)?;
                    options.set_at.push((number(frame.to_owned())?, key, value));
                }
                "--force" => options.force.push(number(next("--force")?)?),
                "--before-start" => options.before_start = true,
                other => return Err(format!("unknown option {other}")),
            }
        }
        Ok(options)
    }

    /// The guest's IOSurfaces by mapping ID, and the host's holds on them.
    #[derive(Default)]
    struct GuestMemory {
        surfaces: Mutex<HashMap<u64, Arc<Buffer>>>,
        holds: Arc<AtomicUsize>,
        most_held: AtomicUsize,
    }

    struct Buffer {
        layout: SurfaceLayout,
        bytes: Vec<u8>,
    }

    struct Held {
        buffer: Arc<Buffer>,
        holds: Arc<AtomicUsize>,
    }

    impl GuestSurface for Held {
        fn layout(&self) -> SurfaceLayout {
            self.buffer.layout.clone()
        }

        fn write(&self, _: &mut dyn FnMut(&mut [u8])) -> Result<(), SurfaceError> {
            Err(SurfaceError::Unreachable("a source buffer".into()))
        }

        fn read(&self, read: &mut dyn FnMut(&[u8])) -> Result<(), SurfaceError> {
            read(&self.buffer.bytes);
            Ok(())
        }
    }

    impl Drop for Held {
        fn drop(&mut self) {
            self.holds.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl Surfaces for GuestMemory {
        fn retain(&self, mapping_id: MappingId) -> Result<Box<dyn GuestSurface>, SurfaceError> {
            let buffer = self
                .surfaces
                .lock()
                .get(&mapping_id.0)
                .cloned()
                .ok_or(SurfaceError::Unknown(mapping_id.0))?;
            let held = self.holds.fetch_add(1, Ordering::SeqCst) + 1;
            self.most_held.fetch_max(held, Ordering::SeqCst);
            Ok(Box::new(Held {
                buffer,
                holds: self.holds.clone(),
            }))
        }
    }

    struct ChannelSink(Mutex<Sender<Vec<u8>>>);

    impl GuestSink for ChannelSink {
        fn send(&self, message: Vec<u8>) {
            let _ = self.0.lock().send(message);
        }
    }

    /// Fills frame `index` in: a raw frame of the options' format.
    pub type FrameSource<'a> = dyn FnMut(u64, &mut [u8]) -> Result<(), String> + 'a;

    /// One sample as the guest's encoder got it.
    #[derive(Clone, Debug)]
    pub struct EmittedSample {
        /// The frame ID of the frame it encodes.
        pub frame_id: i64,
        /// `Eemt`'s status.
        pub status: i32,
        /// `Eemt`'s encode info flags (`,efl`), where it had them.
        pub info_flags: Option<u32>,
        /// The sample buffer, where it parsed.
        pub sample: Option<SampleBuffer>,
        /// The record in force: an index into [`EncodeRun::records`].
        pub record: usize,
    }

    /// The answer to one `eset`.
    #[derive(Clone, Debug, PartialEq)]
    pub struct SetAnswer {
        /// The frame it was sent before, or `None` for one sent before the
        /// first frame.
        pub frame: Option<u32>,
        /// The property.
        pub key: String,
        /// Its `,err`.
        pub status: i32,
    }

    /// The reply to one `efrm`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct FrameReply {
        /// Its `,err`.
        pub status: i32,
        /// Its `,inf`, where it had one.
        pub info_flags: Option<u32>,
    }

    /// What one encode gave.
    #[derive(Debug)]
    pub struct EncodeRun {
        /// `esta`'s status. Where it is not 0, no frame was sent, and the
        /// session was ended at once.
        pub start_status: i32,
        /// Each `eset`'s answer, in the order they were sent.
        pub sets: Vec<SetAnswer>,
        /// Each `efrm`'s reply, by frame.
        pub replies: Vec<FrameReply>,
        /// The frames sent.
        pub frames: u64,
        /// The frames whose `efrm` failed.
        pub failed: u64,
        /// The samples, in the order they came.
        pub emitted: Vec<EmittedSample>,
        /// Each configuration record `Efds` gave, in order.
        pub records: Vec<Vec<u8>>,
        /// From the first `efrm` to `ecfr`'s reply, in seconds.
        pub elapsed: f64,
        /// The most surfaces the host held at once.
        pub most_held: usize,
        /// Surfaces the host's `rels` named.
        pub released: usize,
        /// The host's messages, by type.
        pub counts: HashMap<String, usize>,
        /// Each engine's busy time in this process after the frames
        /// ([`crate::engines`]), as `ENGINE=Nns` words.
        pub engines: String,
    }

    /// What the guest's handler keeps.
    #[derive(Default)]
    struct Guest {
        memory: Arc<GuestMemory>,
        waiting: Mutex<HashMap<u64, Sender<Vec<u8>>>>,
        records: Mutex<Vec<Vec<u8>>>,
        emitted: Mutex<Vec<EmittedSample>>,
        partial: Mutex<Vec<u8>>,
        released: AtomicUsize,
        counts: Mutex<HashMap<String, usize>>,
    }

    impl Guest {
        fn count(&self, what: &str) {
            *self.counts.lock().entry(what.to_owned()).or_default() += 1;
        }

        /// The guest's handler for one host message.
        fn handle(&self, message: &[u8], queue0: &Sender<Vec<u8>>) {
            if let Some((ids, _)) = read_surface_release(message) {
                self.count("rels");
                self.released.fetch_add(ids.len(), Ordering::SeqCst);
                for id in ids {
                    self.memory.surfaces.lock().remove(&id.0);
                }
                return;
            }
            let Ok(header) = read_header(message, 0) else {
                self.count("unreadable");
                return;
            };
            if header.flags.contains(MessageFlags::REPLY_TO_GUEST) {
                if let Some(waiter) = self.waiting.lock().remove(&header.reply_id) {
                    let _ = waiter.send(message.to_vec());
                }
                return;
            }
            let kind = header.message_type.to_string();
            if kind == "Eemt" {
                self.partial.lock().extend_from_slice(message);
                if header.flags.contains(MessageFlags::MORE_FRAGMENTS) {
                    return;
                }
                self.count("Eemt");
                let whole = std::mem::take(&mut *self.partial.lock());
                self.emitted_frame(&whole);
                answer(queue0, &header, |_| {});
                return;
            }
            self.count(&kind);
            let Ok(parsed) = Message::parse(message) else {
                eprintln!("{kind}: unparsable");
                return;
            };
            match &header.message_type.to_be_bytes() {
                b"Epba" => {
                    let attributes = parsed.dictionary(ATTRIBUTES).ok().flatten();
                    eprintln!("Epba (flags {:#x}): {attributes:?}", header.flags.0);
                    if header.flags.contains(MessageFlags::HOST_EXPECTS_REPLY) {
                        answer(queue0, &header, |_| {});
                    }
                }
                b"Efds" => self.describe(&parsed, &header, queue0),
                _ => eprintln!("{kind}: not handled"),
            }
        }

        /// `Efds`: keeps the record, and answers with the format
        /// description as an ImageDescription.
        fn describe(&self, parsed: &Message<'_>, header: &MessageHeader, queue0: &Sender<Vec<u8>>) {
            let read = |key| parsed.s32(key).ok().flatten().unwrap_or(0);
            let (codec, width, height) = (read(CODEC), read(WIDTH), read(HEIGHT));
            let extensions = parsed
                .dictionary(EXTENSIONS)
                .ok()
                .flatten()
                .unwrap_or_default();
            let atom = if codec.to_be_bytes() == *b"hvc1" {
                HEVC_CONFIGURATION
            } else {
                AVC_CONFIGURATION
            };
            let record = extensions
                .get("SampleDescriptionExtensionAtoms")
                .and_then(plist::Value::as_dictionary)
                .and_then(|atoms| atoms.get(&atom.to_string()))
                .and_then(plist::Value::as_data)
                .map(<[u8]>::to_vec)
                .unwrap_or_default();
            eprintln!(
                "Efds: {}x{height}, {} record of {} bytes, extensions {:?}",
                width,
                atom,
                record.len(),
                extensions.keys().collect::<Vec<_>>()
            );
            let name = extensions
                .get("FormatName")
                .and_then(plist::Value::as_string)
                .unwrap_or_default()
                .as_bytes()
                .to_vec();
            self.records.lock().push(record.clone());
            let description = ImageDescription {
                codec: FourCc(codec.cast_unsigned()),
                width: u16::try_from(width).unwrap_or(0),
                height: u16::try_from(height).unwrap_or(0),
                depth: 24,
                compressor_name: name,
                extensions: vec![(atom, record)],
            }
            .to_bytes();
            answer(queue0, header, |reply| {
                reply.image_description(CREATED, &description);
            });
        }

        fn emitted_frame(&self, whole: &[u8]) {
            let Ok(parsed) = Message::parse(whole) else {
                eprintln!("Eemt: unparsable");
                return;
            };
            let sample = parsed
                .sample_buffer(SAMPLE)
                .ok()
                .flatten()
                .and_then(|serialization| SampleBuffer::parse(&serialization).ok());
            let record = self.records.lock().len().saturating_sub(1);
            self.emitted.lock().push(EmittedSample {
                frame_id: parsed.s64(FRAME).ok().flatten().unwrap_or(-1),
                status: parsed.s32(ERROR).ok().flatten().unwrap_or(-1),
                info_flags: parsed.u32(ENCODE_INFO).ok().flatten(),
                sample,
                record,
            });
        }
    }

    fn answer(
        queue0: &Sender<Vec<u8>>,
        request: &MessageHeader,
        extra: impl FnOnce(&mut MessageWriter),
    ) {
        let mut reply = MessageWriter::reply_to(request, MessageFlags::REPLY_TO_HOST);
        reply.s32(ERROR, 0);
        extra(&mut reply);
        let _ = queue0.send(reply.finish());
    }

    /// The guest encoder's side of the calls.
    struct Caller {
        guest: Arc<Guest>,
        queue0: Sender<Vec<u8>>,
        uuid: GuestUuid,
        next_reply_id: std::sync::atomic::AtomicU64,
    }

    impl Caller {
        fn message(
            &self,
            kind: &[u8; 4],
            parameters: impl FnOnce(&mut MessageWriter),
        ) -> (u64, Vec<u8>) {
            let reply_id = self.next_reply_id.fetch_add(1, Ordering::SeqCst);
            let mut message = MessageWriter::new(
                FourCc::new(*kind),
                MessageFlags::GUEST_EXPECTS_REPLY,
                Some(self.uuid),
                reply_id,
            );
            parameters(&mut message);
            (reply_id, message.finish())
        }

        fn call_with(
            &self,
            kind: &[u8; 4],
            reply_id: u64,
            message: Vec<u8>,
            timeout: Duration,
        ) -> Result<i32, String> {
            self.reply_with(kind, reply_id, message, timeout)
                .map(|reply| reply.status)
        }

        /// Sends `message` and waits for its reply's `,err` and `,inf`.
        fn reply_with(
            &self,
            kind: &[u8; 4],
            reply_id: u64,
            message: Vec<u8>,
            timeout: Duration,
        ) -> Result<FrameReply, String> {
            let (arrived, reply) = mpsc::channel();
            self.guest.waiting.lock().insert(reply_id, arrived);
            let _ = self.queue0.send(message);
            let reply = reply.recv_timeout(timeout);
            self.guest.waiting.lock().remove(&reply_id);
            let reply = reply.map_err(|_| format!("no {} reply", String::from_utf8_lossy(kind)))?;
            let parsed = Message::parse(&reply).map_err(|e| e.to_string())?;
            Ok(FrameReply {
                status: parsed.s32(ERROR).map_err(|e| e.to_string())?.unwrap_or(0),
                info_flags: parsed.u32(INFO).ok().flatten(),
            })
        }

        fn call(
            &self,
            kind: &[u8; 4],
            timeout: Duration,
            parameters: impl FnOnce(&mut MessageWriter),
        ) -> Result<i32, String> {
            let (reply_id, message) = self.message(kind, parameters);
            self.call_with(kind, reply_id, message, timeout)
        }

        /// `eset`: its `,err`.
        fn set(&self, key: &str, value: &plist::Value) -> Result<i32, String> {
            let status = self.call(b"eset", WAIT, |message| {
                let _ = message
                    .property_list(PROPERTY_KEY, Some(&plist::Value::String(key.to_owned())));
                let _ = message.property_list(PROPERTY_VALUE, Some(value));
            })?;
            eprintln!("eset {key} = {value:?}: {status}");
            Ok(status)
        }
    }

    /// A buffer as `Epba` asks: both planes `ROW_PADDING` past a
    /// 64-byte-aligned row, chroma page-aligned, filled from `frame`.
    fn buffer(options: &Options, sample_bytes: usize, frame: &[u8]) -> Buffer {
        let width = usize::try_from(options.width).unwrap_or(0);
        let height = usize::try_from(options.height).unwrap_or(0);
        let row = width * sample_bytes;
        // An odd width's last chroma pair makes a chroma row a sample longer.
        let chroma_row = width.div_ceil(2) * 2 * sample_bytes;
        let bytes_per_row = chroma_row.next_multiple_of(64) + ROW_PADDING;
        let chroma_offset = (bytes_per_row * height).next_multiple_of(4096);
        let chroma_rows = height.div_ceil(2);
        let mut bytes = vec![0; chroma_offset + bytes_per_row * chroma_rows];
        for y in 0..height {
            bytes[y * bytes_per_row..][..row].copy_from_slice(&frame[y * row..][..row]);
        }
        let chroma = &frame[row * height..];
        for y in 0..chroma_rows {
            bytes[chroma_offset + y * bytes_per_row..][..chroma_row]
                .copy_from_slice(&chroma[y * chroma_row..][..chroma_row]);
        }
        Buffer {
            layout: SurfaceLayout {
                pixel_format: FourCc::new(options.format),
                width: options.width,
                height: options.height,
                planes: vec![
                    PlaneLayout {
                        offset: 0,
                        bytes_per_row,
                        width: options.width,
                        height: options.height,
                    },
                    PlaneLayout {
                        offset: chroma_offset,
                        bytes_per_row,
                        width: options.width.div_ceil(2),
                        height: options.height.div_ceil(2),
                    },
                ],
                bytes_per_row: 0,
            },
            bytes,
        }
    }

    /// The parameter sets of an `avcC` or `hvcC` record, in order.
    pub fn parameter_sets(hevc: bool, record: &[u8]) -> Vec<Vec<u8>> {
        if !hevc {
            return DecoderConfiguration::parse(record)
                .map(|record| {
                    record
                        .sequence_parameter_sets
                        .iter()
                        .chain(&record.picture_parameter_sets)
                        .map(|unit| unit.to_vec())
                        .collect()
                })
                .unwrap_or_default();
        }
        // hvcC: 22 bytes, numOfArrays, then each array's type byte, NAL
        // count and length-prefixed units (ISO/IEC 14496-15, 8.3.3.1).
        let mut units = Vec::new();
        let Some(&arrays) = record.get(22) else {
            return units;
        };
        let mut at = 23;
        for _ in 0..arrays {
            let Some(count) = record.get(at + 1..at + 3) else {
                break;
            };
            let count = u16::from_be_bytes([count[0], count[1]]);
            at += 3;
            for _ in 0..count {
                let Some(length) = record.get(at..at + 2) else {
                    return units;
                };
                let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
                let Some(unit) = record.get(at + 2..at + 2 + length) else {
                    return units;
                };
                units.push(unit.to_vec());
                at += 2 + length;
            }
        }
        units
    }

    fn time(value: i64, timescale: i32) -> MediaTime {
        MediaTime {
            value,
            timescale,
            flags: MediaTime::FLAG_VALID,
            epoch: 0,
        }
    }

    /// A sample's boolean attachment `key` (`NotSync`, `DependsOnOthers`).
    pub fn attachment(sample: &SampleBuffer, key: &str) -> Option<bool> {
        let Some([SerializedValue::Dictionary(entries)]) = sample.sample_attachments.as_deref()
        else {
            return None;
        };
        entries
            .iter()
            .find(|(name, _)| name == key)
            .and_then(|(_, value)| match value {
                SerializedValue::PropertyList(plist::Value::Boolean(value)) => Some(*value),
                _ => None,
            })
    }

    /// The `encode` command's arguments (module doc), DEVICE first, on the
    /// backend `open` makes of DEVICE, as `program`.
    pub fn run_args(
        program: &str,
        args: &[String],
        open: crate::cli::Open<'_>,
    ) -> std::process::ExitCode {
        let run = || -> Result<(), String> {
            let options = options(args)?;
            let frame_size = options.frame_size();
            let mut input =
                File::open(&options.input).map_err(|e| format!("{}: {e}", options.input))?;
            let available = input.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?
                / u64::try_from(frame_size).unwrap_or(1);
            input.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
            let frames = options.frames.map_or(available, u64::from).min(available);
            let backend = open(&options.node).map_err(|e| format!("{}: {e}", options.node))?;
            let run = encode_frames(&backend, &options, frames, &mut |_, frame| {
                input.read_exact(frame).map_err(|e| e.to_string())
            })?;
            if run.start_status != 0 {
                return Err(format!("esta answered {}", run.start_status));
            }
            write_outputs(&options, &run)
        };
        match run() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{program}: {e}");
                std::process::ExitCode::FAILURE
            }
        }
    }

    /// Encodes `frames` frames of `options`, each filled by `source(index,
    /// frame)`, through a host session on `backend`'s encoders, and returns
    /// what the guest got. An error is a message the host refused or did
    /// not answer.
    pub fn encode_frames(
        backend: &Backend,
        options: &Options,
        frames: u64,
        source: &mut FrameSource<'_>,
    ) -> Result<EncodeRun, String> {
        let sample_bytes = options.sample_bytes();
        let frame_size = options.frame_size();
        let guest = Arc::new(Guest::default());
        let (to_guest, from_host) = mpsc::channel::<Vec<u8>>();
        let session = Arc::new(HostSession::with_backend(
            StdRuntime::shared(),
            Arc::new(ChannelSink(Mutex::new(to_guest))),
            guest.memory.clone(),
            backend,
        ));
        let (queue0, device_queue) = mpsc::channel::<Vec<u8>>();
        let device = {
            let session = session.clone();
            StdRuntime::thread("conformance-device", move || {
                while let Ok(message) = device_queue.recv() {
                    session.deliver(&message);
                }
            })
            .map_err(|e| e.to_string())?
        };
        let handler = {
            let guest = guest.clone();
            let queue0 = queue0.clone();
            StdRuntime::thread("conformance-guest", move || {
                while let Ok(message) = from_host.recv() {
                    guest.handle(&message, &queue0);
                }
            })
            .map_err(|e| e.to_string())?
        };
        let caller = Caller {
            guest: guest.clone(),
            queue0: queue0.clone(),
            uuid: GuestUuid([0x5e; 16]),
            next_reply_id: std::sync::atomic::AtomicU64::new(100),
        };

        let status = caller.call(b"+enc", WAIT, |message| {
            message.s32(CODEC, i32::from_be_bytes(options.codec));
            message.u32(VERSION, 6);
            let _ = message.property_list(GUEST_INFO, None);
        })?;
        eprintln!("+enc: {status}");
        if status != 0 {
            return Err(format!("+enc answered {status}"));
        }
        let mut sets = Vec::new();
        let mut set = |frame: Option<u32>, key: &str, value: &plist::Value| {
            let status = caller.set(key, value)?;
            sets.push(SetAnswer {
                frame,
                key: key.to_owned(),
                status,
            });
            Ok::<(), String>(())
        };
        if options.before_start {
            for (key, value) in &options.set {
                set(None, key, value)?;
            }
        }
        let size = (
            i32::try_from(options.width).unwrap_or(0),
            i32::try_from(options.height).unwrap_or(0),
        );
        let start_status = caller.call(b"esta", WAIT, |message| {
            message.i32_pair(DIMENSIONS, ValueType::Dimensions, size);
        })?;
        eprintln!("esta: {start_status}");
        // A refused start sends no frame: the session ends at once, as a
        // guest's create that fails ends it.
        let frames = if start_status == 0 { frames } else { 0 };
        if start_status == 0 && !options.before_start {
            for (key, value) in &options.set {
                set(None, key, value)?;
            }
        }

        let tick = options.tick();
        let mut frame = vec![0; frame_size];
        let mut failed = 0;
        let mut replies = Vec::new();
        let start = StdRuntime.now();
        for n in 0..frames {
            let index = u32::try_from(n).unwrap_or(u32::MAX);
            for (_, key, value) in options.set_at.iter().filter(|(at, ..)| *at == index) {
                set(Some(index), key, value)?;
            }
            source(n, &mut frame)?;
            let mapping_id = n + 1;
            guest
                .memory
                .surfaces
                .lock()
                .insert(mapping_id, Arc::new(buffer(options, sample_bytes, &frame)));
            let frame_id = n.cast_signed();
            let (reply_id, message) = caller.message(b"efrm", |message| {
                message.s64(FRAME, frame_id);
                message.time(PRESENTATION_TIME, time(frame_id * tick, options.timescale));
                message.time(DURATION, time(tick, options.timescale));
                if options.force.contains(&index) {
                    let _ = message.property_list(
                        FRAME_PROPERTIES,
                        Some(&plist::Value::Dictionary(
                            [(
                                "EncoderForceKeyframe".to_owned(),
                                plist::Value::Boolean(true),
                            )]
                            .into_iter()
                            .collect(),
                        )),
                    );
                }
                let mut uuid = [0xf0; 16];
                uuid[8..].copy_from_slice(&frame_id.to_be_bytes());
                message.uuid(UUID, GuestUuid(uuid));
            });
            let mut with_surface = vec![0, 0, 0, 0x30];
            with_surface.extend_from_slice(b"subs");
            with_surface.extend_from_slice(&mapping_id.to_be_bytes());
            with_surface.extend_from_slice(&[0; 32]);
            with_surface.extend_from_slice(&message);
            let reply = caller.reply_with(b"efrm", reply_id, with_surface, FRAME_WAIT)?;
            if reply.status != 0 {
                failed += 1;
                eprintln!("efrm {n}: {}", reply.status);
            }
            replies.push(reply);
        }
        if start_status == 0 {
            let status = caller.call(b"ecfr", FRAME_WAIT, |message| {
                message.time(PRESENTATION_TIME, MediaTime::INVALID);
            })?;
            eprintln!("ecfr: {status}");
        }
        let elapsed = StdRuntime.elapsed_since(start).as_secs_f64();
        let engines = (crate::engines::busy().into_iter())
            .map(|(engine, ns)| format!("{engine}={ns}ns"))
            .collect::<Vec<_>>()
            .join(" ");
        let status = caller.call(b"einv", WAIT, |_| {})?;
        eprintln!("einv: {status}");
        let _ = queue0.send(
            MessageWriter::new(
                FourCc::new(*b"-enc"),
                MessageFlags::NONE,
                Some(caller.uuid),
                0,
            )
            .finish(),
        );
        session.invalidate();
        session.complete_invalidate();
        drop(caller);
        drop(queue0);
        let _ = device.join();
        // The handler ends with the process: the session's sink may outlive
        // this function's handle on it.
        drop(handler);

        let emitted = std::mem::take(&mut *guest.emitted.lock());
        let records = std::mem::take(&mut *guest.records.lock());
        Ok(EncodeRun {
            start_status,
            sets,
            replies,
            frames,
            failed,
            emitted,
            records,
            elapsed,
            most_held: guest.memory.most_held.load(Ordering::SeqCst),
            released: guest.released.load(Ordering::SeqCst),
            counts: guest.counts.lock().clone(),
            engines,
        })
    }

    /// Writes PREFIX.log, PREFIX.264 or PREFIX.265, PREFIX.mp4 and
    /// PREFIX.record from `run`, and its summary to stderr; an error where
    /// frames went missing.
    fn write_outputs(options: &Options, run: &EncodeRun) -> Result<(), String> {
        let hevc = &options.codec == b"hvc1";
        let (emitted, records, frames, failed, elapsed) = (
            &run.emitted,
            &run.records,
            run.frames,
            run.failed,
            run.elapsed,
        );
        eprintln!("engines: {}", run.engines);
        let mut log = BufWriter::new(
            File::create(format!("{}.log", options.prefix)).map_err(|e| e.to_string())?,
        );
        let mut annex_b = BufWriter::new(
            File::create(format!(
                "{}.{}",
                options.prefix,
                if hevc { "265" } else { "264" }
            ))
            .map_err(|e| e.to_string())?,
        );
        let mut track = Vec::new();
        let mut written_record = None;
        let mut bytes = 0;
        for (index, emitted) in emitted.iter().enumerate() {
            let Some(sample) = emitted.sample.as_ref().filter(|_| emitted.status == 0) else {
                writeln!(
                    log,
                    "sample {index} frame {} status {}",
                    emitted.frame_id, emitted.status
                )
                .map_err(|e| e.to_string())?;
                continue;
            };
            let timing = sample.timings.first().copied();
            let pts = timing.map_or(0, |t| t.presentation.value);
            let dts = timing.map_or(0, |t| t.decode.value);
            let duration = timing.map_or(0, |t| t.duration.value);
            let data = sample.data.clone().unwrap_or_default();
            let sync = attachment(sample, "NotSync") != Some(true);
            let depends = attachment(sample, "DependsOnOthers").unwrap_or(false);
            writeln!(
                log,
                "sample {index} frame {} pts {pts} dts {dts} duration {duration} sync {} depends {} size {} record {} description {}",
                emitted.frame_id,
                u8::from(sync),
                u8::from(depends),
                data.len(),
                emitted.record,
                u8::from(sample.format_description.is_some()),
            )
            .map_err(|e| e.to_string())?;
            if written_record != Some(emitted.record) {
                if let Some(record) = records.get(emitted.record) {
                    for unit in parameter_sets(hevc, record) {
                        annex_b
                            .write_all(&[0, 0, 0, 1])
                            .map_err(|e| e.to_string())?;
                        annex_b.write_all(&unit).map_err(|e| e.to_string())?;
                    }
                }
                written_record = Some(emitted.record);
            }
            for unit in length_prefixed_units(&data)
                .ok_or_else(|| format!("sample {index}: lengths run past the end"))?
            {
                annex_b
                    .write_all(&[0, 0, 0, 1])
                    .map_err(|e| e.to_string())?;
                annex_b.write_all(unit).map_err(|e| e.to_string())?;
            }
            bytes += data.len();
            track.push(mp4::Sample {
                data,
                pts,
                dts,
                duration,
                sync,
            });
        }
        let first_record = records.first().cloned().unwrap_or_default();
        let (entry, record_box) = if hevc {
            (b"hvc1", b"hvcC")
        } else {
            (b"avc1", b"avcC")
        };
        // The stream's size, the frames' rounded down to even.
        let (width, height) = crate::encode_check::encoded_size(options.width, options.height);
        let file = mp4::write(
            entry,
            record_box,
            &first_record,
            width,
            height,
            options.timescale.cast_unsigned(),
            &track,
        );
        std::fs::write(format!("{}.mp4", options.prefix), file).map_err(|e| e.to_string())?;
        std::fs::write(format!("{}.record", options.prefix), &first_record)
            .map_err(|e| e.to_string())?;
        let seconds = frames as f64 / f64::from(options.fps.max(1));
        eprintln!(
            "sent {frames} frames ({failed} failed), got {} samples in {elapsed:.3} s ({:.1} fps): {bytes} bytes, {:.0} bit/s; {} records; most surfaces held {}, released {}",
            emitted.len(),
            frames as f64 / elapsed,
            bytes as f64 * 8.0 / seconds,
            records.len(),
            run.most_held,
            run.released,
        );
        eprintln!("messages: {:?}", run.counts);
        if failed > 0 || emitted.len() as u64 != frames {
            return Err("frames went missing".into());
        }
        Ok(())
    }
}
