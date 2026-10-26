//! Says what an AV1 stream uses, from its headers alone, so that a test
//! stream is known to cover what it is meant to (tools/av1-check.sh): the
//! sequence, the frame types, hidden and shown-existing frames, tiles and
//! tile groups, the loop filters, film grain and superres. It runs the
//! crate's parser over an accelerator that decodes nothing, so it needs no
//! GPU.
//!
//! Before the decode it walks every OBU, so that a stream the decoder
//! refuses is still described: each sequence header's operating points
//! (`operating_point_idc`, the temporal layers in the low byte and the
//! spatial layers above), and the frame and frame header OBUs of each
//! spatial and temporal layer, by their extension headers (`s0t1:30`;
//! `none` where no OBU has one). The decode then counts the global motion
//! models of the frames it decodes, by type (translation, rotzoom,
//! affine), and the warped ones whose shear is not valid (7.11.3.6), which
//! NVDEC is told as `invalid`.
//!
//! The colour is each sequence's `color_config()` (primaries/transfer/
//! matrix as coded, range, chroma sample position), and the last part
//! counts each distinct HDR metadata OBU of the samples: a mastering
//! display prints its primaries (red, green, blue) and white point in 0.16
//! fixed point, then its maximum (24.8) and minimum (18.14) luminance.
//!
//! With `--frames`, it also prints a line for each frame decoded (its
//! type, `show_frame`, `showable_frame`, `refresh_frame_flags`, order hint,
//! size, layer and, for an inter frame, the slots it predicts from) and
//! each frame shown, under its sample's number: how a stream's frames
//! reach the screen.
//!
//! ```sh
//! av1-info stream.ivf
//! av1-info stream.mp4
//! av1-info --frames stream.ivf
//! ```

use reims_vgpu_video_conformance::{ivf, mp4};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use reims_vgpu_video_vaapi::av1::{
    Accelerator, Decoder, Error, FrameParams,
    frame::{FrameHeader, FrameType, GlobalMotion},
    metadata::{self, Metadata},
    obu::{ObuType, obus},
    sequence::SequenceHeader,
};

#[derive(Default)]
struct Stats {
    /// Whether each frame decoded gets a line (`--frames`).
    verbose: bool,
    sequences: Vec<String>,
    frames: usize,
    by_type: [usize; 4],
    hidden: usize,
    error_resilient: usize,
    tiles: BTreeSet<(u32, u32)>,
    max_tile_groups: usize,
    cdef: usize,
    loop_restoration: usize,
    film_grain: usize,
    superres: usize,
    intrabc: usize,
    lossless: usize,
    segmentation: usize,
    delta_q: usize,
    delta_lf: usize,
    screen_content: usize,
    scaled_references: usize,
    slot_sizes: [(u32, u32); 8],
    /// Frames with a global motion model other than identity.
    global_motion_frames: usize,
    /// The models by `GmType`: translation, rotzoom, affine.
    global_motion: [usize; 3],
    /// Rotzoom and affine models whose shear is not valid.
    global_motion_invalid: usize,
    frame_sizes: BTreeSet<(u32, u32, u32)>,
    render_sizes: BTreeSet<(u32, u32)>,
}

impl Accelerator for Stats {
    type Picture = ();
    type Error = Error;

    fn configure(&mut self, s: &SequenceHeader, _first: &FrameHeader) -> Result<(), Error> {
        let c = &s.color;
        let (w, h) = s.max_frame_size();
        self.sequences.push(format!(
            "profile {} level {} {}-bit{} {}x{} max, {} operating points, \
             film grain {}, superres {}, cdef {}, lr {}, order hint {}, \
             reduced still {}, range {}, cp/tc/mc {}/{}/{}, chroma position {}",
            s.seq_profile,
            s.operating_point().seq_level_idx,
            c.bit_depth,
            if c.mono_chrome { " mono" } else { "" },
            w,
            h,
            s.operating_points.len(),
            u8::from(s.film_grain_params_present),
            u8::from(s.enable_superres),
            u8::from(s.enable_cdef),
            u8::from(s.enable_restoration),
            u8::from(s.enable_order_hint),
            u8::from(s.reduced_still_picture_header),
            if c.color_range { "full" } else { "video" },
            c.color_primaries,
            c.transfer_characteristics,
            c.matrix_coefficients,
            c.chroma_sample_position,
        ));
        Ok(())
    }

