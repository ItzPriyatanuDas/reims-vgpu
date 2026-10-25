//! Sessions whose backend call never returns: their end, alone or at a
//! reset, waits its bound and goes on, the stuck thread left running with
//! what it holds until the call returns ([`super::session_thread`]).
//!
//! The backend's parts open nothing while their latch is shut: each
//! session's start (`dsta`, `esta`, `mest`) calls `open` on the session's
//! thread, which then waits as long as the test keeps the latch shut, past
//! any bound. Each start lends one surface, which the stuck thread holds.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use super::session_thread::{DetachedThreads, TEARDOWN_WAIT};
use super::{GuestSink, HostSession, names};
use crate::backend::{
    Backend, BackendDecoder, BackendError, Codec, CodecCapabilities, DecoderBackend, DecoderConfig,
    PixelFormat, ProfileSupport,
};
use crate::encoder_backend::{BackendEncoder, EncoderBackend, EncoderCapabilities, EncoderConfig};
use crate::fourcc::FourCc;
use crate::host::codec_lists::MOTION_PROCESSOR_ID;
use crate::motion::{
    MotionEstimationBackend, MotionEstimationCapabilities, MotionEstimator, MotionEstimatorConfig,
};
use crate::runtime::{Runtime, StdRuntime};
use crate::status::{NO_ERROR, Status};
use crate::surfaces::{GuestSurface, MappingId, SurfaceError, SurfaceLayout, Surfaces};
use crate::sync::{Condvar, Mutex};
use crate::wire::WireError;
use crate::wire::image_description::{AVC_CONFIGURATION, ImageDescription};
use crate::wire::message::{
    GuestUuid, Message, MessageFlags, MessageWriter, ValueType, read_header,
};

/// The sessions' end's bound here.
const BOUND: Duration = Duration::from_millis(200);
/// How long past the bound an end may take, for a slow machine.
const SLACK: Duration = Duration::from_millis(1500);
/// How long a test waits for anything else.
const WAIT: Duration = Duration::from_secs(5);
/// How long the latch holds a call at most, so that a failing test ends.
const LATCH_MOST: Duration = Duration::from_secs(30);
const AVCC: &[u8] = &[1, 77, 0, 30, 0xff, 0xe0, 0];

/// Holds the backend's calls while shut.
#[derive(Default)]
struct Latch {
    open: Mutex<bool>,
    changed: Condvar,
    /// The calls waiting now.
    waiting: AtomicUsize,
}

impl Latch {
    fn hold(&self) {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let started = StdRuntime.now();
        let mut open = self.open.lock();
        while !*open && StdRuntime.elapsed_since(started) < LATCH_MOST {
            self.changed.wait_for(&mut open, Duration::from_millis(10));
        }
        self.waiting.fetch_sub(1, Ordering::SeqCst);
    }

    fn open(&self) {
        *self.open.lock() = true;
        self.changed.notify_all();
    }

