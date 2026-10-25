//! The host session against hand-written guest messages, a sink that
//! records what the host sends, and surfaces that count their holds.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::*;
use crate::backend::{BackendDecoder, BackendError, CodecCapabilities, DecoderConfig, NoDecoders};
use crate::fourcc::FourCc;
use crate::runtime::StdRuntime;
use crate::surfaces::{GuestSurface, SurfaceError, SurfaceLayout};
use crate::sync::Mutex;
use crate::wire::message::{GuestUuid, MessageHeader, read_header};
use crate::wire::surface_boxes::read_surface_release;

#[derive(Default)]
pub(crate) struct RecordingSink {
    pub(crate) sent: Mutex<Vec<Vec<u8>>>,
}

impl GuestSink for RecordingSink {
    fn send(&self, message: Vec<u8>) {
        self.sent.lock().push(message);
    }
}

impl RecordingSink {
    pub(crate) fn take(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.sent.lock())
    }
}

/// What runs, once, as the host lets go of its hold on a surface, before
/// the hold has ended.
type OnRelease = Box<dyn FnOnce() + Send>;

/// What runs, once, inside a surface's retain, before it answers.
type OnRetain = Box<dyn FnOnce() + Send>;

/// Surfaces 1 to 99 exist; each counts the holds on it.
#[derive(Default)]
pub(crate) struct CountingSurfaces {
    pub(crate) holds: Arc<Mutex<HashMap<u64, usize>>>,
    pub(crate) retains: AtomicUsize,
    on_release: Arc<Mutex<HashMap<u64, OnRelease>>>,
    on_retain: Mutex<HashMap<u64, OnRetain>>,
}

struct CountedSurface {
    id: u64,
    holds: Arc<Mutex<HashMap<u64, usize>>>,
    on_release: Arc<Mutex<HashMap<u64, OnRelease>>>,
}

impl GuestSurface for CountedSurface {
    fn layout(&self) -> SurfaceLayout {
        SurfaceLayout {
            pixel_format: FourCc::new(*b"420v"),
            width: 4,
            height: 2,
            planes: Vec::new(),
            bytes_per_row: 4,
        }
    }

    fn write(&self, write: &mut dyn FnMut(&mut [u8])) -> Result<(), SurfaceError> {
        write(&mut [0; 12]);
        Ok(())
    }
}

impl Drop for CountedSurface {
    fn drop(&mut self) {
        let on_release = self.on_release.lock().remove(&self.id);
        if let Some(on_release) = on_release {
            on_release();
        }
        if let Some(count) = self.holds.lock().get_mut(&self.id) {
            *count -= 1;
        }
    }
}

impl Surfaces for CountingSurfaces {
    fn retain(&self, mapping_id: MappingId) -> Result<Box<dyn GuestSurface>, SurfaceError> {
        let on_retain = self.on_retain.lock().remove(&mapping_id.0);
        if let Some(on_retain) = on_retain {
            on_retain();
        }
        if !(1..100).contains(&mapping_id.0) {
            return Err(SurfaceError::Unknown(mapping_id.0));
        }
        self.retains.fetch_add(1, Ordering::SeqCst);
        *self.holds.lock().entry(mapping_id.0).or_default() += 1;
        Ok(Box::new(CountedSurface {
            id: mapping_id.0,
            holds: Arc::clone(&self.holds),
            on_release: Arc::clone(&self.on_release),
        }))
    }
}

impl CountingSurfaces {
    pub(crate) fn held(&self, id: u64) -> usize {
        self.holds.lock().get(&id).copied().unwrap_or(0)
    }
}

struct Harness {
    sink: Arc<RecordingSink>,
    surfaces: Arc<CountingSurfaces>,
    session: HostSession,
}

fn harness() -> Harness {
    let sink = Arc::new(RecordingSink::default());
    let surfaces = Arc::new(CountingSurfaces::default());
    let session = HostSession::new(
        StdRuntime::shared(),
        sink.clone(),
        surfaces.clone(),
        Arc::new(NoDecoders),
    );
    Harness {
        sink,
        surfaces,
        session,
    }
}

