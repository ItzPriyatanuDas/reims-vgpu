//! The decoding process above the tile data: temporal units and their
//! OBUs, sequence headers and their changes, frame headers, the eight
//! reference slots and their update (7.20, 7.21), and which frames are
//! output. What decodes the tiles is an [`Accelerator`]: a backend's, for
//! the hardware it drives.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::Error;
use super::av1c::ConfigurationRecord;
use super::frame::{FrameHeader, FrameType, NUM_REF_FRAMES, PRIMARY_REF_NONE, ReferenceState};
use super::obu::{Obu, ObuType, obus};
use super::sequence::SequenceHeader;
use super::tile_group::TileGroup;

/// What decodes the tiles: the hardware's half of a stateless decoder.
pub trait Accelerator {
    /// A decoded frame's storage, such as a hardware decoder's surface.
    type Picture;
    /// The accelerator's error, which carries this module's errors too.
    type Error: From<Error>;

    /// Prepares for the frames of `sequence`. Called before a sequence's
    /// first frame, and again at the key frame that starts a new sequence
    /// whose frames need other storage: another size, bit depth, profile,
    /// colour format or film grain. `first` is the frame it is called for,
    /// for an accelerator that cannot hold the largest frame the sequence
    /// allows and sizes its storage by the frames it has instead.
    fn configure(
        &mut self,
        sequence: &SequenceHeader,
        first: &FrameHeader,
    ) -> Result<(), Self::Error>;

    /// Storage for a frame as decoded: what later frames predict from.
    fn new_picture(&mut self) -> Result<Self::Picture, Self::Error>;

    /// Storage for a frame with its film grain applied: what is shown.
    fn new_display_picture(&mut self) -> Result<Self::Picture, Self::Error>;

    /// Decodes one frame from all its tile groups into `params.picture`,
    /// and into `params.display` with the film grain applied when the
    /// header applies grain.
    fn decode_frame(&mut self, params: &FrameParams<'_, Self::Picture>) -> Result<(), Self::Error>;
}

/// An accelerator that decodes nothing: a [`Decoder`] over it reads a
/// stream's headers and follows its reference slots alone, for a backend
/// whose own decoder does not say which frames a temporal unit carried
/// ([`Decoder::steps`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct FramesOnly;

impl Accelerator for FramesOnly {
    type Picture = ();
    type Error = Error;

    fn configure(&mut self, _: &SequenceHeader, _: &FrameHeader) -> Result<(), Error> {
        Ok(())
    }

