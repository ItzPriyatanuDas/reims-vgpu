//! The decoding process above the compressed header: chunks and their
//! frames, the frame headers and what they inherit, the eight reference
//! slots and their refresh (8.10), shown existing frames, and which frames
//! are output. What decodes a frame from its bytes, the compressed header
//! and the probability contexts included, is an [`Accelerator`], such as
//! VA-API's.
//!
//! Each frame a chunk carries is a [`Step`] ([`Decoder::steps`]): decoded,
//! shown again (`show_existing_frame`), or refused. Apple's VP9 decoder
//! lends a guest buffer for each, those it refuses and those after one in
//! the same chunk among them, and holds a frame's buffer while a slot
//! holds the frame (measured on Apple's host session on the M3 Max, macOS
//! 27.0.1, 2026-10-25, with libvpx's alternate references, spatial layers
//! and dynamic resize, and with shown existing and intra-only frames), so
//! a host lends by them.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::Error;
use super::header::{
    FrameHeader, FrameType, HeaderState, MAX_SEGMENTS, NUM_REF_FRAMES, ReferenceInfo,
    SEG_LVL_REF_FRAME, SEG_LVL_SKIP,
};
use super::quant::{SegmentQuantizers, filter_levels, segment_count, segment_quantizers};
use super::superframe;
use crate::num::ToUsize;

/// What decodes the frames: the hardware's half of a stateless decoder.
pub trait Accelerator {
    /// A decoded frame's storage, such as a VA surface.
    type Picture;
    /// The accelerator's error, which carries this module's errors too.
    type Error: From<Error>;

    /// Prepares for frames of `format`, the first of them `width` by
    /// `height`. Called before the first frame, and again at a key frame
    /// whose format or size differs from those it was last called with,
    /// and at an intra-only frame of another format. A key frame starts
    /// afresh: no frame after it predicts from one before, and every
    /// probability context is reset. An inter frame of another size than
    /// its references is not configured for: its picture is asked for at
    /// its own size, and it predicts from the references at theirs.
    fn configure(
        &mut self,
        format: &StreamFormat,
        width: u32,
        height: u32,
    ) -> Result<(), Self::Error>;

    /// Storage for a frame of `width` by `height`.
    fn new_picture(&mut self, width: u32, height: u32) -> Result<Self::Picture, Self::Error>;

    /// Decodes one frame into `params.picture`.
    fn decode_frame(&mut self, params: &FrameParams<'_, Self::Picture>) -> Result<(), Self::Error>;
}

/// What a frame's samples are: what an accelerator's storage and its
/// driver's profile follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamFormat {
    /// `Profile`.
    pub profile: u8,
    /// `BitDepth`.
    pub bit_depth: u8,
    /// `subsampling_x`.
    pub subsampling_x: bool,
    /// `subsampling_y`.
    pub subsampling_y: bool,
}

impl StreamFormat {
    fn of(header: &FrameHeader) -> Self {
        Self {
            profile: header.profile,
            bit_depth: header.color.bit_depth,
            subsampling_x: header.color.subsampling_x,
            subsampling_y: header.color.subsampling_y,
        }
    }
}

/// What one segment's blocks are decoded with, for an accelerator that
/// does not work it out from the header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SegmentParams {
    /// The quantizer's step sizes.
    pub quantizers: SegmentQuantizers,
    /// The loop filter level by reference and mode (see
    /// [`super::quant::filter_levels`]).
    pub filter_levels: [[u8; 2]; 4],
    /// Whether `SEG_LVL_REF_FRAME` is active.
    pub reference_enabled: bool,
    /// Its reference: 0 INTRA_FRAME, 1 LAST, 2 GOLDEN, 3 ALTREF.
    pub reference: u8,
    /// Whether `SEG_LVL_SKIP` is active.
    pub skip: bool,
}

/// What the accelerator is given for one frame.
pub struct FrameParams<'a, P> {
    /// The frame's uncompressed header, with what it inherits in force.
    pub header: &'a FrameHeader,
    /// The frame's bytes, from its uncompressed header to its last tile.
    pub data: &'a [u8],
    /// Where the frame is decoded to.
    pub picture: &'a P,
    /// The frame each reference slot holds (`ref_frame_map`).
    pub references: [Option<&'a P>; NUM_REF_FRAMES],
    /// Each segment's parameters: all eight with segmentation, the first
    /// alone without, the others then zero.
    pub segments: [SegmentParams; MAX_SEGMENTS],
}