const SESSION: GuestUuid = GuestUuid([0x5a; 16]);

fn request(message_type: &[u8; 4], uuid: GuestUuid, reply_id: u64) -> MessageWriter {
    MessageWriter::new(
        FourCc::new(*message_type),
        MessageFlags::GUEST_EXPECTS_REPLY,
        Some(uuid),
        reply_id,
    )
}

fn with_surfaces(ids: &[u64], message: Vec<u8>) -> Vec<u8> {
    let mut bytes = vec![0, 0, 0, 0x30];
    bytes.extend_from_slice(b"subs");
    for slot in 0..5 {
        bytes.extend_from_slice(&ids.get(slot).copied().unwrap_or(0).to_be_bytes());
    }
    bytes.extend_from_slice(&message);
    bytes
}

fn parse(bytes: &[u8]) -> Message<'_> {
    match Message::parse(bytes) {
        Ok(message) => message,
        Err(error) => panic!("the host sent an unreadable message: {error}"),
    }
}

#[test]
fn the_decoder_list_is_a_reply_with_a_list_and_version_6() -> Result<(), WireError> {
    let h = harness();
    let mut decs = request(b"decs", GuestUuid::ZERO, 41);
    decs.u32(names::VERSION, 6);
    assert_eq!(h.session.deliver(&decs.finish()), NO_ERROR);
    let sent = h.sink.take();
    assert_eq!(sent.len(), 1);
    let reply = parse(&sent[0]);
    assert_eq!(
        reply.header(),
        MessageHeader {
            message_type: FourCc::new(*b"decs"),
            flags: MessageFlags::REPLY_TO_GUEST,
            guest_uuid: GuestUuid::ZERO,
            reply_id: 41,
        }
    );
    assert_eq!(
        reply.property_list(names::DECODERS)?,
        Some(plist::Value::Array(Vec::new()))
    );
    assert_eq!(reply.u32(names::VERSION)?, Some(6));
    Ok(())
}

#[test]
fn capabilities_of_a_codec_the_host_lacks_are_null() -> Result<(), WireError> {
    let h = harness();
    let mut dcap = request(b"dcap", GuestUuid::ZERO, 7);
    dcap.u32(names::VERSION, 6)
        .s32(names::CODEC, i32::from_be_bytes(*b"mp4v"));
    assert_eq!(h.session.deliver(&dcap.finish()), NO_ERROR);
    let sent = h.sink.take();
    let reply = parse(&sent[0]);
    assert!(reply.contains(
        names::DECODER_CAPABILITIES,
        crate::wire::message::ValueType::PropertyList
    ));
    assert_eq!(reply.property_list(names::DECODER_CAPABILITIES)?, None);
    assert_eq!(reply.u32(names::VERSION)?, Some(6));
    Ok(())
}

/// A backend that lists AV1 Main and opens nothing.
struct Av1Listed;

impl DecoderBackend for Av1Listed {
    fn capabilities(&self) -> Vec<CodecCapabilities> {
        vec![CodecCapabilities {
            codec: crate::backend::Codec::Av1,
            profiles: vec![crate::backend::ProfileSupport {
                profile: 0,
                max_level: None,
            }],
            max_width: 8192,
            max_height: 4352,
        }]
    }

    fn open(&self, _config: &DecoderConfig) -> Result<Box<dyn BackendDecoder>, BackendError> {
        Err(BackendError::Unsupported("listed only".into()))
    }
}

fn av1_session() -> (Arc<RecordingSink>, HostSession) {
    let sink = Arc::new(RecordingSink::default());
    let session = HostSession::new(
        StdRuntime::shared(),
        sink.clone(),
        Arc::new(CountingSurfaces::default()),
        Arc::new(Av1Listed),
    );
    (sink, session)
}