    fn new_picture(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn new_display_picture(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn decode_frame(&mut self, _: &FrameParams<'_, ()>) -> Result<(), Error> {
        Ok(())
    }
}

/// What the accelerator is given for one frame.
pub struct FrameParams<'a, P> {
    /// The sequence header in force.
    pub sequence: &'a SequenceHeader,
    /// The frame's header.
    pub header: &'a FrameHeader,
    /// Where the frame is decoded to.
    pub picture: &'a P,
    /// Where the frame with its film grain goes, when the header applies
    /// grain.
    pub display: Option<&'a P>,
    /// The frame each reference slot holds (`ref_frame_map`), as decoded.
    pub references: [Option<&'a P>; NUM_REF_FRAMES],
    /// The frame's tile groups, every tile of it once.
    pub tile_groups: &'a [TileGroup<'a>],
}

/// A frame the decoder outputs.
pub struct Output<P> {
    /// The picture to show: with its film grain, where the frame has some.
    pub picture: Arc<P>,
    /// The decoded frame's own number ([`Step::picture_id`]): the same for
    /// a frame shown again as when it was decoded.
    pub(crate) picture_id: u64,
    /// The id the temporal unit that showed the frame was decoded with.
    pub id: u64,
    /// `UpscaledWidth`: the frame's width.
    pub width: u32,
    /// `FrameHeight`.
    pub height: u32,
    /// The sequence's bit depth: 8, 10 or 12.
    pub bit_depth: u8,
    /// The sequence's `color_range`: full range when set.
    pub full_range: bool,
    /// The frame's type.
    pub frame_type: FrameType,
    /// Whether a `show_existing_frame` header showed it, rather than its
    /// own decode.
    pub show_existing_frame: bool,
    /// Its `OrderHint`.
    pub order_hint: u32,
    /// The spatial layer it is of: 0 without an OBU extension header.
    pub(crate) spatial_id: u8,
}

/// One frame of a temporal unit, as the decoder took it: decoded, or shown
/// again from its slot (`show_existing_frame`), in the order the unit
/// carries them, the frames of every spatial layer among them. Ours: what a
/// host lends its guest a buffer for, as Apple's AV1 decoder lends one for
/// each frame it decodes and each it shows again, and holds while the
/// frame is referenced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// The frame's number, unique in the process: the frame decoded, or
    /// the one shown again. A decoder numbers each frame it decodes anew.
    pub(crate) picture_id: u64,
    /// Whether a `show_existing_frame` header showed the frame again,
    /// rather than this step decoding it.
    pub(crate) shown_again: bool,
    /// `UpscaledWidth`: the frame's width.
    pub(crate) width: u32,
    /// `FrameHeight`.
    pub(crate) height: u32,
    /// The spatial layer the frame is of: 0 without an OBU extension
    /// header.
    pub(crate) spatial_id: u8,
    /// The frame each reference slot holds after this step, by number.
    pub(crate) references: [Option<u64>; NUM_REF_FRAMES],
    /// The frame whose saved state the frame loads, by number: the slot
    /// `ref_frame_idx[primary_ref_frame]` named, for an inter frame whose
    /// `primary_ref_frame` is not `PRIMARY_REF_NONE`.
    pub(crate) primary_reference: Option<u64>,
    /// Whether the decode outputs the frame: the frame the unit shows of
    /// its highest spatial layer ([`Decoder::decode`]).
    pub(crate) output: bool,
    /// The largest frame the sequence in force codes:
    /// `max_frame_width_minus_1 + 1` by `max_frame_height_minus_1 + 1`.
    pub(crate) largest: (u32, u32),
}

impl From<&Step> for reims_vgpu_video_core::backend::FrameDecoded {
    fn from(step: &Step) -> Self {
        Self {
            picture: step.picture_id,
            shown_again: step.shown_again,
            refused: false,
            width: step.width,
            height: step.height,
            references: step.references,
            primary_reference: step.primary_reference,
            output: step.output,
            largest: step.largest,
        }
    }
}

/// The next frame number: a frame's number is unique in the process, so
/// that a decoder made anew, as at a new `av1C`, numbers no frame as one
/// before it did.
static NEXT_PICTURE_ID: AtomicU64 = AtomicU64::new(1);

/// The frame a reference slot holds.
struct Slot<P> {
    picture: Arc<P>,
    display: Option<Arc<P>>,
    /// Its number ([`Step::picture_id`]).
    id: u64,
}

impl<P> Clone for Slot<P> {
    fn clone(&self) -> Self {
        Self {
            picture: self.picture.clone(),
            display: self.display.clone(),
            id: self.id,
        }
    }
}

/// A frame whose header has come and whose tiles are coming.
struct PendingFrame<'a> {
    header: FrameHeader,
    tile_groups: Vec<TileGroup<'a>>,
}

/// An AV1 decoder over an [`Accelerator`], one temporal unit per sample.
///
/// Samples are in the low overhead bitstream format (5.2), as MP4
/// samples and IVF frames are: OBUs with their sizes. Operating point 0
/// is decoded: every frame of a sample in its layers. Each frame of the
/// highest spatial layer the sample shows a frame of is output, in the
/// order it shows them; a lower layer's frame is decoded as a reference
/// alone. Large scale tile decoding is refused with
/// [`Error::Unsupported`].
pub struct Decoder<A: Accelerator> {
    accelerator: A,
    /// The sequence header in force.
    sequence: Option<Arc<SequenceHeader>>,
    /// A sequence header that differs from the one in force, waiting for
    /// the key frame that starts it.
    pending_sequence: Option<Arc<SequenceHeader>>,
    /// Whether the accelerator is configured for `sequence`.
    configured: bool,
    refs: [ReferenceState; NUM_REF_FRAMES],
    slots: [Option<Slot<A::Picture>>; NUM_REF_FRAMES],
    /// The frames the last [`Decoder::decode`] decoded or showed again.
    steps: Vec<Step>,
}

impl<A: Accelerator> Decoder<A> {
    /// A decoder whose samples carry their own sequence headers.
    pub fn new(accelerator: A) -> Self {
        Self {
            accelerator,
            sequence: None,
            pending_sequence: None,
            configured: false,
            refs: [ReferenceState::default(); NUM_REF_FRAMES],
            slots: Default::default(),
            steps: Vec::new(),
        }
    }