/// A frame the decoder outputs.
pub struct Output<P> {
    /// The picture to show.
    pub picture: Arc<P>,
    /// The id the chunk that showed the frame was decoded with.
    pub id: u64,
    /// `FrameWidth` of the frame shown.
    pub width: u32,
    /// `FrameHeight`.
    pub height: u32,
    /// Its bit depth: 8 or 10.
    pub bit_depth: u8,
    /// Its `color_range`: full range when set.
    pub full_range: bool,
    /// Its frame type.
    pub frame_type: FrameType,
    /// Whether a `show_existing_frame` header showed it, rather than its
    /// own decode.
    pub show_existing_frame: bool,
    /// Whether it is stored in a reference slot.
    pub reference: bool,
    /// Frames shown since the last key frame, this one not counted: its
    /// place in display order.
    pub display_index: u32,
}

/// One frame of a chunk, as the decoder took it, in the order the chunk
/// carries them: decoded, shown again from its slot
/// (`show_existing_frame`), or refused. Ours: what a host lends its guest a
/// buffer for, as Apple's VP9 decoder lends one for each frame it decodes,
/// shows again or refuses, and holds while a slot holds the frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// The frame's number, unique in the process: the frame decoded or
    /// refused, or the one shown again.
    pub(crate) picture_id: u64,
    /// Whether a `show_existing_frame` header showed the frame again,
    /// rather than this step decoding it.
    pub(crate) shown_again: bool,
    /// Whether the frame was refused, and not decoded: one with an error,
    /// one after it in its chunk, or one after a frame that failed
    /// ([`Error::Unrecovered`]). No slot holds a frame after it, as no
    /// frame decodes before the next key frame, which refreshes them all.
    pub(crate) refused: bool,
    /// `FrameWidth`: the frame's, or the frame's shown again; 0 for a
    /// frame refused before its size was read.
    pub(crate) width: u32,
    /// `FrameHeight`.
    pub(crate) height: u32,
    /// The frame each reference slot holds after this step, by number:
    /// none after a frame refused.
    pub(crate) references: [Option<u64>; NUM_REF_FRAMES],
    /// Whether the decode outputs the frame: each frame shown of a chunk
    /// that decodes whole.
    pub(crate) output: bool,
}

impl From<&Step> for reims_vgpu_video_core::backend::FrameDecoded {
    /// VP9 codes no largest frame: a step's own size stands for it, so the
    /// first frame asks for nothing beyond the description's size.
    fn from(step: &Step) -> Self {
        Self {
            picture: step.picture_id,
            shown_again: step.shown_again,
            refused: step.refused,
            width: step.width,
            height: step.height,
            references: step.references,
            primary_reference: None,
            output: step.output,
            largest: (step.width, step.height),
        }
    }
}

/// The next frame number: unique in the process, so that a decoder made
/// anew, as at a new `vpcC`, numbers no frame as one before it did.
static NEXT_PICTURE_ID: AtomicU64 = AtomicU64::new(1);

fn next_picture_id() -> u64 {
    NEXT_PICTURE_ID.fetch_add(1, Ordering::Relaxed)
}

/// An accelerator that decodes nothing: a [`Decoder`] over it reads a
/// stream's headers and follows its reference slots alone, for a backend
/// whose own decoder does not say which frames a chunk carried
/// ([`Decoder::steps`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct FramesOnly;

impl Accelerator for FramesOnly {
    type Picture = ();
    type Error = Error;

    fn configure(&mut self, _: &StreamFormat, _: u32, _: u32) -> Result<(), Error> {
        Ok(())
    }

    fn new_picture(&mut self, _: u32, _: u32) -> Result<(), Error> {
        Ok(())
    }

    fn decode_frame(&mut self, _: &FrameParams<'_, ()>) -> Result<(), Error> {
        Ok(())
    }
}

/// The frame a reference slot holds.
struct Slot<P> {
    picture: Arc<P>,
    info: ReferenceInfo,
    frame_type: FrameType,
    /// Its number ([`Step::picture_id`]).
    id: u64,
}