#[test]
fn the_av1_decoder_is_listed_as_apples_is_without_capabilities() -> Result<(), WireError> {
    let (sink, session) = av1_session();
    let mut decs = request(b"decs", GuestUuid::ZERO, 3);
    decs.u32(names::VERSION, 6);
    assert_eq!(session.deliver(&decs.finish()), NO_ERROR);
    let sent = sink.take();
    let Some(plist::Value::Array(entries)) = parse(&sent[0]).property_list(names::DECODERS)? else {
        return Err(WireError::Malformed("no decoder list"));
    };
    let Some(entry) = entries.first().and_then(plist::Value::as_dictionary) else {
        return Err(WireError::Malformed("no AV1 entry"));
    };
    // Measured on macOS 27 (26A434): `avd.av1`'s entry has these keys and
    // no VTDecoderCapabilities.
    let mut keys: Vec<&str> = entry.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "CMClassImplementationID",
            "CMClassImplementationName",
            "IsPreferredInternalPlugin",
            "VTCodecType",
            "VTRating",
        ]
    );
    assert_eq!(
        entry.get("VTCodecType"),
        Some(&plist::Value::Array(vec![plist::Value::String(
            "av01".into()
        )]))
    );
    assert_eq!(
        entry.get("CMClassImplementationID"),
        Some(&plist::Value::String(
            "reims-vgpu-video.videodecoder.av1".into()
        ))
    );
    Ok(())
}

#[test]
fn av1_capabilities_are_in_apples_av1_shape_for_the_profiles_listed() -> Result<(), WireError> {
    let (sink, session) = av1_session();
    let mut dcap = request(b"dcap", GuestUuid::ZERO, 4);
    dcap.u32(names::VERSION, 6)
        .s32(names::CODEC, i32::from_be_bytes(*b"av01"));
    assert_eq!(session.deliver(&dcap.finish()), NO_ERROR);
    let sent = sink.take();
    let strings = |values: &[&str]| {
        plist::Value::Array(
            values
                .iter()
                .map(|&v| plist::Value::String(v.into()))
                .collect(),
        )
    };
    let profile = crate::wire::property_list::dictionary([
        ("VTChromaSubsampling", strings(&["110", "111"])),
        ("VTColorDepth", strings(&["08", "10"])),
        ("VTIsHardwareAccelerated", plist::Value::Boolean(true)),
        ("VTMaxDecodeLevel", plist::Value::Integer(19.into())),
        ("VTMaxHDRPlaybackLevel", plist::Value::Integer(16.into())),
        ("VTMaxPlaybackLevel", plist::Value::Integer(17.into())),
    ]);
    let expected = crate::wire::property_list::dictionary([
        (
            "VTPerProfileSupport",
            plist::Value::Dictionary(crate::wire::property_list::dictionary([(
                "0",
                plist::Value::Dictionary(profile),
            )])),
        ),
        (
            "VTSupportedProfiles",
            plist::Value::Array(vec![plist::Value::Integer(0.into())]),
        ),
    ]);
    assert_eq!(
        parse(&sent[0]).property_list(names::DECODER_CAPABILITIES)?,
        Some(plist::Value::Dictionary(expected))
    );
    Ok(())
}

/// A backend that lists VP9 Profiles 0 and 2 and opens nothing.
struct Vp9Listed;

impl DecoderBackend for Vp9Listed {
    fn capabilities(&self) -> Vec<CodecCapabilities> {
        vec![CodecCapabilities {
            codec: crate::backend::Codec::Vp9,
            profiles: [0, 2]
                .into_iter()
                .map(|profile| crate::backend::ProfileSupport {
                    profile,
                    max_level: None,
                })
                .collect(),
            max_width: 8192,
            max_height: 4352,
        }]
    }

    fn open(&self, _config: &DecoderConfig) -> Result<Box<dyn BackendDecoder>, BackendError> {
        Err(BackendError::Unsupported("listed only".into()))
    }
}

