//! The message types and parameter keys the host session reads and writes,
//! as the wire spells them. Each is Apple's, as the appender or getter
//! that uses it spells it; the Rust names are ours.

use crate::fourcc::FourCc;

/// A message the host session handles itself, before routing by UUID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionMessage {
    /// `decs`: list the host's decoders.
    DecoderList,
    /// `+dec`: open a decoder session.
    CreateDecoder,
    /// `+enc`: open an encoder session.
    CreateEncoder,
    /// `+mep`: open a motion-estimation session.
    CreateMotionEstimation,
    /// `-all`: on internal builds only, drop every session.
    DestroyAll,
    /// `-dec`: close a decoder session.
    DestroyDecoder,
    /// `-enc`: close an encoder session.
    DestroyEncoder,
    /// `-mep`: close a motion-estimation session.
    DestroyMotionEstimation,
    /// `-uui`: close the decoder and encoder sessions of a UUID.
    DestroyUuid,
    /// `dcap`: a decoder's capabilities.
    DecoderCapabilities,
    /// `meps`: list the host's motion-estimation processors.
    MotionEstimationList,
    /// `encs`: list the host's encoders.
    EncoderList,
    /// `jcap`: the JPEG engine's capabilities.
    JpegCapabilities,
    /// `jdec`: decode a JPEG between two surfaces.
    JpegDecode,
    /// `jenc`: encode a JPEG between two surfaces.
    JpegEncode,
}

impl SessionMessage {
    const ALL: [(Self, [u8; 4]); 15] = [
        (Self::DecoderList, *b"decs"),
        (Self::CreateDecoder, *b"+dec"),
        (Self::CreateEncoder, *b"+enc"),
        (Self::CreateMotionEstimation, *b"+mep"),
        (Self::DestroyAll, *b"-all"),
        (Self::DestroyDecoder, *b"-dec"),
        (Self::DestroyEncoder, *b"-enc"),
        (Self::DestroyMotionEstimation, *b"-mep"),
        (Self::DestroyUuid, *b"-uui"),
        (Self::DecoderCapabilities, *b"dcap"),
        (Self::MotionEstimationList, *b"meps"),
        (Self::EncoderList, *b"encs"),
        (Self::JpegCapabilities, *b"jcap"),
        (Self::JpegDecode, *b"jdec"),
        (Self::JpegEncode, *b"jenc"),
    ];

    /// The message `message_type` names, if the host session handles it.
    pub(crate) fn from_fourcc(message_type: FourCc) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|(_, code)| FourCc::new(*code) == message_type)
            .map(|(message, _)| *message)
    }
}

/// The terminate-all message's type.
pub(crate) const TERMINATE_ALL: FourCc = FourCc::new(*b"Bye!");

/// `,ver`: a protocol version, `=u32`.
pub(crate) const VERSION: FourCc = FourCc::new(*b",ver");
/// `,err`: a status, `=s32`.
pub(crate) const ERROR: FourCc = FourCc::new(*b",err");
/// `,cod`: a codec type, `=s32`.
pub(crate) const CODEC: FourCc = FourCc::new(*b",cod");
/// `,dec`: the decoder list, `=pls`.
pub(crate) const DECODERS: FourCc = FourCc::new(*b",dec");
/// `,enc`: the encoder list, `=pls`.
pub(crate) const ENCODERS: FourCc = FourCc::new(*b",enc");
/// `,mep`: the motion-estimation processor list, `=pls`.
pub(crate) const MOTION_ESTIMATORS: FourCc = FourCc::new(*b",mep");
/// `,dca`: a decoder's capabilities, `=pls`.
pub(crate) const DECODER_CAPABILITIES: FourCc = FourCc::new(*b",dca");
/// `,cid`: the host decoder or encoder the guest chose, `=pls`.
pub(crate) const CODEC_ID: FourCc = FourCc::new(*b",cid");
/// `,gin`: the guest's info, `=pls`.
pub(crate) const GUEST_INFO: FourCc = FourCc::new(*b",gin");
/// `,hin`: the host's info, `=pls`.
pub(crate) const HOST_INFO: FourCc = FourCc::new(*b",hin");
/// `,joo`: JPEG options, `=pls`.
pub(crate) const JPEG_OPTIONS: FourCc = FourCc::new(*b",joo");
/// `,jca`: the JPEG engine's capabilities, `=pls`.
pub(crate) const JPEG_CAPABILITIES: FourCc = FourCc::new(*b",jca");