    /// A decoder for samples described by an `av1C` record, whose
    /// sequence header, when it carries one, is in force from the start.
    pub fn from_av1c(accelerator: A, record: &[u8]) -> Result<Self, A::Error> {
        let mut decoder = Self::new(accelerator);
        let record = ConfigurationRecord::parse(record)?;
        if let Some(payload) = record.sequence_header_obu()? {
            decoder.sequence = Some(Arc::new(SequenceHeader::parse(payload)?));
        }
        Ok(decoder)
    }

    /// The accelerator.
    pub fn accelerator(&self) -> &A {
        &self.accelerator
    }

    /// The sequence header in force, once one has come.
    pub fn sequence(&self) -> Option<&SequenceHeader> {
        self.sequence.as_deref()
    }

    /// The frames the last [`Decoder::decode`] decoded or showed again, in
    /// order, those before an error included.
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// Forgets every reference, as for a seek: decoding starts again at
    /// the next key frame.
    pub fn reset(&mut self) {
        self.refs = [ReferenceState::default(); NUM_REF_FRAMES];
        self.slots = Default::default();
    }

    /// Decodes one temporal unit, tagged `id`, and returns the frames it
    /// shows of its highest spatial layer, in order: usually one. The
    /// decodes may still be running in the accelerator.
    pub fn decode(&mut self, sample: &[u8], id: u64) -> Result<Vec<Output<A::Picture>>, A::Error> {
        self.steps.clear();
        let mut outputs = Vec::new();
        let mut pending: Option<PendingFrame<'_>> = None;
        // The spatial layers that have shown a frame, a bit each.
        let mut shown = 0u8;
        for obu in obus(sample) {
            let obu = obu?;
            let operating_point = self
                .pending_sequence
                .as_deref()
                .or(self.sequence.as_deref())
                .map_or(0, |s| s.operating_point().idc);
            let always_kept = matches!(
                obu.obu_type,
                ObuType::SequenceHeader | ObuType::TemporalDelimiter
            );
            if !always_kept && !obu.in_operating_point(operating_point) {
                continue;
            }
            match obu.obu_type {
                ObuType::TemporalDelimiter => {
                    if pending.is_some() {
                        return Err(Error::Truncated.into());
                    }
                }
                ObuType::SequenceHeader => self.sequence_header(obu.payload)?,
                ObuType::FrameHeader | ObuType::RedundantFrameHeader => {
                    // While a frame's tiles are coming, a frame header is
                    // a copy of its own (frame_header_copy()); a redundant
                    // one with no frame in progress repeats a finished one.
                    if pending.is_some() || obu.obu_type == ObuType::RedundantFrameHeader {
                        continue;
                    }
                    pending = self.frame_header(&obu, id, &mut shown, &mut outputs)?.map(
                        |(header, _)| PendingFrame {
                            header,
                            tile_groups: Vec::new(),
                        },
                    );
                }
                ObuType::Frame => {
                    if pending.is_some() {
                        return Err(Error::FrameInsideFrame.into());
                    }
                    let Some((header, used)) =
                        self.frame_header(&obu, id, &mut shown, &mut outputs)?
                    else {
                        return Err(Error::ShownExistingFrameInFrameObu.into());
                    };
                    let tiles = obu.payload.get(used..).ok_or(Error::Truncated)?;
                    let group = TileGroup::parse(tiles, &header.tile_info)?;
                    // 6.10.1: a frame OBU's tile group starts at tile 0, as
                    // a first tile group OBU must, so that no tile before
                    // tg_start goes undecoded.
                    if group.tg_start != 0 {
                        return Err(Error::OutOfRange("tg_start").into());
                    }
                    let last = group.is_last(&header.tile_info);
                    let mut frame = PendingFrame {
                        header,
                        tile_groups: vec![group],
                    };
                    if last {
                        self.finish_frame(&mut frame, id, &mut outputs)?;
                    } else {
                        pending = Some(frame);
                    }
                }
                ObuType::TileGroup => {
                    let Some(frame) = pending.as_mut() else {
                        return Err(Error::TileGroupWithoutFrameHeader.into());
                    };
                    let group = TileGroup::parse(obu.payload, &frame.header.tile_info)?;
                    // The group starts at the tile after the last group's.
                    let follows = match frame.tile_groups.last() {
                        Some(last) => last.tg_end.checked_add(1) == Some(group.tg_start),
                        None => group.tg_start == 0,
                    };
                    if !follows {
                        return Err(Error::OutOfRange("tg_start").into());
                    }
                    let last = group.is_last(&frame.header.tile_info);
                    frame.tile_groups.push(group);
                    if last && let Some(mut frame) = pending.take() {
                        self.finish_frame(&mut frame, id, &mut outputs)?;
                    }
                }
                ObuType::TileList => {
                    return Err(Error::Unsupported("large scale tile decoding").into());
                }
                ObuType::Metadata | ObuType::Padding | ObuType::Reserved(_) => {}
            }
        }
        if pending.is_some() {
            // A frame's tiles all come in its temporal unit (7.5).
            return Err(Error::Truncated.into());
        }
        // A unit of several spatial layers shows the top one, as Apple's
        // host and libdav1d (ffmpeg's, all_layers 0) do on libaom's
        // scalable streams; where the top layer is missing from a unit,
        // the highest it has.
        if let Some(top) = outputs.iter().map(|o| o.spatial_id).max() {
            outputs.retain(|o| o.spatial_id == top);
        }
        // Each output is the step that showed it: the last step of its
        // picture and layer, as a unit shows one frame of a layer.
        for output in &outputs {
            if let Some(step) = self.steps.iter_mut().rev().find(|step| {
                step.picture_id == output.picture_id && step.spatial_id == output.spatial_id
            }) {
                step.output = true;
            }
        }
        Ok(outputs)
    }

