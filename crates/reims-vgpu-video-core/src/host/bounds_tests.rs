//! The bounds of `guest_bounds`, against a host session whose sink records
//! and never answers, and a backend that lists H.264 and decodes nothing.

use std::sync::Arc;
use std::time::Duration;

use super::tests::RecordingSink;
use super::*;
use crate::backend::{BackendDecoder, BackendError, Codec, CodecCapabilities, DecoderConfig};
use crate::fourcc::FourCc;
use crate::runtime::{Runtime, StdRuntime};
use crate::surfaces::{GuestSurface, SurfaceError, SurfaceLayout};
use crate::wire::message::{GuestUuid, MessageHeader};
use crate::wire::sample_buffer::SampleBuffer;
use crate::wire::surface_boxes::read_surface_release;

/// Lists H.264, and opens no decoder.
struct ListsH264;

impl DecoderBackend for ListsH264 {
    fn capabilities(&self) -> Vec<CodecCapabilities> {
        vec![CodecCapabilities {
            codec: Codec::H264,
            profiles: Vec::new(),
            max_width: 4096,
            max_height: 4096,
        }]
    }

    fn open(&self, _config: &DecoderConfig) -> Result<Box<dyn BackendDecoder>, BackendError> {
        Err(BackendError::Unsupported("none here".into()))
    }
}

/// Every mapping ID is a surface.
struct AnySurfaces;

struct Plain;

impl GuestSurface for Plain {
    fn layout(&self) -> SurfaceLayout {
        SurfaceLayout {
            pixel_format: FourCc::new(*b"420v"),
            width: 2,
            height: 2,
            planes: Vec::new(),
            bytes_per_row: 2,
        }
    }

    fn write(&self, write: &mut dyn FnMut(&mut [u8])) -> Result<(), SurfaceError> {
        write(&mut [0; 6]);
        Ok(())
    }
}

impl Surfaces for AnySurfaces {
    fn retain(&self, _mapping_id: MappingId) -> Result<Box<dyn GuestSurface>, SurfaceError> {
        Ok(Box::new(Plain))
    }
}

fn session() -> (Arc<RecordingSink>, HostSession) {
    let sink = Arc::new(RecordingSink::default());
    let session = HostSession::new(
        StdRuntime::shared(),
        sink.clone(),
        Arc::new(AnySurfaces),
        Arc::new(ListsH264),
    );
    (sink, session)
}

fn uuid(n: u8) -> GuestUuid {
    GuestUuid([n; 16])
}

fn message(kind: &[u8; 4], uuid: GuestUuid, flags: MessageFlags, reply_id: u64) -> MessageWriter {
    MessageWriter::new(FourCc::new(*kind), flags, Some(uuid), reply_id)
}

fn create_decoder(session: &HostSession, uuid: GuestUuid) -> OsStatus {
    let mut create = message(b"+dec", uuid, MessageFlags::GUEST_EXPECTS_REPLY, 1);
    create.s32(names::CODEC, Codec::H264.fourcc().0.cast_signed());
    session.deliver(&create.finish())
}