impl<P> Clone for Slot<P> {
    fn clone(&self) -> Self {
        Self {
            picture: self.picture.clone(),
            info: self.info,
            frame_type: self.frame_type,
            id: self.id,
        }
    }
}

/// A VP9 decoder over an [`Accelerator`], one chunk per sample.
///
/// A chunk is what an MP4 sample or an IVF frame holds: one frame, or a
/// superframe of several. Every frame of a chunk is decoded, and each frame
/// it shows is output, in the order it shows them: VP9 has no reordering,
/// so decode order is display order. Profiles 1 and 3 and 12 bits are
/// refused with [`Error::Unsupported`].
///
/// After a frame that does not decode, every frame is refused with
/// [`Error::Unrecovered`], and not decoded, until a key frame decodes, as
/// Apple's VP9 decoder answers each frame after one it refuses -12909,
/// those of the format description's size too, until a key frame of that
/// size decodes (measured on Apple's host session on the M3 Max, macOS
/// 27.0.1, 2026-10-25, with libvpx's dynamic resize and its spatial
/// layers). The frames after one that failed would otherwise predict from
/// the slots it did not refresh. A key frame refreshes every slot and
/// resets everything frames inherit, so nothing before it is wanted.
pub struct Decoder<A: Accelerator> {
    accelerator: A,
    state: HeaderState,
    slots: [Option<Slot<A::Picture>>; NUM_REF_FRAMES],
    /// The format and size the accelerator was last configured for.
    configured: Option<(StreamFormat, u32, u32)>,
    /// Frames shown since the last key frame.
    shown_since_key: u32,
    /// Whether a frame failed since the last key frame decoded, while a
    /// slot held a frame: each frame but a key frame is then refused.
    damaged: bool,
    /// The frames the last [`Decoder::decode`] took ([`Decoder::steps`]).
    steps: Vec<Step>,
}

impl<A: Accelerator> Decoder<A> {
    /// A decoder whose first frame must be a key frame, or an intra-only
    /// one.
    pub fn new(accelerator: A) -> Self {
        Self {
            accelerator,
            state: HeaderState::default(),
            slots: Default::default(),
            configured: None,
            shown_since_key: 0,
            damaged: false,
            steps: Vec::new(),
        }
    }

    /// The accelerator.
    pub fn accelerator(&self) -> &A {
        &self.accelerator
    }

    /// The frames the last [`Decoder::decode`] took, in order, those it
    /// refused included.
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// The frames the last [`Decoder::decode`] took, once: none until the
    /// next decode.
    pub fn take_steps(&mut self) -> Vec<Step> {
        std::mem::take(&mut self.steps)
    }

    /// Forgets every reference and everything frames inherit, as for a
    /// seek: decoding starts again at the next key frame.
    pub fn reset(&mut self) {
        self.state = HeaderState::default();
        self.slots = Default::default();
        self.damaged = false;
    }

    /// Decodes one chunk, tagged `id`, and returns the frames it shows, in
    /// order: usually one, none for a chunk that holds only a hidden frame.
    /// The decodes may still be running in the accelerator.
    ///
    /// A frame with an error is not decoded, and nothing of its header is
    /// kept; the frames of the chunk before it are decoded and stored. Each
    /// frame after it, in this chunk or a later one, is refused with
    /// [`Error::Unrecovered`] until a key frame decodes. A failure before
    /// any slot holds a frame leaves nothing to predict from, so the
    /// frames after it are refused as missing their references until a
    /// key or an intra-only frame decodes, as at the start.
    ///
    /// Its frames are [`Decoder::steps`]: of a chunk with an error, the
    /// frames before it decoded and output none, then that frame and each
    /// after it refused; of one whose superframe index does not read, one
    /// frame refused.
    pub fn decode(&mut self, chunk: &[u8], id: u64) -> Result<Vec<Output<A::Picture>>, A::Error> {
        self.steps.clear();
        let mut outputs = Vec::new();
        let frames = match superframe::frames(chunk) {
            Ok(frames) => frames,
            Err(error) => {
                self.fail(1);
                return Err(error.into());
            }
        };
        let mut left = &frames[..];
        while let Some((frame, after)) = left.split_first() {
            if let Err(error) = self.decode_frame(frame, id, &mut outputs) {
                // This frame and those after it.
                self.fail(left.len());
                return Err(error);
            }
            left = after;
        }
        Ok(outputs)
    }