    /// Waits until `count` calls are held.
    fn until_holding(&self, count: usize) {
        let started = StdRuntime.now();
        while self.waiting.load(Ordering::SeqCst) < count {
            assert!(
                StdRuntime.elapsed_since(started) < WAIT,
                "the backend was not called"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Every part's `open` waits on the latch, then refuses.
#[derive(Default)]
struct Stuck {
    latch: Latch,
}

fn released() -> BackendError {
    BackendError::Unsupported("the latch let it go".into())
}

impl DecoderBackend for Stuck {
    fn capabilities(&self) -> Vec<CodecCapabilities> {
        vec![CodecCapabilities {
            codec: Codec::H264,
            profiles: vec![ProfileSupport {
                profile: 77,
                max_level: None,
            }],
            max_width: 4096,
            max_height: 4096,
        }]
    }

    fn open(&self, _: &DecoderConfig) -> Result<Box<dyn BackendDecoder>, BackendError> {
        self.latch.hold();
        Err(released())
    }
}

impl EncoderBackend for Stuck {
    fn encoders(&self) -> Vec<EncoderCapabilities> {
        vec![EncoderCapabilities {
            codec: Codec::H264,
            profiles: vec![77],
            max_width: 4096,
            max_height: 4096,
            source_formats: vec![PixelFormat::Nv12VideoRange],
            frame_reordering: false,
        }]
    }

    fn open(&self, _: &EncoderConfig) -> Result<Box<dyn BackendEncoder>, BackendError> {
        self.latch.hold();
        Err(released())
    }
}

impl MotionEstimationBackend for Stuck {
    fn capabilities(&self) -> MotionEstimationCapabilities {
        MotionEstimationCapabilities {
            source_formats: vec![PixelFormat::Nv12VideoRange],
            max_width: 4096,
            max_height: 4096,
        }
    }

    fn open(&self, _: &MotionEstimatorConfig) -> Result<Box<dyn MotionEstimator>, BackendError> {
        self.latch.hold();
        Err(released())
    }
}

/// The guest's surfaces: any ID is one, and the host's holds are counted.
#[derive(Default)]
struct Counted {
    held: Arc<Mutex<HashSet<u64>>>,
}

struct Hold {
    mapping_id: u64,
    held: Arc<Mutex<HashSet<u64>>>,
}

impl GuestSurface for Hold {
    fn layout(&self) -> SurfaceLayout {
        SurfaceLayout {
            pixel_format: FourCc::new(*b"420v"),
            width: 16,
            height: 16,
            planes: Vec::new(),
            bytes_per_row: 16,
        }
    }

    fn write(&self, _: &mut dyn FnMut(&mut [u8])) -> Result<(), SurfaceError> {
        Err(SurfaceError::Unreachable("not written here".into()))
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.held.lock().remove(&self.mapping_id);
    }
}

impl Surfaces for Counted {
    fn retain(&self, mapping_id: MappingId) -> Result<Box<dyn GuestSurface>, SurfaceError> {
        self.held.lock().insert(mapping_id.0);
        Ok(Box::new(Hold {
            mapping_id: mapping_id.0,
            held: self.held.clone(),
        }))
    }
}

impl Counted {
    fn holds(&self, mapping_id: u64) -> bool {
        self.held.lock().contains(&mapping_id)
    }
}

struct ChannelSink(Mutex<Sender<Vec<u8>>>);

impl GuestSink for ChannelSink {
    fn send(&self, message: Vec<u8>) {
        let _ = self.0.lock().send(message);
    }
}

/// The three kinds of session, and the messages that open, start and
/// close each.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Decoder,
    Encoder,
    Motion,
}

const ALL: [Kind; 3] = [Kind::Decoder, Kind::Encoder, Kind::Motion];

impl Kind {
    fn create(self, uuid: GuestUuid) -> Vec<u8> {
        let (name, flags) = match self {
            Self::Decoder => (b"+dec", MessageFlags::GUEST_EXPECTS_REPLY),
            Self::Encoder => (b"+enc", MessageFlags::GUEST_EXPECTS_REPLY),
            Self::Motion => (b"+mep", MessageFlags::GUEST_EXPECTS_REPLY),
        };
        let mut message = MessageWriter::new(FourCc::new(*name), flags, Some(uuid), 1);
        match self {
            Self::Decoder | Self::Encoder => {
                message.s32(names::CODEC, i32::from_be_bytes(*b"avc1"));
                message.u32(names::VERSION, 6);
            }
            Self::Motion => {
                message.u32(names::VERSION, 6);
                let specification = plist::Dictionary::from_iter([(
                    "HostProcessorID".to_owned(),
                    plist::Value::String(MOTION_PROCESSOR_ID.to_owned()),
                )]);
                let _ = message.property_list(
                    names::MOTION_OPTIONS,
                    Some(&plist::Value::Dictionary(specification)),
                );
            }
        }
        let _ = message.property_list(names::GUEST_INFO, None);
        message.finish()
    }

    /// The start, which opens the backend, lending `mapping_id`.
    fn start(self, uuid: GuestUuid, mapping_id: u64) -> Vec<u8> {
        let name = match self {
            Self::Decoder => b"dsta",
            Self::Encoder => b"esta",
            Self::Motion => b"mest",
        };
        let mut message = MessageWriter::new(
            FourCc::new(*name),
            MessageFlags::GUEST_EXPECTS_REPLY,
            Some(uuid),
            2,
        );
        match self {
            Self::Decoder => {
                let description = ImageDescription {
                    codec: FourCc::new(*b"avc1"),
                    width: 64,
                    height: 48,
                    depth: 24,
                    compressor_name: Vec::new(),
                    extensions: vec![(AVC_CONFIGURATION, AVCC.to_vec())],
                }
                .to_bytes();
                message.image_description(names::FORMAT_DESCRIPTION, &description);
                message.s32(names::GUEST_POOL, 0);
            }
            Self::Encoder | Self::Motion => {
                message.i32_pair(names::DIMENSIONS, ValueType::Dimensions, (64, 48));
            }
        }
        with_surface(mapping_id, &message.finish())
    }

    fn close(self, uuid: GuestUuid) -> Vec<u8> {
        let name = match self {
            Self::Decoder => b"-dec",
            Self::Encoder => b"-enc",
            Self::Motion => b"-mep",
        };
        MessageWriter::new(FourCc::new(*name), MessageFlags::NONE, Some(uuid), 0).finish()
    }

    /// The `,err` a create refused for now carries.
    fn not_available_now(self) -> i32 {
        match self {
            Self::Decoder => Status::VideoDecoderNotAvailableNow.code(),
            Self::Encoder => Status::VideoEncoderNotAvailableNow.code(),
            Self::Motion => Status::MotionEstimationProcessorNotFound.code(),
        }
    }
}

/// The guest kext's `subs` box for one surface, then the message.
fn with_surface(mapping_id: u64, message: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0, 0, 0, 0x30];
    bytes.extend_from_slice(b"subs");
    bytes.extend_from_slice(&mapping_id.to_be_bytes());
    bytes.extend_from_slice(&[0; 32]);
    bytes.extend_from_slice(message);
    bytes
}

/// A host session on the stuck backend, its ends bounded at [`BOUND`],
/// keeping `most` detached threads of its own count.
struct Host {
    session: HostSession,
    backend: Arc<Stuck>,
    surfaces: Arc<Counted>,
    detached: Arc<DetachedThreads>,
    sent: Receiver<Vec<u8>>,
}

impl Host {
    fn new(most: usize) -> Self {
        let backend = Arc::new(Stuck::default());
        let surfaces = Arc::new(Counted::default());
        let (to_guest, sent) = mpsc::channel();
        let parts = Backend {
            decoders: Some(backend.clone()),
            encoders: Some(backend.clone()),
            motion: Some(backend.clone()),
            ..Backend::none("stuck")
        };
        let detached = Arc::new(DetachedThreads::new(most));
        let session = HostSession::with_backend(
            StdRuntime::shared(),
            Arc::new(ChannelSink(Mutex::new(to_guest))),
            surfaces.clone(),
            &parts,
        )
        .with_teardown(BOUND, detached.clone());
        Self {
            session,
            backend,
            surfaces,
            detached,
            sent,
        }
    }

    /// Creates a session of `kind`: its reply's `,err`. What the host
    /// sent before the reply, a `rels` or an earlier session's reply, is
    /// passed over.
    fn create(&self, kind: Kind, uuid: GuestUuid) -> Result<Option<i32>, WireError> {
        let create = kind.create(uuid);
        let asked = read_header(&create, 0)?;
        assert_eq!(self.session.deliver(&create), NO_ERROR);
        loop {
            let sent = self
                .sent
                .recv_timeout(WAIT)
                .map_err(|_| WireError::Malformed("no create reply"))?;
            let Ok(header) = read_header(&sent, 0) else {
                continue;
            };
            if header.message_type == asked.message_type && header.guest_uuid == uuid {
                return Message::parse(&sent)?.s32(names::ERROR);
            }
        }
    }

    /// Creates a session of `kind` and starts it, lending `mapping_id`,
    /// and waits until its backend call is held, with `held` held in all.
    fn stuck(&self, kind: Kind, uuid: GuestUuid, mapping_id: u64, held: usize) {
        assert_eq!(self.create(kind, uuid), Ok(Some(NO_ERROR)), "{kind:?}");
        assert_eq!(
            self.session.deliver(&kind.start(uuid, mapping_id)),
            NO_ERROR
        );
        self.backend.latch.until_holding(held);
        assert!(self.surfaces.holds(mapping_id));
    }

    /// Lets the stuck calls go, and waits until every detached thread has
    /// ended.
    fn release(&self) {
        self.backend.latch.open();
        let started = StdRuntime.now();
        while self.detached.count() > 0 {
            assert!(
                StdRuntime.elapsed_since(started) < WAIT,
                "a detached thread did not end"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.backend.latch.open();
    }
}

fn uuid(n: u8) -> GuestUuid {
    GuestUuid([n; 16])
}

/// The guest's own longest wait.
#[test]
fn the_bound_is_the_guests_longest_wait() {
    assert_eq!(TEARDOWN_WAIT, Duration::from_secs(10));
}

/// Closing a session whose backend never returns waits the bound, and
/// returns: the thread is left running with the surface its message lent,
/// later messages for the UUID find no session, and the thread ends with
/// its hold once the call returns. While it runs, the process's bound of
/// detached threads, here 1, refuses every new session for now.
fn closing_a_stuck_session_waits_its_bound(kind: Kind) -> Result<(), WireError> {
    let host = Host::new(1);
    host.stuck(kind, uuid(1), 7, 1);
    let closing = StdRuntime.now();
    assert_eq!(host.session.deliver(&kind.close(uuid(1))), NO_ERROR);
    let took = StdRuntime.elapsed_since(closing);
    assert!(took >= BOUND && took < BOUND + SLACK, "{kind:?}: {took:?}");
    assert_eq!(host.detached.count(), 1);
    assert!(host.surfaces.holds(7), "the stuck thread's hold");
    assert_eq!(
        host.session.deliver(&kind.start(uuid(1), 8)),
        Status::SessionNotFound.code()
    );
    for other in ALL {
        assert_eq!(
            host.create(other, uuid(2))?,
            Some(other.not_available_now()),
            "{other:?} past the bound"
        );
    }
    host.release();
    assert!(!host.surfaces.holds(7));
    assert_eq!(host.create(kind, uuid(3))?, Some(NO_ERROR));
    Ok(())
}

#[test]
fn closing_a_stuck_decoder_waits_its_bound() -> Result<(), WireError> {
    closing_a_stuck_session_waits_its_bound(Kind::Decoder)
}

#[test]
fn closing_a_stuck_encoder_waits_its_bound() -> Result<(), WireError> {
    closing_a_stuck_session_waits_its_bound(Kind::Encoder)
}

#[test]
fn closing_a_stuck_motion_session_waits_its_bound() -> Result<(), WireError> {
    closing_a_stuck_session_waits_its_bound(Kind::Motion)
}

/// A reset with a stuck session of each kind waits one bound for all of
/// them, not one each. Between its phases a stuck session's messages are
/// refused, -19098, and lend nothing; the second phase does not wait.
#[test]
fn a_reset_waits_one_bound_for_every_stuck_session() -> Result<(), WireError> {
    let host = Host::new(3);
    for (n, kind) in (1..).zip(ALL) {
        host.stuck(kind, uuid(n), u64::from(n), usize::from(n));
    }
    let invalidating = StdRuntime.now();
    host.session.invalidate();
    let took = StdRuntime.elapsed_since(invalidating);
    assert!(took >= BOUND && took < BOUND + SLACK, "{took:?}");
    assert_eq!(host.detached.count(), 3);
    for (n, kind) in (1..).zip(ALL) {
        assert_eq!(
            host.session.deliver(&kind.start(uuid(n), 9)),
            Status::SessionInvalidated.code(),
            "{kind:?}"
        );
    }
    assert!(!host.surfaces.holds(9));
    let completing = StdRuntime.now();
    host.session.complete_invalidate();
    assert!(
        StdRuntime.elapsed_since(completing) < BOUND,
        "{:?}",
        StdRuntime.elapsed_since(completing)
    );
    assert!((1..=3).all(|n| host.surfaces.holds(n)));
    host.release();
    assert!((1..=3).all(|n| !host.surfaces.holds(n)));
    Ok(())
}

/// A session whose thread answers in time is not left running.
#[test]
fn a_session_that_answers_is_not_detached() -> Result<(), WireError> {
    let host = Host::new(1);
    host.backend.latch.open();
    for (n, kind) in (1..).zip(ALL) {
        assert_eq!(host.create(kind, uuid(n))?, Some(NO_ERROR));
        assert_eq!(host.session.deliver(&kind.start(uuid(n), 1)), NO_ERROR);
        let closing = StdRuntime.now();
        assert_eq!(host.session.deliver(&kind.close(uuid(n))), NO_ERROR);
        assert!(StdRuntime.elapsed_since(closing) < BOUND, "{kind:?}");
    }
    assert_eq!(host.detached.count(), 0);
    Ok(())
}