    fn new_picture(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn new_display_picture(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn decode_frame(&mut self, p: &FrameParams<'_, ()>) -> Result<(), Error> {
        let h = p.header;
        if self.verbose {
            // The slots an inter frame predicts from, LAST to ALTREF.
            let refs = if matches!(h.frame_type, FrameType::Key | FrameType::IntraOnly) {
                String::new()
            } else {
                let slots: Vec<String> = h.ref_frame_idx.iter().map(u8::to_string).collect();
                format!(
                    " refs {} primary {} mvs {}",
                    slots.join(","),
                    h.primary_ref_frame,
                    u8::from(h.use_ref_frame_mvs)
                )
            };
            println!(
                "  decode {:?} show {} showable {} refresh {:#04x} hint {} {}x{} s{}t{}{refs}",
                h.frame_type,
                u8::from(h.show_frame),
                u8::from(h.showable_frame),
                h.refresh_frame_flags,
                h.order_hint,
                h.upscaled_width,
                h.frame_height,
                h.spatial_id,
                h.temporal_id,
            );
        }
        self.frames += 1;
        self.by_type[h.frame_type as usize] += 1;
        self.hidden += usize::from(!h.show_frame);
        self.error_resilient += usize::from(h.error_resilient_mode);
        self.tiles.insert((h.tile_info.cols, h.tile_info.rows));
        self.max_tile_groups = self.max_tile_groups.max(p.tile_groups.len());
        let cdef = &h.cdef;
        let cdef_on = cdef.bits > 0
            || cdef.y_pri_strength[0] | cdef.y_sec_strength[0] != 0
            || cdef.uv_pri_strength[0] | cdef.uv_sec_strength[0] != 0;
        self.cdef += usize::from(cdef_on && !h.coded_lossless && !h.allow_intrabc);
        self.loop_restoration += usize::from(
            h.loop_restoration
                .frame_restoration_type
                .iter()
                .any(|&t| t != 0),
        );
        self.film_grain += usize::from(h.film_grain.apply_grain);
        self.superres += usize::from(h.upscaled_width != h.frame_width);
        self.intrabc += usize::from(h.allow_intrabc);
        self.lossless += usize::from(h.coded_lossless);
        self.segmentation += usize::from(h.segmentation.enabled);
        self.delta_q += usize::from(h.delta_q_present);
        self.delta_lf += usize::from(h.delta_lf_present);
        self.screen_content += usize::from(h.allow_screen_content_tools);
        let size = (h.upscaled_width, h.frame_height);
        if !h.frame_type.is_intra() {
            self.scaled_references += usize::from(
                h.ref_frame_idx
                    .iter()
                    .any(|&i| self.slot_sizes[usize::from(i)] != size),
            );
        }
        for (i, slot) in self.slot_sizes.iter_mut().enumerate() {
            if h.refresh_frame_flags & (1 << i) != 0 {
                *slot = size;
            }
        }
        // The models of LAST_FRAME to ALTREF_FRAME: INTRA_FRAME has none.
        let models: Vec<&GlobalMotion> = h.global_motion[1..]
            .iter()
            .filter(|m| m.kind != 0)
            .collect();
        self.global_motion_frames += usize::from(!models.is_empty());
        for model in models {
            if let Some(count) = self.global_motion.get_mut(usize::from(model.kind) - 1) {
                *count += 1;
            }
            self.global_motion_invalid += usize::from(model.kind >= 2 && !model.shear_is_valid());
        }
        self.frame_sizes
            .insert((h.upscaled_width, h.frame_width, h.frame_height));
        self.render_sizes.insert((h.render_width, h.render_height));
        Ok(())
    }
}

fn run() -> Result<String, String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let verbose = args.first().is_some_and(|a| a == "--frames");
    if verbose {
        args.remove(0);
    }
    let path = args
        .first()
        .cloned()
        .ok_or("usage: av1-info [--frames] <stream.ivf|stream.mp4>")?;
    let stats = Stats {
        verbose,
        ..Stats::default()
    };
    let file = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
    let (mut decoder, samples) = if path.ends_with(".mp4") {
        let track = mp4::av1_track(&file)?;
        let decoder = Decoder::from_av1c(stats, &track.avcc).map_err(|e| e.to_string())?;
        (decoder, track.samples)
    } else {
        (Decoder::new(stats), ivf::frames(&file)?)
    };
    let mut out = describe_obus(&file, &samples)?;
    let (mut shown, mut shown_existing, mut shown_existing_key) = (0usize, 0usize, 0usize);
    let mut metadata: BTreeMap<String, usize> = BTreeMap::new();
    for (id, &(offset, size)) in samples.iter().enumerate() {
        let sample = file
            .get(offset..offset + size)
            .ok_or("sample past the file's end")?;
        for obu in obus(sample) {
            let obu = obu.map_err(|e| format!("sample {id}: {e}"))?;
            if obu.obu_type == ObuType::Metadata {
                *metadata
                    .entry(describe(metadata::parse(obu.payload)))
                    .or_default() += 1;
            }
        }
        if verbose {
            println!("sample {id}:");
        }
        let outputs = match decoder.decode(sample, id as u64) {
            Ok(outputs) => outputs,
            Err(e) => {
                print!("{out}");
                return Err(format!("sample {id}: {e}"));
            }
        };
        for output in outputs {
            if verbose {
                println!(
                    "  shown {:?} hint {} {}x{}{}",
                    output.frame_type,
                    output.order_hint,
                    output.width,
                    output.height,
                    if output.show_existing_frame {
                        " (existing)"
                    } else {
                        ""
                    },
                );
            }
            shown += 1;
            shown_existing += usize::from(output.show_existing_frame);
            shown_existing_key +=
                usize::from(output.show_existing_frame && output.frame_type == FrameType::Key);
        }
    }
    let s = decoder.accelerator();
    let set = |items: Vec<String>| items.join(" ");
    for sequence in &s.sequences {
        let _ = writeln!(out, "sequence: {sequence}");
    }
    let _ = write!(
        out,
        "{} samples, {} frames ({} key, {} inter, {} intra-only, {} switch), \
         {} hidden, {} shown ({} shown existing, {} of them key frames), \
         {} error resilient; tiles {}, up to {} tile groups a frame; cdef {}, \
         lr {}, film grain {}, superres {}, scaled references {}, screen content {}, \
         intrabc {}, lossless {}, segmentation {}, delta q {}, delta lf {}; \
         global motion in {} frames (translation {}, rotzoom {}, affine {}; \
         shear invalid {}); sizes {}; render sizes {}; metadata {}",
        samples.len(),
        s.frames,
        s.by_type[FrameType::Key as usize],
        s.by_type[FrameType::Inter as usize],
        s.by_type[FrameType::IntraOnly as usize],
        s.by_type[FrameType::Switch as usize],
        s.hidden,
        shown,
        shown_existing,
        shown_existing_key,
        s.error_resilient,
        set(s.tiles.iter().map(|(c, r)| format!("{c}x{r}")).collect()),
        s.max_tile_groups,
        s.cdef,
        s.loop_restoration,
        s.film_grain,
        s.superres,
        s.scaled_references,
        s.screen_content,
        s.intrabc,
        s.lossless,
        s.segmentation,
        s.delta_q,
        s.delta_lf,
        s.global_motion_frames,
        s.global_motion[0],
        s.global_motion[1],
        s.global_motion[2],
        s.global_motion_invalid,
        set(s
            .frame_sizes
            .iter()
            .map(|&(up, w, h)| if up == w {
                format!("{w}x{h}")
            } else {
                format!("{w}x{h}->{up}")
            })
            .collect()),
        set(s
            .render_sizes
            .iter()
            .map(|(w, h)| format!("{w}x{h}"))
            .collect()),
        if metadata.is_empty() {
            "none".into()
        } else {
            set(metadata.iter().map(|(k, v)| format!("{k}:{v}")).collect())
        },
    );
    Ok(out)
}

/// What the OBUs say before any decode: each distinct sequence header's
/// operating points, and the frame and frame header OBUs of each layer.
fn describe_obus(file: &[u8], samples: &[(usize, usize)]) -> Result<String, String> {
    let mut points: Vec<String> = Vec::new();
    let mut layers: BTreeMap<(u8, u8), usize> = BTreeMap::new();
    let mut without_extension = 0usize;
    for (id, &(offset, size)) in samples.iter().enumerate() {
        let sample = file
            .get(offset..offset + size)
            .ok_or("sample past the file's end")?;
        for obu in obus(sample) {
            let obu = obu.map_err(|e| format!("sample {id}: {e}"))?;
            match obu.obu_type {
                ObuType::SequenceHeader => {
                    let s = SequenceHeader::parse(obu.payload)
                        .map_err(|e| format!("sample {id}: sequence header: {e}"))?;
                    let line = s
                        .operating_points
                        .iter()
                        .map(|p| format!("{:#05x}@{}", p.idc, p.seq_level_idx))
                        .collect::<Vec<_>>()
                        .join(" ");
                    if !points.contains(&line) {
                        points.push(line);
                    }
                }
                ObuType::Frame | ObuType::FrameHeader => match obu.extension {
                    Some((temporal, spatial)) => {
                        *layers.entry((spatial, temporal)).or_default() += 1
                    }
                    None => without_extension += 1,
                },
                _ => {}
            }
        }
    }
    let mut out = String::new();
    for line in &points {
        let _ = writeln!(out, "operating points (idc@level): {line}");
    }
    let layers = if layers.is_empty() {
        "none".to_owned()
    } else {
        layers
            .iter()
            .map(|((s, t), n)| format!("s{s}t{t}:{n}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let _ = writeln!(
        out,
        "frame OBUs by layer: {layers}; without an extension: {without_extension}"
    );
    Ok(out)
}

/// One metadata OBU, as one word.
fn describe(parsed: Result<Metadata, Error>) -> String {
    match parsed {
        Ok(Metadata::MasteringDisplay(d)) => {
            let [r, g, b] = d.primaries;
            format!(
                "mdcv={},{}/{},{}/{},{}/{},{}/{},{}",
                r.0,
                r.1,
                g.0,
                g.1,
                b.0,
                b.1,
                d.white_point.0,
                d.white_point.1,
                d.luminance_max,
                d.luminance_min
            )
        }
        Ok(Metadata::ContentLightLevel(l)) => format!("cll={},{}", l.max_cll, l.max_fall),
        Ok(Metadata::Other(kind)) => format!("type{kind}"),
        Err(e) => format!("unreadable({e})"),
    }
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(text) => {
            println!("{text}");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("av1-info: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