/// A message for a decoder session, from the guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DecoderMessage {
    /// `dcop`: copy a property.
    CopyProperty,
    /// `dset`: set a property.
    SetProperty,
    /// `dsta`: start the session with a format description.
    StartSession,
    /// `dfrm`: decode a frame.
    DecodeFrame,
    /// `dsup`: the supported properties.
    SupportedProperties,
    /// `dsts`: start a tile session.
    StartTileSession,
    /// `dtil`: decode a tile.
    DecodeTile,
    /// `dtfi`: finish the delayed tiles.
    FinishDelayedTiles,
    /// `dfin`: finish the delayed frames.
    FinishDelayedFrames,
    /// `dcan`: whether a format description can be taken.
    CanAcceptFormatDescription,
    /// `dinv`: invalidate the decoder.
    Invalidate,
    /// `dmul`: set several properties.
    SetProperties,
    /// `dsav`: the serializable properties.
    SerializableProperties,
}

impl DecoderMessage {
    const ALL: [(Self, [u8; 4]); 13] = [
        (Self::CopyProperty, *b"dcop"),
        (Self::SetProperty, *b"dset"),
        (Self::StartSession, *b"dsta"),
        (Self::DecodeFrame, *b"dfrm"),
        (Self::SupportedProperties, *b"dsup"),
        (Self::StartTileSession, *b"dsts"),
        (Self::DecodeTile, *b"dtil"),
        (Self::FinishDelayedTiles, *b"dtfi"),
        (Self::FinishDelayedFrames, *b"dfin"),
        (Self::CanAcceptFormatDescription, *b"dcan"),
        (Self::Invalidate, *b"dinv"),
        (Self::SetProperties, *b"dmul"),
        (Self::SerializableProperties, *b"dsav"),
    ];

    /// The decoder message `message_type` names, if it is one.
    pub(crate) fn from_fourcc(message_type: FourCc) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|(_, code)| FourCc::new(*code) == message_type)
            .map(|(message, _)| *message)
    }
}

/// `Dpba`: the host decoder sets the guest's pixel buffer attributes.
pub(crate) const SET_PIXEL_BUFFER_ATTRIBUTES: FourCc = FourCc::new(*b"Dpba");
/// `Dbuf`: the host decoder asks the guest for a pixel buffer.
pub(crate) const CREATE_PIXEL_BUFFER: FourCc = FourCc::new(*b"Dbuf");
/// `Demt`: the host decoder emits a decoded frame.
pub(crate) const EMIT_DECODED_FRAME: FourCc = FourCc::new(*b"Demt");
/// `Dfub`: the host decoder lets go of pixel buffers it did not emit.
pub(crate) const FREE_UNUSED_PIXEL_BUFFERS: FourCc = FourCc::new(*b"Dfub");

/// `,fds`: a format description, `=fds`.
pub(crate) const FORMAT_DESCRIPTION: FourCc = FourCc::new(*b",fds");
/// `,blu`: whether the guest makes pixel buffers from a pool, `=s32`.
pub(crate) const GUEST_POOL: FourCc = FourCc::new(*b",blu");
/// `,frm`: a frame's identifier, `=s64`.
pub(crate) const FRAME: FourCc = FourCc::new(*b",frm");
/// `,sbf`: a sample buffer, `=sbf`.
pub(crate) const SAMPLE_BUFFER: FourCc = FourCc::new(*b",sbf");
/// `,dfl`: the decode flags, `=u32`.
pub(crate) const DECODE_FLAGS: FourCc = FourCc::new(*b",dfl");
/// `,fop`: a frame's options, `=pls`.
pub(crate) const FRAME_OPTIONS: FourCc = FourCc::new(*b",fop");
/// `,inf`: the decode info flags, `=u32`.
pub(crate) const INFO_FLAGS: FourCc = FourCc::new(*b",inf");
/// `,uui`: a pixel buffer's UUID, `=uui`.
pub(crate) const PIXEL_BUFFER_UUID: FourCc = FourCc::new(*b",uui");
/// `,ui+`: pixel buffers' UUIDs, `=ui+`.
pub(crate) const PIXEL_BUFFER_UUIDS: FourCc = FourCc::new(*b",ui+");
/// `,pba`: pixel buffer attributes, `=pls`.
pub(crate) const PIXEL_BUFFER_ATTRIBUTES: FourCc = FourCc::new(*b",pba");
/// `,key`: a property's key, `=pls`.
pub(crate) const PROPERTY_KEY: FourCc = FourCc::new(*b",key");
/// `,val`: a property's value, `=pls`.
pub(crate) const PROPERTY_VALUE: FourCc = FourCc::new(*b",val");
/// `,sup`: the supported properties, `=pls`.
pub(crate) const SUPPORTED_PROPERTIES: FourCc = FourCc::new(*b",sup");
/// `,pro`: properties, `=pls`.
pub(crate) const PROPERTIES: FourCc = FourCc::new(*b",pro");
/// `,can`: whether a format description can be taken, `=s32`.
pub(crate) const CAN_ACCEPT: FourCc = FourCc::new(*b",can");

