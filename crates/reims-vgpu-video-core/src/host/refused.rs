//! Sessions for codecs this host does not offer: the guest asked to open
//! one, the session exists so that its close finds it, and its create
//! reply says why it has no codec.
//!
//! Apple's host makes the session even when it cannot make the codec, and
//! puts the creation's status in the reply's `,err`. The guest then fails its own
//! create and closes the session.

use crate::host::names;
use crate::host::session::create_reply;
use crate::status::{NO_ERROR, OsStatus, Status};
use crate::wire::WireError;
use crate::wire::message::Message;

/// Which kind of session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefusedKind {
    /// `+dec`.
    Decoder,
    /// `+enc`.
    Encoder,
    /// `+mep`.
    MotionEstimation,
}

impl RefusedKind {
    /// The status the create reply carries: the one Apple's selection
    /// returns when nothing matches.
    pub(crate) fn status(self) -> Status {
        match self {
            Self::Decoder => Status::CouldNotFindVideoDecoder,
            Self::Encoder => Status::CouldNotFindVideoEncoder,
            Self::MotionEstimation => Status::MotionEstimationProcessorNotFound,
        }
    }

    /// The status the create reply carries for a session the host has no
    /// room for now: VideoToolbox's "not available now", and for motion
    /// estimation, which has no such status, its own not-found.
    pub(crate) fn not_available_now(self) -> Status {
        match self {
            Self::Decoder => Status::VideoDecoderNotAvailableNow,
            Self::Encoder => Status::VideoEncoderNotAvailableNow,
            Self::MotionEstimation => self.status(),
        }
    }
}

/// A session with no codec.
#[derive(Debug)]
pub(crate) struct RefusedSession {
    kind: RefusedKind,
}

impl RefusedSession {
    /// Reads the create message as Apple's create reads it, and makes the
    /// session and its reply. A decoder or encoder create needs a nonzero
    /// `,cod`; each reads `,ver`, then `,cid` or `,meo`, then `,gin`, and a
    /// parameter that does not read fails the create with no session and no reply.
    pub(crate) fn create(
        message: &Message<'_>,
        kind: RefusedKind,
    ) -> Result<(Self, Vec<u8>), WireError> {
        Self::create_with_status(message, kind, kind.status().code())
    }

    /// As [`Self::create`], with `status` in the reply's `,err`: the
    /// backend's, for a codec it lists but cannot make now.
    pub(crate) fn create_with_status(
        message: &Message<'_>,
        kind: RefusedKind,
        status: OsStatus,
    ) -> Result<(Self, Vec<u8>), WireError> {
        let reply = Self::reply(message, kind, status)?;
        Ok((Self { kind }, reply))
    }

    /// The create reply `create` makes, with `status` in its `,err`, and
    /// no session: for a create the host refuses before it would make one
    /// (`host::guest_bounds::MAX_SESSIONS`).
    pub(crate) fn reply(
        message: &Message<'_>,
        kind: RefusedKind,
        status: impl Into<OsStatus>,
    ) -> Result<Vec<u8>, WireError> {
        match kind {
            RefusedKind::Decoder | RefusedKind::Encoder => {
                if message.s32(names::CODEC)?.unwrap_or(0) == 0 {
                    return Err(WireError::Malformed("no codec type"));
                }
                message.u32(names::VERSION)?;
                message.property_list(names::CODEC_ID)?;
            }
            RefusedKind::MotionEstimation => {
                message.u32(names::VERSION)?;
                message.dictionary(names::MOTION_OPTIONS)?;
            }
        }
        message.dictionary(names::GUEST_INFO)?;
        create_reply(message, status.into())
    }

    /// A message for the session. With no codec there is nothing to answer
    /// with; a guest whose create failed sends nothing but the close.
    pub(crate) fn deliver(&self, message: &Message<'_>) -> OsStatus {
        log::warn!(
            "{:?} session without a codec got '{}'",
            self.kind,
            message.header().message_type
        );
        NO_ERROR
    }
}