fn vp9_session() -> (Arc<RecordingSink>, HostSession) {
    let sink = Arc::new(RecordingSink::default());
    let session = HostSession::new(
        StdRuntime::shared(),
        sink.clone(),
        Arc::new(CountingSurfaces::default()),
        Arc::new(Vp9Listed),
    );
    (sink, session)
}

#[test]
fn the_vp9_decoder_is_listed_as_apples_is_without_capabilities() -> Result<(), WireError> {
    let (sink, session) = vp9_session();
    let mut decs = request(b"decs", GuestUuid::ZERO, 3);
    decs.u32(names::VERSION, 6);
    assert_eq!(session.deliver(&decs.finish()), NO_ERROR);
    let sent = sink.take();
    let Some(plist::Value::Array(entries)) = parse(&sent[0]).property_list(names::DECODERS)? else {
        return Err(WireError::Malformed("no decoder list"));
    };
    let Some(entry) = entries.first().and_then(plist::Value::as_dictionary) else {
        return Err(WireError::Malformed("no VP9 entry"));
    };
    // Measured on macOS 27 (26A434): `avd.vp9`'s entry has these keys and
    // no VTDecoderCapabilities.
    let mut keys: Vec<&str> = entry.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "CMClassImplementationID",
            "CMClassImplementationName",
            "IsPreferredInternalPlugin",
            "VTCodecType",
            "VTRating",
        ]
    );
    assert_eq!(
        entry.get("VTCodecType"),
        Some(&plist::Value::Array(vec![plist::Value::String(
            "vp09".into()
        )]))
    );
    assert_eq!(
        entry.get("CMClassImplementationID"),
        Some(&plist::Value::String(
            "reims-vgpu-video.videodecoder.vp9".into()
        ))
    );
    assert_eq!(
        entry.get("VTRating"),
        Some(&plist::Value::Integer(400.into()))
    );
    Ok(())
}

#[test]
fn vp9_capabilities_are_null_as_apples_are() -> Result<(), WireError> {
    // Measured on macOS 27 (26A434): `dcap` for `vp09` answers `,dca` as an
    // empty `=pls`, then `,ver` 6, with no error.
    let (sink, session) = vp9_session();
    let mut dcap = request(b"dcap", GuestUuid::ZERO, 4);
    dcap.u32(names::VERSION, 6)
        .s32(names::CODEC, i32::from_be_bytes(*b"vp09"));
    assert_eq!(session.deliver(&dcap.finish()), NO_ERROR);
    let sent = sink.take();
    let reply = parse(&sent[0]);
    assert!(reply.contains(
        names::DECODER_CAPABILITIES,
        crate::wire::message::ValueType::PropertyList
    ));
    assert_eq!(reply.property_list(names::DECODER_CAPABILITIES)?, None);
    assert_eq!(reply.u32(names::VERSION)?, Some(6));
    Ok(())
}

#[test]
fn a_codec_sent_as_u32_is_malformed_and_unanswered() {
    // Measured on macOS 27 (26A434): `dcap` with `,cod` as `=u32` returns
    // -19093 and sends nothing.
    let h = harness();
    let mut dcap = request(b"dcap", GuestUuid::ZERO, 7);
    dcap.u32(names::VERSION, 6)
        .u32(names::CODEC, u32::from_be_bytes(*b"avc1"));
    assert_eq!(h.session.deliver(&dcap.finish()), -19093);
    assert!(h.sink.take().is_empty());
}

#[test]
fn the_encoder_and_processor_lists_are_empty() -> Result<(), WireError> {
    let h = harness();
    let mut encs = request(b"encs", GuestUuid::ZERO, 1);
    encs.u32(names::VERSION, 6);
    let mut meps = request(b"meps", GuestUuid::ZERO, 2);
    meps.u32(names::VERSION, 6);
    assert_eq!(h.session.deliver(&encs.finish()), NO_ERROR);
    assert_eq!(h.session.deliver(&meps.finish()), NO_ERROR);
    let sent = h.sink.take();
    assert_eq!(sent.len(), 2);
    let empty = Some(plist::Value::Array(Vec::new()));
    assert_eq!(parse(&sent[0]).property_list(names::ENCODERS)?, empty);
    assert_eq!(
        parse(&sent[1]).property_list(names::MOTION_ESTIMATORS)?,
        empty
    );
    assert_eq!(parse(&sent[1]).u32(names::VERSION)?, Some(6));
    Ok(())
}