/// A message for an encoder session, from the guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EncoderMessage {
    /// `ecop`: copy a property.
    CopyProperty,
    /// `eset`: set a property.
    SetProperty,
    /// `esta`: start the session with the picture's dimensions.
    StartSession,
    /// `epfr`: prepare to encode frames.
    PrepareToEncodeFrames,
    /// `efrm`: encode a frame.
    EncodeFrame,
    /// `emif`: encode a multi-image frame.
    EncodeMultiImageFrame,
    /// `ecfr`: complete the frames up to a time stamp.
    CompleteFrames,
    /// `ebps`: begin a pass.
    BeginPass,
    /// `eeps`: end a pass.
    EndPass,
    /// `esup`: the supported properties.
    SupportedProperties,
    /// `ests`: start a tile session.
    StartTileSession,
    /// `epti`: prepare to encode tiles.
    PrepareToEncodeTiles,
    /// `etil`: encode a tile.
    EncodeTile,
    /// `ecti`: complete the tiles.
    CompleteTiles,
    /// `einv`: invalidate the encoder.
    Invalidate,
    /// `emul`: set several properties.
    SetProperties,
    /// `esav`: the serializable properties.
    SerializableProperties,
}

impl EncoderMessage {
    const ALL: [(Self, [u8; 4]); 17] = [
        (Self::CopyProperty, *b"ecop"),
        (Self::SetProperty, *b"eset"),
        (Self::StartSession, *b"esta"),
        (Self::PrepareToEncodeFrames, *b"epfr"),
        (Self::EncodeFrame, *b"efrm"),
        (Self::EncodeMultiImageFrame, *b"emif"),
        (Self::CompleteFrames, *b"ecfr"),
        (Self::BeginPass, *b"ebps"),
        (Self::EndPass, *b"eeps"),
        (Self::SupportedProperties, *b"esup"),
        (Self::StartTileSession, *b"ests"),
        (Self::PrepareToEncodeTiles, *b"epti"),
        (Self::EncodeTile, *b"etil"),
        (Self::CompleteTiles, *b"ecti"),
        (Self::Invalidate, *b"einv"),
        (Self::SetProperties, *b"emul"),
        (Self::SerializableProperties, *b"esav"),
    ];

    /// The encoder message `message_type` names, if it is one.
    pub(crate) fn from_fourcc(message_type: FourCc) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|(_, code)| FourCc::new(*code) == message_type)
            .map(|(message, _)| *message)
    }
}

/// `Epba`: the host encoder sets the guest's source pixel buffer
/// attributes.
pub(crate) const SET_SOURCE_PIXEL_BUFFER_ATTRIBUTES: FourCc = FourCc::new(*b"Epba");
/// `Efds`: the host encoder asks the guest for a format description.
pub(crate) const CREATE_VIDEO_FORMAT_DESCRIPTION: FourCc = FourCc::new(*b"Efds");
/// `Eemt`: the host encoder emits an encoded frame.
pub(crate) const EMIT_ENCODED_FRAME: FourCc = FourCc::new(*b"Eemt");
/// `mpin`: the host encoder's multi-pass storage asks the guest to
/// invalidate its own, as the host's is released.
pub(crate) const MULTI_PASS_STORAGE_INVALIDATE: FourCc = FourCc::new(*b"mpin");