    /// A chunk's decode that failed with `refused` of its frames left:
    /// they are refused steps, and none of its frames is output.
    fn fail(&mut self, refused: usize) {
        self.mark_failed();
        for step in &mut self.steps {
            step.output = false;
        }
        for _ in 0..refused {
            self.steps.push(Step {
                picture_id: next_picture_id(),
                shown_again: false,
                refused: true,
                width: 0,
                height: 0,
                references: [None; NUM_REF_FRAMES],
                output: false,
            });
        }
    }

    /// The frame each slot holds, by number.
    fn slot_ids(&self) -> [Option<u64>; NUM_REF_FRAMES] {
        self.slots
            .each_ref()
            .map(|slot| slot.as_ref().map(|slot| slot.id))
    }

    /// Takes a frame already decoded as one that failed, as when the
    /// hardware reports so once its decode is done: each frame after it is
    /// refused with [`Error::Unrecovered`] until a key frame decodes, as
    /// after a frame that fails here.
    pub fn mark_failed(&mut self) {
        if self.slots.iter().any(Option::is_some) {
            self.damaged = true;
        }
    }

    fn decode_frame(
        &mut self,
        frame: &[u8],
        id: u64,
        outputs: &mut Vec<Output<A::Picture>>,
    ) -> Result<(), A::Error> {
        let refs = self
            .slots
            .each_ref()
            .map(|slot| slot.as_ref().map(|slot| slot.info));
        let mut state = self.state;
        let header = FrameHeader::parse(frame, &mut state, &refs)?;
        if self.damaged && (header.show_existing_frame || header.frame_type != FrameType::Key) {
            return Err(Error::Unrecovered.into());
        }
        if header.show_existing_frame {
            // 6.2: nothing is decoded, nothing is refreshed, and the frame
            // in the slot is output again.
            let slot = self
                .slots
                .get(usize::from(header.frame_to_show_map_idx))
                .and_then(Option::as_ref)
                .cloned()
                .ok_or(Error::MissingReference)?;
            self.steps.push(Step {
                picture_id: slot.id,
                shown_again: true,
                refused: false,
                width: slot.info.width,
                height: slot.info.height,
                references: self.slot_ids(),
                output: true,
            });
            outputs.push(Output {
                picture: slot.picture,
                id,
                width: slot.info.width,
                height: slot.info.height,
                bit_depth: slot.info.color.bit_depth,
                full_range: slot.info.color.full_range,
                frame_type: slot.frame_type,
                show_existing_frame: true,
                reference: true,
                display_index: self.shown_since_key,
            });
            self.shown_since_key = self.shown_since_key.saturating_add(1);
            return Ok(());
        }
        check_supported(&header)?;
        check_tiles(frame, &header)?;
        let format = StreamFormat::of(&header);
        let (width, height) = (header.width, header.height);
        let reconfigure = match header.frame_type {
            FrameType::Key => self.configured != Some((format, width, height)),
            FrameType::NonKey if header.intra_only => self
                .configured
                .is_none_or(|(configured, ..)| configured != format),
            FrameType::NonKey => match self.configured {
                None => return Err(Error::MissingReference.into()),
                // An inter frame codes its profile, and inherits its depth
                // and subsampling: a profile of another format than the
                // session's would reach a context made for that one.
                Some((configured, ..)) if configured != format => {
                    return Err(Error::InterFrameFormat.into());
                }
                Some(_) => false,
            },
        };
        if reconfigure {
            // An accelerator that refuses a configuration may have dropped
            // the last one, as VA-API's does: until one succeeds there is
            // none, so that the next key frame configures again, whatever
            // its size.
            self.configured = None;
            self.accelerator.configure(&format, width, height)?;
            self.configured = Some((format, width, height));
        }
        let picture = Arc::new(self.accelerator.new_picture(width, height)?);
        let mut segments = [SegmentParams::default(); MAX_SEGMENTS];
        for (index, segment) in segments.iter_mut().enumerate().take(segment_count(&header)) {
            *segment = segment_params(&header, index);
        }
        let references = self
            .slots
            .each_ref()
            .map(|slot| slot.as_ref().map(|slot| &*slot.picture));
        self.accelerator.decode_frame(&FrameParams {
            header: &header,
            data: frame,
            picture: &picture,
            references,
            segments,
        })?;
        self.state = state;
        let picture_id = next_picture_id();
        let slot = Slot {
            picture: picture.clone(),
            info: ReferenceInfo {
                width,
                height,
                color: header.color,
            },
            frame_type: header.frame_type,
            id: picture_id,
        };
        for (i, entry) in self.slots.iter_mut().enumerate() {
            if header.refresh_frame_flags & (1 << i) != 0 {
                *entry = Some(slot.clone());
            }
        }
        if header.frame_type == FrameType::Key {
            self.shown_since_key = 0;
            self.damaged = false;
        }
        self.steps.push(Step {
            picture_id,
            shown_again: false,
            refused: false,
            width,
            height,
            references: self.slot_ids(),
            output: header.show_frame,
        });
        if header.show_frame {
            outputs.push(Output {
                picture,
                id,
                width,
                height,
                bit_depth: header.color.bit_depth,
                full_range: header.color.full_range,
                frame_type: header.frame_type,
                show_existing_frame: false,
                reference: header.refresh_frame_flags != 0,
                display_index: self.shown_since_key,
            });
            self.shown_since_key = self.shown_since_key.saturating_add(1);
        }
        Ok(())
    }
}