/// An encoder backend that lists H.264 and HEVC and opens nothing.
struct ListedEncoders;

impl crate::encoder_backend::EncoderBackend for ListedEncoders {
    fn encoders(&self) -> Vec<crate::encoder_backend::EncoderCapabilities> {
        [crate::backend::Codec::H264, crate::backend::Codec::Hevc]
            .map(|codec| crate::encoder_backend::EncoderCapabilities {
                codec,
                profiles: vec![77],
                max_width: 4096,
                max_height: 4096,
                source_formats: vec![crate::backend::PixelFormat::Nv12VideoRange],
                frame_reordering: false,
            })
            .to_vec()
    }

    fn open(
        &self,
        _config: &crate::encoder_backend::EncoderConfig,
    ) -> Result<Box<dyn crate::encoder_backend::BackendEncoder>, BackendError> {
        Err(BackendError::Unsupported("listed only".into()))
    }
}

#[test]
fn the_encoder_list_is_the_backends_in_apples_shape() -> Result<(), WireError> {
    let sink = Arc::new(RecordingSink::default());
    let session = HostSession::with_encoders(
        StdRuntime::shared(),
        sink.clone(),
        Arc::new(CountingSurfaces::default()),
        Arc::new(NoDecoders),
        Arc::new(ListedEncoders),
    );
    let mut encs = request(b"encs", GuestUuid::ZERO, 1);
    encs.u32(names::VERSION, 6);
    assert_eq!(session.deliver(&encs.finish()), NO_ERROR);
    let sent = sink.take();
    assert_eq!(sent.len(), 1);
    let Some(plist::Value::Array(entries)) = parse(&sent[0]).property_list(names::ENCODERS)? else {
        return Err(WireError::Malformed("no encoder list"));
    };
    let entry = |index: usize, key: &str| {
        entries[index]
            .as_dictionary()
            .and_then(|entry| entry.get(key))
            .cloned()
    };
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entry(0, "CMClassImplementationID"),
        Some(plist::Value::String(
            "reims-vgpu-video.videoencoder.avc".into()
        ))
    );
    assert_eq!(
        entry(0, "VTCodecType"),
        Some(plist::Value::Integer(
            i64::from(u32::from_be_bytes(*b"avc1")).into()
        ))
    );
    assert_eq!(
        entry(1, "VTCodecName"),
        Some(plist::Value::String("HEVC".into()))
    );
    assert_eq!(
        entry(1, "VTRating"),
        Some(plist::Value::Integer(400.into()))
    );
    assert_eq!(entry(1, "Hide"), None);
    assert_eq!(parse(&sent[0]).u32(names::VERSION)?, Some(6));
    Ok(())
}

#[test]
fn a_backend_with_encoders_alone_lists_no_decoder_and_its_encoders() -> Result<(), WireError> {
    let sink = Arc::new(RecordingSink::default());
    let backend = Backend {
        encoders: Some(Arc::new(ListedEncoders)),
        ..Backend::none("encoders alone")
    };
    let session = HostSession::with_backend(
        StdRuntime::shared(),
        sink.clone(),
        Arc::new(CountingSurfaces::default()),
        &backend,
    );
    let mut decs = request(b"decs", GuestUuid::ZERO, 1);
    decs.u32(names::VERSION, 6);
    assert_eq!(session.deliver(&decs.finish()), NO_ERROR);
    let mut encs = request(b"encs", GuestUuid::ZERO, 2);
    encs.u32(names::VERSION, 6);
    assert_eq!(session.deliver(&encs.finish()), NO_ERROR);
    let sent = sink.take();
    assert_eq!(sent.len(), 2);
    assert_eq!(
        parse(&sent[0]).property_list(names::DECODERS)?,
        Some(plist::Value::Array(Vec::new()))
    );
    let Some(plist::Value::Array(entries)) = parse(&sent[1]).property_list(names::ENCODERS)? else {
        return Err(WireError::Malformed("no encoder list"));
    };
    assert_eq!(entries.len(), 2);
    Ok(())
}