    /// Records a step: the frame `picture_id` of `header`, decoded or shown
    /// again, with the slots as they are now.
    fn record_step(
        &mut self,
        picture_id: u64,
        header: &FrameHeader,
        primary_reference: Option<u64>,
        largest: (u32, u32),
    ) {
        let references = self
            .slots
            .each_ref()
            .map(|slot| slot.as_ref().map(|slot| slot.id));
        self.steps.push(Step {
            picture_id,
            shown_again: header.show_existing_frame,
            width: header.upscaled_width,
            height: header.frame_height,
            spatial_id: header.spatial_id,
            references,
            primary_reference,
            output: false,
            largest,
        });
    }

    /// A sequence header OBU: in force at once when it is the first, or
    /// says what the one in force says; otherwise waiting for its key
    /// frame.
    fn sequence_header(&mut self, payload: &[u8]) -> Result<(), A::Error> {
        let sequence = SequenceHeader::parse(payload)?;
        if self.sequence.as_deref() == Some(&sequence) {
            self.pending_sequence = None;
            return Ok(());
        }
        if self.sequence.is_none() {
            self.sequence = Some(Arc::new(sequence));
        } else {
            self.pending_sequence = Some(Arc::new(sequence));
        }
        Ok(())
    }

    /// Parses a frame header OBU's header. A shown existing frame is
    /// output and its slot's refresh done here; any other frame comes
    /// back, with the bytes its header took, to wait for its tiles.
    /// `shown` holds the spatial layers the temporal unit has shown a
    /// frame of.
    fn frame_header(
        &mut self,
        obu: &Obu<'_>,
        id: u64,
        shown: &mut u8,
        outputs: &mut Vec<Output<A::Picture>>,
    ) -> Result<Option<(FrameHeader, usize)>, A::Error> {
        let sequence = self
            .pending_sequence
            .clone()
            .or_else(|| self.sequence.clone())
            .ok_or(Error::MissingSequenceHeader)?;
        let mut refs = self.refs;
        let (header, used) = FrameHeader::parse(
            obu.payload,
            &sequence,
            &mut refs,
            obu.temporal_id(),
            obu.spatial_id(),
        )?;
        // 7.5: a temporal unit shows one frame of each spatial layer at
        // most, which a sample is; checked before anything changes.
        if header.show_existing_frame || header.show_frame {
            let layer = 1u8 << (header.spatial_id & 7);
            if *shown & layer != 0 {
                return Err(Error::SecondShownFrame.into());
            }
            *shown |= layer;
        }
        if self.pending_sequence.is_some() {
            if header.show_existing_frame
                || header.frame_type != FrameType::Key
                || !header.show_frame
            {
                return Err(Error::SequenceWithoutKeyFrame.into());
            }
            let previous = self.sequence.replace(sequence.clone());
            self.pending_sequence = None;
            if previous.is_none_or(|p| needs_configure(&p, &sequence)) {
                self.configured = false;
            }
        }
        if header.show_existing_frame {
            let index = usize::from(header.frame_to_show_map_idx);
            let slot = self
                .slots
                .get(index)
                .and_then(Option::as_ref)
                .cloned()
                .ok_or(Error::MissingReference)?;
            if header.frame_type == FrameType::Key {
                // The frame loading process (7.21), then the update of
                // every slot (7.20).
                let mut state = *refs.get(index).ok_or(Error::MissingReference)?;
                state.key_shown = true;
                refs = [state; NUM_REF_FRAMES];
                self.slots = core::array::from_fn(|_| Some(slot.clone()));
            }
            self.refs = refs;
            self.record_step(slot.id, &header, None, sequence.max_frame_size());
            outputs.push(Output {
                picture_id: slot.id,
                picture: slot.display.unwrap_or(slot.picture),
                id,
                width: header.upscaled_width,
                height: header.frame_height,
                bit_depth: sequence.color.bit_depth,
                full_range: sequence.color.color_range,
                frame_type: header.frame_type,
                show_existing_frame: true,
                order_hint: header.order_hint,
                spatial_id: header.spatial_id,
            });
            return Ok(None);
        }
        if !self.configured {
            self.accelerator.configure(&sequence, &header)?;
            self.configured = true;
        }
        self.refs = refs;
        Ok(Some((header, used)))
    }