/// `,vdi`: the picture's dimensions, `=wxh`.
pub(crate) const DIMENSIONS: FourCc = FourCc::new(*b",vdi");
/// `,pts`: a presentation time stamp, `=tim`.
pub(crate) const PRESENTATION_TIME: FourCc = FourCc::new(*b",pts");
/// `,dur`: a duration, `=tim`.
pub(crate) const DURATION: FourCc = FourCc::new(*b",dur");
/// `,fpr`: a frame's properties, `=pls`; in `eeps`'s reply, whether more
/// passes are wanted, `=s32`.
pub(crate) const FRAME_PROPERTIES: FourCc = FourCc::new(*b",fpr");
/// `,pat`: a pixel buffer's attachments, `=pls`.
pub(crate) const PIXEL_BUFFER_ATTACHMENTS: FourCc = FourCc::new(*b",pat");
/// `,efl`: the encode info flags of an emission, `=u32`.
pub(crate) const ENCODE_INFO_FLAGS: FourCc = FourCc::new(*b",efl");
/// `,bfl`: a pass's begin flags, `=u32`.
pub(crate) const BEGIN_PASS_FLAGS: FourCc = FourCc::new(*b",bfl");
/// `,esp`: an encoder specification, `=pls`.
pub(crate) const ENCODER_SPECIFICATION: FourCc = FourCc::new(*b",esp");
/// `,twh`: a tile's dimensions, `=32s`.
pub(crate) const TILE_DIMENSIONS: FourCc = FourCc::new(*b",twh");
/// `,ept`: whether tiles are coming, `=s32`.
pub(crate) const TILE_COUNT: FourCc = FourCc::new(*b",ept");
/// `,til`: a tile's identifier, `=s64`.
pub(crate) const TILE: FourCc = FourCc::new(*b",til");
/// `,tio`: a tile's offset, `=32p`.
pub(crate) const TILE_OFFSET: FourCc = FourCc::new(*b",tio");
/// `,tia`: a tile's aperture, `=32s`.
pub(crate) const TILE_APERTURE: FourCc = FourCc::new(*b",tia");
/// `,tip`: a tile's properties, `=pls`.
pub(crate) const TILE_PROPERTIES: FourCc = FourCc::new(*b",tip");
/// `,wdt`: a width, `=s32`.
pub(crate) const WIDTH: FourCc = FourCc::new(*b",wdt");
/// `,hgt`: a height, `=s32`.
pub(crate) const HEIGHT: FourCc = FourCc::new(*b",hgt");
/// `,ext`: a format description's extensions, `=pls`.
pub(crate) const EXTENSIONS: FourCc = FourCc::new(*b",ext");
/// `=fds`: the format description in `Efds`'s reply, whose key is its
/// value type.
pub(crate) const CREATED_FORMAT_DESCRIPTION: FourCc = FourCc::new(*b"=fds");

/// A message for a motion-estimation session, from the guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MotionMessage {
    /// `mest`: start the session with the pictures' dimensions.
    StartSession,
    /// `mepf`: estimate the motion between two frames.
    ProcessFrames,
    /// `mcfr`: complete the frames in flight.
    CompleteFrames,
    /// `mcop`: copy a property.
    CopyProperty,
    /// `mset`: set a property.
    SetProperty,
    /// `mmul`: set several properties.
    SetProperties,
    /// `msav`: the serializable properties.
    SerializableProperties,
    /// `minv`: invalidate the processor.
    Invalidate,
    /// `mesu`: the supported properties, which Apple's host never answers.
    SupportedProperties,
}

impl MotionMessage {
    const ALL: [(Self, [u8; 4]); 9] = [
        (Self::StartSession, *b"mest"),
        (Self::ProcessFrames, *b"mepf"),
        (Self::CompleteFrames, *b"mcfr"),
        (Self::CopyProperty, *b"mcop"),
        (Self::SetProperty, *b"mset"),
        (Self::SetProperties, *b"mmul"),
        (Self::SerializableProperties, *b"msav"),
        (Self::Invalidate, *b"minv"),
        (Self::SupportedProperties, *b"mesu"),
    ];

    /// The motion-estimation message `message_type` names, if it is one.
    pub(crate) fn from_fourcc(message_type: FourCc) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|(_, code)| FourCc::new(*code) == message_type)
            .map(|(message, _)| *message)
    }
}

/// `Mspa`: the host processor sets the guest's source pixel buffer
/// attributes.
pub(crate) const SET_MOTION_SOURCE_ATTRIBUTES: FourCc = FourCc::new(*b"Mspa");
/// `Mvpb`: the host processor sets the guest's motion vector pixel buffer
/// attributes.
pub(crate) const SET_MOTION_VECTOR_ATTRIBUTES: FourCc = FourCc::new(*b"Mvpb");
/// `Mbuf`: the host processor asks the guest for a motion vector buffer.
pub(crate) const CREATE_MOTION_VECTOR_BUFFER: FourCc = FourCc::new(*b"Mbuf");
/// `Memt`: the host processor emits a frame's motion vectors.
pub(crate) const EMIT_MOTION_VECTORS: FourCc = FourCc::new(*b"Memt");
/// `Mdpb`: the host processor tells the guest which of its pixel buffers
/// it has let go.
pub(crate) const DROPPED_PIXEL_BUFFERS: FourCc = FourCc::new(*b"Mdpb");

/// `,meo`: a motion-estimation processor's specification, or a frame's
/// options, `=pls`.
pub(crate) const MOTION_OPTIONS: FourCc = FourCc::new(*b",meo");
/// `,rid`: a motion-estimation request's identifier, `=s64`.
pub(crate) const REQUEST: FourCc = FourCc::new(*b",rid");
/// `,mef`: a motion-estimation frame's flags, `=u32`.
pub(crate) const MOTION_FRAME_FLAGS: FourCc = FourCc::new(*b",mef");