#[test]
fn a_guest_older_than_6_gets_no_processor_list() {
    // Measured on macOS 27 (26A434): `meps` with `,ver` 5 returns -19960
    // and sends nothing.
    let h = harness();
    let mut meps = request(b"meps", GuestUuid::ZERO, 2);
    meps.u32(names::VERSION, 5);
    assert_eq!(h.session.deliver(&meps.finish()), -19960);
    assert!(h.sink.take().is_empty());
}

#[test]
fn jpeg_requests_without_an_engine_are_answered_as_by_apples_host_without_one()
-> Result<(), WireError> {
    let h = harness();
    assert_eq!(
        h.session
            .deliver(&request(b"jcap", GuestUuid::ZERO, 3).finish()),
        NO_ERROR
    );
    let jdec = request(b"jdec", GuestUuid::ZERO, 4).finish();
    assert_eq!(h.session.deliver(&with_surfaces(&[5, 6], jdec)), NO_ERROR);
    let jenc = request(b"jenc", GuestUuid::ZERO, 5).finish();
    assert_eq!(h.session.deliver(&with_surfaces(&[5, 6], jenc)), NO_ERROR);
    let sent = h.sink.take();
    assert_eq!(sent.len(), 5, "three replies, each surface pair's release");
    // CMPhoto's -16994, with no capabilities.
    assert_eq!(parse(&sent[0]).s32(names::ERROR)?, Some(-16994));
    assert_eq!(parse(&sent[0]).dictionary(names::JPEG_CAPABILITIES)?, None);
    assert_eq!(parse(&sent[1]).s32(names::ERROR)?, Some(-16994));
    assert!(read_surface_release(&sent[2]).is_some());
    assert_eq!(parse(&sent[3]).s32(names::ERROR)?, Some(-16994));
    assert!(read_surface_release(&sent[4]).is_some());
    Ok(())
}

#[test]
fn a_jpeg_decode_needs_two_surfaces() -> Result<(), WireError> {
    let h = harness();
    let jdec = request(b"jdec", GuestUuid::ZERO, 4).finish();
    assert_eq!(h.session.deliver(&with_surfaces(&[5], jdec)), NO_ERROR);
    let sent = h.sink.take();
    assert_eq!(parse(&sent[0]).s32(names::ERROR)?, Some(-12902));
    Ok(())
}

#[test]
fn surfaces_held_for_a_message_are_released_after_it() -> Result<(), WireError> {
    let h = harness();
    let jdec = request(b"jdec", GuestUuid::ZERO, 4).finish();
    assert_eq!(h.session.deliver(&with_surfaces(&[5, 6], jdec)), NO_ERROR);
    let sent = h.sink.take();
    assert_eq!(sent.len(), 2, "the reply, then the release");
    assert_eq!(parse(&sent[0]).s32(names::ERROR)?, Some(-16994));
    assert_eq!(
        read_surface_release(&sent[1]),
        Some((vec![MappingId(5), MappingId(6)], &[][..]))
    );
    assert_eq!(h.surfaces.held(5), 0);
    assert_eq!(h.surfaces.held(6), 0);
    Ok(())
}

