//! Says what a VP9 stream uses, from its frame headers alone, with the
//! crate's own parser: so that each test stream is known to cover what it
//! is meant to (tools/vp9-check.sh). Runs on any host.
//!
//! ```sh
//! vp9-info stream.ivf|stream.mp4 [--frames]
//! ```
//!
//! `--frames` prints a line per frame; the last line is always a summary
//! of counts, `summary: key=1 inter=59 ...`. `ref-out-of-range=` counts the inter frames that
//! name a reference outside the scaling motion vector prediction takes, more than twice as
//! large or less than a sixteenth as large on a side (libvpx's `valid_ref_frame_size`), and
//! `refs-out-of-range=` the references so named. Its `colours=` counts the frames of each
//! colour configuration, as `color_space` (7.2.2's code), range, subsampling and bit depth:
//! `cs2/limited/420/8bit:10`.

use std::collections::BTreeMap;
use std::process::ExitCode;

use reims_vgpu_video_conformance::{ivf, mp4};
use reims_vgpu_video_vaapi::vp9::header::{
    FrameHeader, FrameType, HeaderState, InterpolationFilter, NUM_REF_FRAMES, ReferenceInfo,
    SEG_LVL_ALT_L, SEG_LVL_ALT_Q, SEG_LVL_REF_FRAME, SEG_LVL_SKIP,
};
use reims_vgpu_video_vaapi::vp9::{superframe, vpcc::ConfigurationRecord};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vp9-info: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let per_frame = args.iter().any(|a| a == "--frames");
    let path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or("usage: vp9-info <stream> [--frames]")?;
    let file = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let samples = if path.ends_with(".mp4") {
        let track = mp4::vp9_track(&file)?;
        let record = ConfigurationRecord::parse(&track.avcc).map_err(|e| e.to_string())?;
        println!(
            "vpcC: version {} profile {} level {} bit depth {} subsampling {:?} full range {} colour {:?}",
            record.version,
            record.profile,
            record.level,
            record.bit_depth,
            record.chroma_subsampling,
            record.full_range,
            record.colour
        );
        track.samples
    } else {
        ivf::vp9_frames(&file)?
    };
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut count = |key: &'static str, yes: bool| {
        if yes {
            *counts.entry(key).or_default() += 1;
        }
    };
    let mut refs_out_of_range = 0u64;
    let mut sizes = BTreeMap::new();
    let mut profiles = BTreeMap::new();
    let mut colours = BTreeMap::new();
    let mut max_tiles = (0, 0);
    let mut state = HeaderState::default();
    let mut refs: [Option<ReferenceInfo>; NUM_REF_FRAMES] = [None; NUM_REF_FRAMES];
    let mut last_size = None;
    for (index, &(offset, size)) in samples.iter().enumerate() {
        let chunk = file
            .get(offset..offset + size)
            .ok_or("past the file's end")?;
        let frames = superframe::frames(chunk).map_err(|e| format!("chunk {index}: {e}"))?;
        count("chunks", true);
        count("superframes", frames.len() > 1);
        for frame in frames {
            let h = FrameHeader::parse(frame, &mut state, &refs)
                .map_err(|e| format!("chunk {index}: {e}"))?;
            count("frames", true);
            if h.show_existing_frame {
                count("show-existing", true);
                if per_frame {
                    println!(
                        "chunk {index}: show existing slot {}",
                        h.frame_to_show_map_idx
                    );
                }
                continue;
            }
            let key = h.frame_type == FrameType::Key;
            count("key", key);
            count("intra-only", h.intra_only);
            count("inter", !h.is_intra());
            count("hidden", !h.show_frame);
            count("error-resilient", h.error_resilient_mode);
            count("reset-context", h.reset_frame_context != 0);
            count("no-context-refresh", !h.refresh_frame_context);
            count("parallel", h.frame_parallel_decoding_mode);
            count("context-idx-nonzero", h.frame_context_idx != 0);
            count("size-from-ref", h.size_from_ref.is_some());
            count(
                "render-differs",
                (h.render_width, h.render_height) != (h.width, h.height),
            );
            count(
                "resized-inter",
                !h.is_intra() && last_size.is_some_and(|s| s != (h.width, h.height)),
            );
            count(
                "scaled-reference",
                !h.is_intra()
                    && h.ref_frame_idx.iter().any(|&i| {
                        refs[usize::from(i)]
                            .is_some_and(|r| (r.width, r.height) != (h.width, h.height))
                    }),
            );
            let out_of_range = if h.is_intra() {
                0
            } else {
                h.ref_frame_idx
                    .iter()
                    .filter(|&&i| {
                        refs[usize::from(i)].is_some_and(|r| {
                            !(2 * h.width >= r.width
                                && 2 * h.height >= r.height
                                && h.width <= 16 * r.width
                                && h.height <= 16 * r.height)
                        })
                    })
                    .count()
            };
            count("ref-out-of-range", out_of_range > 0);
            refs_out_of_range += out_of_range as u64;
            count("odd-size", h.width % 2 == 1 || h.height % 2 == 1);
            if !h.is_intra() {
                count(
                    match h.interpolation_filter {
                        InterpolationFilter::EightTapSmooth => "filter-smooth",
                        InterpolationFilter::EightTap => "filter-regular",
                        InterpolationFilter::EightTapSharp => "filter-sharp",
                        InterpolationFilter::Bilinear => "filter-bilinear",
                        InterpolationFilter::Switchable => "filter-switchable",
                    },
                    true,
                );
                count("high-precision-mv", h.allow_high_precision_mv);
                count("sign-bias", h.ref_frame_sign_bias.iter().any(|&b| b));
            }
            let lf = &h.loop_filter;
            count("lf-off", lf.level == 0);
            count("lf-sharpness", lf.sharpness != 0);
            count("lf-delta-enabled", lf.delta_enabled);
            count("lf-delta-update", lf.delta_update);
            count("lossless", h.quantization.lossless());
            let q = &h.quantization;
            count(
                "q-deltas",
                q.delta_q_y_dc != 0 || q.delta_q_uv_dc != 0 || q.delta_q_uv_ac != 0,
            );
            let s = &h.segmentation;
            count("seg-enabled", s.enabled);
            count("seg-update-map", s.update_map);
            count("seg-temporal", s.temporal_update);
            count("seg-update-data", s.update_data);
            count("seg-abs", s.enabled && s.abs_or_delta_update);
            for (feature, name) in [
                (SEG_LVL_ALT_Q, "seg-alt-q"),
                (SEG_LVL_ALT_L, "seg-alt-lf"),
                (SEG_LVL_REF_FRAME, "seg-ref"),
                (SEG_LVL_SKIP, "seg-skip"),
            ] {
                count(
                    name,
                    (0..8).any(|segment| s.feature_active(segment, feature)),
                );
            }
            // Which reference a segment is held to, and whether one is
            // held to it with skip as well.
            for (reference, name, with_skip) in [
                (0, "seg-ref-intra", "seg-ref-intra-skip"),
                (1, "seg-ref-last", "seg-ref-last-skip"),
                (2, "seg-ref-golden", "seg-ref-golden-skip"),
                (3, "seg-ref-altref", "seg-ref-altref-skip"),
            ] {
                let held = |segment: usize| {
                    s.feature_active(segment, SEG_LVL_REF_FRAME)
                        && s.feature_data[segment][SEG_LVL_REF_FRAME] == reference
                };
                count(name, (0..8).any(held));
                count(
                    with_skip,
                    (0..8).any(|segment| held(segment) && s.feature_active(segment, SEG_LVL_SKIP)),
                );
            }
            count("tile-columns", h.tile_info.cols_log2 > 0);
            count("tile-rows", h.tile_info.rows_log2 > 0);
            max_tiles.0 = max_tiles.0.max(1 << h.tile_info.cols_log2);
            max_tiles.1 = max_tiles.1.max(1 << h.tile_info.rows_log2);
            *sizes.entry((h.width, h.height)).or_insert(0u64) += 1;
            *profiles
                .entry((h.profile, h.color.bit_depth))
                .or_insert(0u64) += 1;
            let c = h.color;
            *colours
                .entry((
                    c.color_space,
                    c.full_range,
                    c.subsampling_x,
                    c.subsampling_y,
                    c.bit_depth,
                ))
                .or_insert(0u64) += 1;
            if per_frame {
                println!(
                    "chunk {index}: {} {}x{}{} show {} refresh {:#04x} refs {:?} q {} lf {}/{} seg {} tiles {}x{} ctx {}{}",
                    if key {
                        "key"
                    } else if h.intra_only {
                        "intra-only"
                    } else {
                        "inter"
                    },
                    h.width,
                    h.height,
                    if (h.render_width, h.render_height) != (h.width, h.height) {
                        format!(" render {}x{}", h.render_width, h.render_height)
                    } else {
                        String::new()
                    },
                    u8::from(h.show_frame),
                    h.refresh_frame_flags,
                    h.ref_frame_idx,
                    q.base_q_idx,
                    lf.level,
                    lf.sharpness,
                    if s.enabled { "on" } else { "off" },
                    1 << h.tile_info.cols_log2,
                    1 << h.tile_info.rows_log2,
                    h.frame_context_idx,
                    if h.error_resilient_mode {
                        " resilient"
                    } else {
                        ""
                    },
                );
            }
            let info = ReferenceInfo {
                width: h.width,
                height: h.height,
                color: h.color,
            };
            for (slot, entry) in refs.iter_mut().enumerate() {
                if h.refresh_frame_flags & (1 << slot) != 0 {
                    *entry = Some(info);
                }
            }
            last_size = Some((h.width, h.height));
        }
    }
    let sizes: Vec<String> = sizes
        .iter()
        .map(|((w, h), n)| format!("{w}x{h}:{n}"))
        .collect();
    let profiles: Vec<String> = profiles
        .iter()
        .map(|((p, b), n)| format!("{p}/{b}bit:{n}"))
        .collect();
    let colours: Vec<String> = colours
        .iter()
        .map(|((cs, full, x, y, depth), n)| {
            let subsampling = match (x, y) {
                (true, true) => "420",
                (true, false) => "422",
                (false, true) => "440",
                (false, false) => "444",
            };
            let range = if *full { "full" } else { "limited" };
            format!("cs{cs}/{range}/{subsampling}/{depth}bit:{n}")
        })
        .collect();
    counts.insert("refs-out-of-range", refs_out_of_range);
    let counts: Vec<String> = counts.iter().map(|(k, v)| format!("{k}={v}")).collect();
    println!(
        "summary: {} profiles={} sizes={} max-tiles={}x{} colours={}",
        counts.join(" "),
        profiles.join(","),
        sizes.join(","),
        max_tiles.0,
        max_tiles.1,
        colours.join(",")
    );
    Ok(())
}