/// The replies sent so far for `kind`, waited for up to 5 s until there
/// are `count`.
fn replies(sink: &RecordingSink, kind: &[u8; 4], count: usize) -> Vec<Vec<u8>> {
    let started = StdRuntime.now();
    let mut found = Vec::new();
    loop {
        found.extend(sink.take().into_iter().filter(|sent| {
            read_header(sent, 0)
                .is_ok_and(|header: MessageHeader| header.message_type == FourCc::new(*kind))
        }));
        if found.len() >= count || StdRuntime.elapsed_since(started) > Duration::from_secs(5) {
            return found;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn error_of(reply: &[u8]) -> Option<i32> {
    Message::parse(reply).ok()?.s32(names::ERROR).ok()?
}

#[test]
fn a_reply_longer_than_the_guests_buffer_is_not_sent() -> Result<(), WireError> {
    let (sink, session) = session();
    assert_eq!(create_decoder(&session, uuid(1)), NO_ERROR);
    // RealTime set to 20,000 bytes of data, then asked back: the reply
    // would be past the guest's 0x4000-byte buffers.
    let mut set = message(b"dset", uuid(1), MessageFlags::GUEST_EXPECTS_REPLY, 2);
    set.property_list(
        names::PROPERTY_KEY,
        Some(&plist::Value::String("RealTime".into())),
    )?;
    set.property_list(
        names::PROPERTY_VALUE,
        Some(&plist::Value::Data(vec![7; 20_000])),
    )?;
    assert_eq!(session.deliver(&set.finish()), NO_ERROR);
    assert_eq!(replies(&sink, b"dset", 1).len(), 1);
    let mut copy = message(b"dcop", uuid(1), MessageFlags::GUEST_EXPECTS_REPLY, 3);
    copy.property_list(
        names::PROPERTY_KEY,
        Some(&plist::Value::String("RealTime".into())),
    )?;
    assert_eq!(session.deliver(&copy.finish()), NO_ERROR);
    // A dsup after it is answered, so the dcop was handled before it.
    let supported = message(b"dsup", uuid(1), MessageFlags::GUEST_EXPECTS_REPLY, 4);
    assert_eq!(session.deliver(&supported.finish()), NO_ERROR);
    let sent = {
        let started = StdRuntime.now();
        let mut sent = Vec::new();
        while StdRuntime.elapsed_since(started) < Duration::from_secs(5) {
            sent.extend(sink.take());
            if sent.iter().any(|m: &Vec<u8>| {
                read_header(m, 0).is_ok_and(|h| h.message_type == FourCc::new(*b"dsup"))
            }) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        sent
    };
    assert!(sent.iter().all(|m| m.len() <= GUEST_BUFFER_SIZE));
    assert!(
        !sent
            .iter()
            .any(|m| read_header(m, 0).is_ok_and(|h| h.message_type == FourCc::new(*b"dcop")))
    );
    Ok(())
}

#[test]
fn releases_past_one_message_go_in_several() {
    let (sink, session) = session();
    let shared = &session.shared;
    let ids: Vec<MappingId> = (1..=3000).map(MappingId).collect();
    let holds: Vec<_> = ids
        .chunks(5)
        .flat_map(|chunk| {
            let held = mapped_surfaces::hold(&shared.mapped, &AnySurfaces, chunk);
            held.expect("every surface is held")
        })
        .collect();
    assert_eq!(holds.len(), 3000);
    drop(holds);
    shared.relinquish_unused_surfaces();
    let sent = sink.take();
    assert_eq!(sent.len(), 2);
    let mut released = Vec::new();
    for message in &sent {
        assert!(message.len() <= GUEST_BUFFER_SIZE);
        let Some((ids, rest)) = read_surface_release(message) else {
            panic!("a rels the kext reads");
        };
        assert!(rest.is_empty());
        released.extend(ids);
    }
    assert_eq!(released, ids);
}

#[test]
fn sessions_past_the_bound_are_refused_for_now() -> Result<(), WireError> {
    let (sink, session) = session();
    for n in 0..MAX_SESSIONS {
        assert_eq!(create_decoder(&session, uuid(n as u8 + 1)), NO_ERROR);
    }
    assert_eq!(replies(&sink, b"+dec", MAX_SESSIONS).len(), MAX_SESSIONS);
    let past = uuid(200);
    assert_eq!(create_decoder(&session, past), NO_ERROR);
    let refused = replies(&sink, b"+dec", 1);
    assert_eq!(
        refused.first().and_then(|reply| error_of(reply)),
        Some(Status::VideoDecoderNotAvailableNow.code())
    );
    // No session was kept for it.
    let after = message(b"dsup", past, MessageFlags::GUEST_EXPECTS_REPLY, 2);
    assert_eq!(
        session.deliver(&after.finish()),
        Status::SessionNotFound.code()
    );
    // A close makes room again.
    let close = message(b"-dec", uuid(1), MessageFlags::NONE, 0);
    assert_eq!(session.deliver(&close.finish()), NO_ERROR);
    assert_eq!(create_decoder(&session, past), NO_ERROR);
    let made = replies(&sink, b"+dec", 1);
    assert_eq!(
        made.first().and_then(|reply| error_of(reply)),
        Some(NO_ERROR)
    );
    Ok(())
}

/// A `dfrm` fragment of `piece` bytes of sample buffer, flagged with more
/// to come when `more`.
fn frame_fragment(uuid: GuestUuid, piece: usize, more: bool) -> Vec<u8> {
    let flags = if more {
        MessageFlags::MORE_FRAGMENTS
    } else {
        MessageFlags::GUEST_EXPECTS_REPLY
    };
    let mut fragment = message(b"dfrm", uuid, flags, 9);
    fragment.sample_buffer_with_limit(names::SAMPLE_BUFFER, usize::MAX, &vec![0; piece]);
    fragment.finish()
}

/// A whole `dfrm` of one small sample, which reads.
fn whole_frame(uuid: GuestUuid) -> Vec<u8> {
    let sample = SampleBuffer {
        data: Some(vec![1, 2, 3]),
        sample_count: 1,
        ..SampleBuffer::default()
    };
    let mut frame = message(b"dfrm", uuid, MessageFlags::GUEST_EXPECTS_REPLY, 10);
    frame.s64(names::FRAME, 4);
    frame.sample_buffer_with_limit(names::SAMPLE_BUFFER, usize::MAX, &sample.serialize());
    frame.finish()
}

#[test]
fn a_frame_message_past_the_bound_is_dropped() {
    let (sink, session) = session();
    assert_eq!(create_decoder(&session, uuid(1)), NO_ERROR);
    // 65 fragments of 2 MiB, past 128 MiB, then the last: no reply. With
    // no decode session started, a dfrm that reads is answered 311, as
    // Apple's AVD decoders answer it.
    let piece = 2 << 20;
    for _ in 0..(MAX_FRAME_MESSAGE_BYTES / piece + 1) {
        assert_eq!(
            session.deliver(&frame_fragment(uuid(1), piece, true)),
            NO_ERROR
        );
    }
    assert_eq!(
        session.deliver(&frame_fragment(uuid(1), 16, false)),
        NO_ERROR
    );
    // The next dfrm reads: its reply is the only one.
    assert_eq!(session.deliver(&whole_frame(uuid(1))), NO_ERROR);
    let answered = replies(&sink, b"dfrm", 1);
    assert_eq!(answered.len(), 1);
    assert_eq!(
        answered.first().and_then(|reply| error_of(reply)),
        Some(Status::DecoderNotStarted.code())
    );
    // The session's thread lets the last frame go after its reply: once
    // it has, every fragment before it is handled too, and no reply is
    // left to come.
    assert!(
        held_comes_to_nothing(&session),
        "{} bytes held",
        session.shared.held.bytes()
    );
    assert!(replies(&sink, b"dfrm", 0).is_empty());
}

/// Waits up to 5 s until `session` holds none of the guest's messages,
/// which its sessions' threads let go once each is handled, after its
/// reply: whether it came to that.
fn held_comes_to_nothing(session: &HostSession) -> bool {
    let started = StdRuntime.now();
    while session.shared.held.bytes() > 0 {
        if StdRuntime.elapsed_since(started) > Duration::from_secs(5) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

#[test]
fn messages_past_what_the_host_holds_are_refused() {
    let (_sink, session) = session();
    assert_eq!(create_decoder(&session, uuid(1)), NO_ERROR);
    // Fragments are held until their last comes: past the per-session
    // count, the next is refused, and nothing is held once the session
    // goes.
    let statuses: Vec<OsStatus> = (0..MAX_HELD_MESSAGES_PER_SESSION + 10)
        .map(|_| session.deliver(&frame_fragment(uuid(1), 1, true)))
        .collect();
    assert!(statuses.contains(&Status::AllocationFailed.code()));
    assert!(statuses.iter().filter(|&&s| s == NO_ERROR).count() <= MAX_HELD_MESSAGES_PER_SESSION);
    let close = message(b"-dec", uuid(1), MessageFlags::NONE, 0);
    assert_eq!(session.deliver(&close.finish()), NO_ERROR);
    assert_eq!(session.shared.held.bytes(), 0);
}

/// One session piling up unhandled fragments must not refuse another
/// session's messages. The stalled session never starts its decoder, so its
/// fragments pile up held; a second session's `+dec` and a `dsup` it
/// answers still go through.
#[test]
fn a_flooding_session_does_not_starve_another() -> Result<(), WireError> {
    let (sink, session) = session();
    assert_eq!(create_decoder(&session, uuid(1)), NO_ERROR);
    // Fill the first session past its own bound, holding every fragment.
    let flood: Vec<OsStatus> = (0..MAX_HELD_MESSAGES_PER_SESSION + 10)
        .map(|_| session.deliver(&frame_fragment(uuid(1), 4096, true)))
        .collect();
    assert!(flood.contains(&Status::AllocationFailed.code()));
    // A second session opens and is answered, though the first is full.
    assert_eq!(create_decoder(&session, uuid(2)), NO_ERROR);
    assert_eq!(
        replies(&sink, b"+dec", 1)
            .first()
            .and_then(|reply| error_of(reply)),
        Some(NO_ERROR)
    );
    let supported = message(b"dsup", uuid(2), MessageFlags::GUEST_EXPECTS_REPLY, 4);
    assert_eq!(session.deliver(&supported.finish()), NO_ERROR);
    assert_eq!(replies(&sink, b"dsup", 1).len(), 1);
    // The backstop carries only the first session's share, below its whole.
    assert!(session.shared.held.bytes() < MAX_HELD_BYTES);
    Ok(())
}

#[test]
fn a_reply_past_its_bound_is_malformed() -> Result<(), Status> {
    let clerk = reply_clerk::ReplyClerk::new(StdRuntime::shared());
    let reply_id = clerk.prepare()?;
    let piece = |more| {
        let flags = if more {
            MessageFlags(MessageFlags::REPLY_TO_HOST.0 | MessageFlags::MORE_FRAGMENTS.0)
        } else {
            MessageFlags::REPLY_TO_HOST
        };
        let mut writer = MessageWriter::new(FourCc::new(*b"Dbuf"), flags, None, reply_id);
        writer.data(names::FRAME, &vec![0; 1 << 20]);
        writer.finish()
    };
    let mut refused = false;
    for _ in 0..(MAX_REPLY_BYTES >> 20) + 1 {
        if clerk.deliver(&piece(true), None) == Err(Status::MalformedMessage) {
            refused = true;
            break;
        }
    }
    assert!(refused);
    clerk.clean_up(reply_id);
    Ok(())
}