/// The surfaces of one message go to the guest in one `rels` once both are
/// let go, though another thread's delivery sweeps while the delivering
/// thread lets go of the first: a sweep then names neither.
#[test]
fn surfaces_let_go_together_are_released_in_one_rels_whoever_sweeps() -> Result<(), WireError> {
    let sink = Arc::new(RecordingSink::default());
    let surfaces = Arc::new(CountingSurfaces::default());
    let session = Arc::new(HostSession::new(
        StdRuntime::shared(),
        sink.clone(),
        surfaces.clone(),
        Arc::new(NoDecoders),
    ));
    let on_release: OnRelease = Box::new({
        let session = Arc::downgrade(&session);
        move || {
            let stray = MessageWriter::new(FourCc::new(*b"-all"), MessageFlags::NONE, None, 0);
            let delivering = StdRuntime::thread("test-delivering", move || {
                if let Some(session) = session.upgrade() {
                    session.deliver(&stray.finish());
                }
            });
            if let Ok(delivering) = delivering {
                let _ = delivering.join();
            }
        }
    });
    surfaces.on_release.lock().insert(5, on_release);
    let jdec = request(b"jdec", GuestUuid::ZERO, 4).finish();
    assert_eq!(session.deliver(&with_surfaces(&[5, 6], jdec)), NO_ERROR);
    let sent = sink.take();
    assert_eq!(sent.len(), 2, "the reply, then the release");
    assert_eq!(parse(&sent[0]).s32(names::ERROR)?, Some(-16994));
    assert_eq!(
        read_surface_release(&sent[1]),
        Some((vec![MappingId(5), MappingId(6)], &[][..]))
    );
    assert!(surfaces.on_release.lock().is_empty());
    assert_eq!(surfaces.held(5), 0);
    assert_eq!(surfaces.held(6), 0);
    Ok(())
}

/// The mapper's retain runs outside the surface table's lock. A host
/// whose IOSurface device's interrupt delivers a message, as the C API
/// lets it, while a delivery waits in a retain for that device's queue,
/// has it answered: under the lock, its sweep would wait for the waiting
/// delivery for ever. The inner delivery names the same new surface: the
/// outer one holds it again once the inner one has let it go, and one
/// `rels` names it, after both replies, as if the outer delivery had held
/// it first.
#[test]
fn a_delivery_from_inside_a_surfaces_retain_is_answered() -> Result<(), WireError> {
    let sink = Arc::new(RecordingSink::default());
    let surfaces = Arc::new(CountingSurfaces::default());
    let session = Arc::new(HostSession::new(
        StdRuntime::shared(),
        sink.clone(),
        surfaces.clone(),
        Arc::new(NoDecoders),
    ));
    let inner_answer = Arc::new(Mutex::new(None));
    let on_retain: OnRetain = Box::new({
        let session = Arc::downgrade(&session);
        let inner_answer = inner_answer.clone();
        move || {
            let jdec = request(b"jdec", GuestUuid::ZERO, 2).finish();
            let (answer, answered) = std::sync::mpsc::channel();
            let delivering = StdRuntime::thread("test-interrupt", move || {
                if let Some(session) = session.upgrade() {
                    let _ = answer.send(session.deliver(&with_surfaces(&[5], jdec)));
                }
            });
            assert!(delivering.is_ok());
            *inner_answer.lock() = answered.recv_timeout(Duration::from_secs(10)).ok();
        }
    });
    surfaces.on_retain.lock().insert(5, on_retain);
    let jdec = request(b"jdec", GuestUuid::ZERO, 1).finish();
    assert_eq!(session.deliver(&with_surfaces(&[5], jdec)), NO_ERROR);
    assert_eq!(*inner_answer.lock(), Some(NO_ERROR));
    let sent = sink.take();
    let replies: Vec<_> = (sent.iter())
        .filter_map(|m| read_header(m, 0).ok())
        .map(|h| (h.message_type, h.reply_id))
        .collect();
    assert_eq!(
        replies,
        [(FourCc::new(*b"jdec"), 2), (FourCc::new(*b"jdec"), 1)]
    );
    assert_eq!(sent.len(), 3, "both replies, then the release");
    assert_eq!(
        read_surface_release(&sent[2]),
        Some((vec![MappingId(5)], &[][..]))
    );
    assert_eq!(surfaces.retains.load(Ordering::SeqCst), 2);
    assert_eq!(surfaces.held(5), 0);
    Ok(())
}