/// One segment's parameters, from the header with what it inherits.
fn segment_params(header: &FrameHeader, segment: usize) -> SegmentParams {
    let s = &header.segmentation;
    SegmentParams {
        quantizers: segment_quantizers(header, segment).unwrap_or_default(),
        filter_levels: filter_levels(header, segment),
        reference_enabled: s.feature_active(segment, SEG_LVL_REF_FRAME),
        // feature_value of two bits.
        reference: s
            .feature_data
            .get(segment)
            .and_then(|data| u8::try_from(data[SEG_LVL_REF_FRAME] & 0x3).ok())
            .unwrap_or(0),
        skip: s.feature_active(segment, SEG_LVL_SKIP),
    }
}

/// Checks that the frame's tiles are where their size markers say (6.4):
/// after the compressed header, each tile but the last is a 4-byte
/// big-endian size and that many bytes, and the last takes the rest. Every
/// tile has a byte at least, as a bool decoder needs (ffmpeg's
/// `decode_tiles` refuses one without). The accelerator is handed the
/// frame whole and the hardware reads the markers itself, so they are
/// checked here first.
pub(crate) fn check_tiles(frame: &[u8], header: &FrameHeader) -> Result<(), Error> {
    let start = header.compressed_header_end()?;
    let mut data = frame.get(start..).ok_or(Error::Truncated)?;
    let tiles = (1usize << header.tile_info.rows_log2) << header.tile_info.cols_log2;
    for _ in 1..tiles {
        let size = data
            .first_chunk::<4>()
            .map(|marker| u32::from_be_bytes(*marker).to_usize())
            .ok_or(Error::Truncated)?;
        if size == 0 {
            return Err(Error::OutOfRange("tile_size"));
        }
        data = data
            .get(4..)
            .and_then(|rest| rest.get(size..))
            .ok_or(Error::Truncated)?;
    }
    if data.is_empty() {
        return Err(Error::Truncated);
    }
    Ok(())
}

/// What this decoder refuses of a frame, whatever the accelerator: all but
/// Profiles 0 and 2 (4:4:4, 4:2:2 and 4:4:0 are Profiles 1 and 3, and
/// so is an alpha plane, which VP9 carries beside the frames), and 12
/// bits, for which CoreVideo has no 4:2:0 format and this decoder no
/// tables.
pub fn check_supported(header: &FrameHeader) -> Result<(), Error> {
    match header.profile {
        0 | 2 => {}
        1 => {
            return Err(Error::Unsupported(
                "VP9 Profile 1 (8-bit 4:2:2, 4:4:0, 4:4:4)",
            ));
        }
        _ => {
            return Err(Error::Unsupported(
                "VP9 Profile 3 (high bit depth 4:2:2, 4:4:0, 4:4:4)",
            ));
        }
    }
    let c = &header.color;
    if !(c.subsampling_x && c.subsampling_y) {
        return Err(Error::Unsupported("VP9 chroma other than 4:2:0"));
    }
    if c.bit_depth > 10 {
        return Err(Error::Unsupported("VP9 at 12 bits"));
    }
    Ok(())
}