    /// Decodes a frame whose tiles have all come, stores it in the slots
    /// its header refreshes, and outputs it if it is shown.
    fn finish_frame(
        &mut self,
        frame: &mut PendingFrame<'_>,
        id: u64,
        outputs: &mut Vec<Output<A::Picture>>,
    ) -> Result<(), A::Error> {
        let sequence = self.sequence.clone().ok_or(Error::MissingSequenceHeader)?;
        let header = &frame.header;
        let picture = Arc::new(self.accelerator.new_picture()?);
        let display = if header.film_grain.apply_grain {
            Some(Arc::new(self.accelerator.new_display_picture()?))
        } else {
            None
        };
        let references = self
            .slots
            .each_ref()
            .map(|slot| slot.as_ref().map(|slot| &*slot.picture));
        let primary_reference = (!header.is_intra()
            && header.primary_ref_frame != PRIMARY_REF_NONE)
            .then(|| {
                header
                    .ref_frame_idx
                    .get(usize::from(header.primary_ref_frame))
            })
            .flatten()
            .and_then(|&slot| self.slots.get(usize::from(slot))?.as_ref())
            .map(|slot| slot.id);
        self.accelerator.decode_frame(&FrameParams {
            sequence: &sequence,
            header,
            picture: &picture,
            display: display.as_deref(),
            references,
            tile_groups: &frame.tile_groups,
        })?;
        let picture_id = NEXT_PICTURE_ID.fetch_add(1, Ordering::Relaxed);
        let state = header.reference_state();
        for (i, (kept, slot)) in (0u8..).zip(self.refs.iter_mut().zip(&mut self.slots)) {
            if header.refresh_frame_flags & (1 << i) != 0 {
                *kept = state;
                *slot = Some(Slot {
                    picture: picture.clone(),
                    display: display.clone(),
                    id: picture_id,
                });
            }
        }
        self.record_step(
            picture_id,
            header,
            primary_reference,
            sequence.max_frame_size(),
        );
        if header.show_frame {
            outputs.push(Output {
                picture_id,
                picture: display.unwrap_or(picture),
                id,
                width: header.upscaled_width,
                height: header.frame_height,
                bit_depth: sequence.color.bit_depth,
                full_range: sequence.color.color_range,
                frame_type: header.frame_type,
                show_existing_frame: false,
                order_hint: header.order_hint,
                spatial_id: header.spatial_id,
            });
        }
        Ok(())
    }
}

/// Whether frames of `new` need other storage than those of `old`.
fn needs_configure(old: &SequenceHeader, new: &SequenceHeader) -> bool {
    old.seq_profile != new.seq_profile
        || old.max_frame_size() != new.max_frame_size()
        || old.color.bit_depth != new.color.bit_depth
        || old.color.mono_chrome != new.color.mono_chrome
        || old.color.subsampling_x != new.color.subsampling_x
        || old.color.subsampling_y != new.color.subsampling_y
        || old.film_grain_params_present != new.film_grain_params_present
        || old.use_128x128_superblock != new.use_128x128_superblock
}
