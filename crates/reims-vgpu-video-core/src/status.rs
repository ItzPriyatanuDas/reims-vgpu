//! The status codes the guest sees: the i32 the device writes back for each
//! message, and the `,err` parameters of replies and host requests.
//!
//! Each value is Apple's. The VideoToolbox ones are public (`VTErrors.h`
//! in the macOS SDK); the paravirtualization ones are Apple's host's and
//! guest's own. The variant names are ours, from Apple's constant names.

/// The status of an operation: 0 is success, anything else an `OSStatus`
/// error. Backends and the guest can produce any value, so it stays an i32
/// where it crosses the wire.
pub type OsStatus = i32;

/// The success status.
pub const NO_ERROR: OsStatus = 0;

/// The error statuses this crate produces itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum Status {
    /// `kCMBaseObjectError_UnimplementedFunction`: an operation the decoder
    /// does not implement.
    UnimplementedFunction = -12782,
    /// `kVTPropertyNotSupportedErr`.
    PropertyNotSupported = -12900,
    /// `kVTPropertyReadOnlyErr`.
    PropertyReadOnly = -12901,
    /// `kVTParameterErr`.
    Parameter = -12902,
    /// `kVTAllocationFailedErr`.
    AllocationFailed = -12904,
    /// `kVTInvalidSessionErr`.
    InvalidSession = -12903,
    /// `kVTCouldNotFindVideoDecoderErr`.
    CouldNotFindVideoDecoder = -12906,
    /// `kVTCouldNotFindVideoEncoderErr`.
    CouldNotFindVideoEncoder = -12908,
    /// `kVTVideoDecoderBadDataErr`.
    VideoDecoderBadData = -12909,
    /// `kVTVideoDecoderUnsupportedDataFormatErr`.
    VideoDecoderUnsupportedDataFormat = -12910,
    /// `kVTVideoDecoderMalfunctionErr`.
    VideoDecoderMalfunction = -12911,
    /// `kVTVideoEncoderMalfunctionErr`.
    VideoEncoderMalfunction = -12912,
    /// `kVTVideoDecoderNotAvailableNowErr` (`VTErrors.h:42`): what a `+dec`
    /// past the sessions this host keeps is answered with.
    VideoDecoderNotAvailableNow = -12913,
    /// `kVTVideoEncoderNotAvailableNowErr` (`VTErrors.h:45`), likewise for
    /// `+enc`.
    VideoEncoderNotAvailableNow = -12915,
    /// `kVTVideoDecoderReferenceMissingErr`: what Apple's HEVC decoder
    /// answers a RASL picture it skips with.
    VideoDecoderReferenceMissing = -17694,
    /// `kVTSessionMalfunctionErr` (`VTErrors.h:61`): what Apple's
    /// motion-estimation processor emits a frame with after its
    /// invalidation.
    SessionMalfunction = -17691,
    /// What Apple's motion-estimation processor answers `mset` with for a
    /// key it does not take, `Paravirtualized` among them (measured on the
    /// M3 Max, macOS 27, 2026-10-24). The name is ours: no source names the
    /// value.
    MotionPropertyRefused = -2002,
    /// A guest reply that did not come in time
    /// (kVTPVReplyTimedOutErr).
    ReplyTimedOut = -19092,
    /// A message that does not parse, or that the receiver cannot take in
    /// its state (kVTPVMalformedMessageErr).
    MalformedMessage = -19093,
    /// A message for a UUID with no session.
    SessionNotFound = -19094,
    /// A wait cut short because its reply clerk was invalidated
    /// (kVTPVInvalidatedErr).
    ReplyClerkInvalidated = -19095,
    /// A send on an invalidated host or decoder session.
    SessionInvalidated = -19098,
    /// What Apple's host answers `esta` with for a size its AVE encoder does
    /// not take, from `VTVideoEncoderStartSession`, before any `Epba`:
    /// H.264 at 8192x8192 and 16384x16384, HEVC at 16384x16384 (measured on
    /// the M3 Max, macOS 27, 2026-10-24). The name is ours: no source names
    /// the value.
    EncoderSizeUnsupported = -19354,
    /// What Apple's VP9 decoder answers `dsta` with for a `vp09` format
    /// description that carries no `vpcC` record, before any `Dpba`; each
    /// `dfrm` after it is answered [`Self::DecoderNotStarted`] (measured on
    /// the M3 Max, macOS 27.0.1, 26A434, 2026-10-25). The name is ours: no
    /// source names the value.
    ConfigurationRecordMissing = -8971,
    /// What Apple's AVD decoders answer `dfrm` with when they have no
    /// decode session: before any `dsta`, after one they refused, and after
    /// `dinv`, with `,inf` 0 and nothing emitted (AVD's H.264, HEVC, VP9
    /// and AV1 decoders, either decode flag, on the M3 Max, macOS 27.0.1,
    /// 2026-10-24). Apple's host hands it on from
    /// `VTVideoDecoderDecodeFrame`. A positive status, AVD's own; the name
    /// is ours: no source names the value.
    DecoderNotStarted = 311,
    /// No motion-estimation processor matches
    /// (kVTMESProcessorNotFoundErr).
    MotionEstimationProcessorNotFound = -19350,
    /// A motion-estimation list request from a guest older than protocol 6
    /// (kVTMESUnsupportedGuestErr).
    UnsupportedGuest = -19960,
}

impl Status {
    /// The value the guest sees.
    #[expect(
        clippy::as_conversions,
        reason = "each status's discriminant is its code"
    )]
    pub const fn code(self) -> OsStatus {
        self as OsStatus
    }
}

impl From<Status> for OsStatus {
    fn from(status: Status) -> Self {
        status.code()
    }
}