#[test]
fn a_surface_that_cannot_be_held_drops_them_all_and_releases_id_0() {
    let h = harness();
    let jdec = request(b"jdec", GuestUuid::ZERO, 4).finish();
    h.session.deliver(&with_surfaces(&[5, 500], jdec));
    let sent = h.sink.take();
    let reply = parse(&sent[0]);
    // With no surfaces the decode is refused for its count.
    assert_eq!(reply.s32(names::ERROR), Ok(Some(-12902)));
    assert_eq!(
        read_surface_release(&sent[1]),
        Some((vec![MappingId(5), MappingId(0)], &[][..]))
    );
    assert_eq!(h.surfaces.held(5), 0);
}

#[test]
fn an_encoder_session_is_made_but_refused() -> Result<(), WireError> {
    let h = harness();
    let mut create = request(b"+enc", SESSION, 9);
    create
        .s32(names::CODEC, i32::from_be_bytes(*b"avc1"))
        .u32(names::VERSION, 6);
    let create = create.finish();
    assert_eq!(h.session.deliver(&create), NO_ERROR);
    let sent = h.sink.take();
    let reply = parse(&sent[0]);
    assert_eq!(reply.header().guest_uuid, SESSION);
    assert_eq!(reply.s32(names::ERROR)?, Some(-12908));
    assert_eq!(reply.u32(names::VERSION)?, Some(6));
    assert!(reply.dictionary(names::HOST_INFO)?.is_some());

    assert_eq!(h.session.deliver(&create), -19093, "a second create");
    assert!(h.sink.take().is_empty());

    let close = MessageWriter::new(FourCc::new(*b"-enc"), MessageFlags::NONE, Some(SESSION), 0);
    assert_eq!(h.session.deliver(&close.finish()), NO_ERROR);
    let other = request(b"Eemt", SESSION, 0).finish();
    assert_eq!(h.session.deliver(&other), -19094);
    Ok(())
}

#[test]
fn a_create_without_a_codec_makes_no_session() {
    let h = harness();
    let mut create = request(b"+dec", SESSION, 9);
    create.u32(names::VERSION, 6);
    assert_eq!(h.session.deliver(&create.finish()), -19093);
    assert!(h.sink.take().is_empty());
    assert_eq!(
        h.session.deliver(&request(b"dsup", SESSION, 0).finish()),
        -19094
    );
}

#[test]
fn terminate_all_is_answered_with_nothing() {
    let h = harness();
    let all = MessageWriter::new(FourCc::new(*b"-all"), MessageFlags::NONE, None, 0);
    assert_eq!(h.session.deliver(&all.finish()), NO_ERROR);
    assert!(h.sink.take().is_empty());
}

#[test]
fn an_unknown_session_is_not_found() {
    let h = harness();
    assert_eq!(
        h.session.deliver(&request(b"zzzz", SESSION, 0).finish()),
        -19094
    );
}

#[test]
fn a_malformed_message_is_refused() {
    let h = harness();
    assert_eq!(
        h.session.deliver(&[0, 0, 0, 8, b'a', b'b', b'c', b'd']),
        -19093
    );
    assert_eq!(h.session.deliver(&[1, 2]), -19093);
}

#[test]
fn an_invalidated_session_sends_nothing() {
    let h = harness();
    h.session.invalidate();
    let mut decs = request(b"decs", GuestUuid::ZERO, 41);
    decs.u32(names::VERSION, 6);
    assert_eq!(h.session.deliver(&decs.finish()), -19098);
    assert!(h.sink.take().is_empty());
    h.session.complete_invalidate();
}

#[test]
fn the_terminate_all_message_is_a_bare_bye() {
    let bytes = terminate_all_sessions_message();
    let mut expected = vec![0, 0, 0, 0x30];
    expected.extend_from_slice(b"vtpvBye!");
    expected.resize(48, 0);
    assert_eq!(bytes, expected);
    assert_eq!(
        read_header(&bytes, 0).map(|header| header.message_type),
        Ok(FourCc::new(*b"Bye!"))
    );
}
