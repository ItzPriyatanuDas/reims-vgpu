//! The decode harness on a stand-in backend, so that it runs on every
//! host: an H.264 MP4 written from our own parameter sets, and decoders
//! that "decode" each sample into a pattern of its frame ID.

use std::collections::HashMap;
use std::sync::Arc;

use reims_vgpu_video_codec::encode::h264::{Pps, Sps, Vui, avcc};
use reims_vgpu_video_codec::encode::{Entropy, Profile};
use reims_vgpu_video_core::backend::{
    Backend, BackendDecoder, BackendError, Codec, CodecCapabilities, CompressedFrame, DecodedFrame,
    DecoderBackend, DecoderConfig, FrameReadBack, PixelFormat, ProfileSupport, copy_biplanar,
    read_back_by,
};
use reims_vgpu_video_core::fourcc::FourCc;
use reims_vgpu_video_core::plist;
use reims_vgpu_video_core::wire::message::{MessageFlags, MessageWriter};

use super::stand_in::{Alignment, buffer_layout, listed_decoder};
use super::{decode_file, decode_file_sized};
use crate::md5::Md5;
use crate::mp4_write::{self, Sample};
use crate::suite::compare;

const WIDTH: u32 = 64;
const HEIGHT: u32 = 64;
const FRAMES: usize = 5;

/// The pattern a frame of `frame_id` holds: luma, then interleaved chroma,
/// each `width` bytes a row.
fn pattern(frame_id: i64, (width, height): (u32, u32)) -> (Vec<u8>, Vec<u8>) {
    let seed = u8::try_from(frame_id & 0x3f).unwrap_or(0);
    let width = usize::try_from(width).unwrap_or(0);
    let height = usize::try_from(height).unwrap_or(0);
    let luma = (0..width * height)
        .map(|i| u8::try_from(i % 251).unwrap_or(0).wrapping_add(seed))
        .collect();
    let chroma = (0..width * height / 2)
        .map(|i| u8::try_from(i % 241).unwrap_or(0) ^ seed)
        .collect();
    (luma, chroma)
}

/// The MD5 the harness should give a frame of `frame_id` and `size`: its
/// planes, packed.
fn expected_md5(frame_id: i64, size: (u32, u32)) -> String {
    let (luma, chroma) = pattern(frame_id, size);
    let mut md5 = Md5::new();
    md5.update(&luma);
    md5.update(&chroma);
    md5.hex()
}

struct PatternFrame {
    frame_id: i64,
    size: (u32, u32),
}

impl DecodedFrame for PatternFrame {
    fn frame_id(&self) -> i64 {
        self.frame_id
    }
    fn pixel_format(&self) -> PixelFormat {
        PixelFormat::Nv12VideoRange
    }
    fn width(&self) -> u32 {
        self.size.0
    }
    fn height(&self) -> u32 {
        self.size.1
    }
    fn read_back(&self) -> Result<Box<dyn FrameReadBack + '_>, BackendError> {
        let (luma, chroma) = pattern(self.frame_id, self.size);
        let (width, height) = self.size;
        Ok(read_back_by(move |planes| {
            let stride = usize::try_from(width).unwrap_or(0);
            copy_biplanar(
                PixelFormat::Nv12VideoRange,
                width,
                height,
                [(&luma, stride), (&chroma, stride)],
                planes,
            );
            Ok(())
        }))
    }
}

/// Decoders that emit a pattern frame for each sample, or fail the sample
/// whose frame ID is `fail`; with `odd_size`, the frames of odd IDs are of
/// that size, as an AV1 stream's frames may be.
struct PatternDecoders {
    fail: Option<i64>,
    odd_size: Option<(u32, u32)>,
}

struct PatternDecoder {
    fail: Option<i64>,
    odd_size: Option<(u32, u32)>,
}

impl PatternDecoder {
    fn size(&self, frame_id: i64) -> (u32, u32) {
        match self.odd_size {
            Some(size) if frame_id % 2 == 1 => size,
            _ => (WIDTH, HEIGHT),
        }
    }
}

impl DecoderBackend for PatternDecoders {
    fn capabilities(&self) -> Vec<CodecCapabilities> {
        [(Codec::H264, 100), (Codec::Av1, 0)]
            .into_iter()
            .map(|(codec, profile)| CodecCapabilities {
                codec,
                profiles: vec![ProfileSupport {
                    profile,
                    max_level: None,
                }],
                max_width: 1920,
                max_height: 1088,
            })
            .collect()
    }

    fn open(&self, config: &DecoderConfig) -> Result<Box<dyn BackendDecoder>, BackendError> {
        assert_eq!((config.width, config.height), (WIDTH, HEIGHT));
        Ok(Box::new(PatternDecoder {
            fail: self.fail,
            odd_size: self.odd_size,
        }))
    }
}

impl BackendDecoder for PatternDecoder {
    fn output_format(&self) -> PixelFormat {
        PixelFormat::Nv12VideoRange
    }

    fn decode(
        &mut self,
        frame: &CompressedFrame<'_>,
    ) -> Result<Vec<Box<dyn DecodedFrame>>, BackendError> {
        if Some(frame.frame_id) == self.fail {
            return Err(BackendError::BadData("the stand-in fails this one".into()));
        }
        Ok(vec![Box::new(PatternFrame {
            frame_id: frame.frame_id,
            size: self.size(frame.frame_id),
        })])
    }

    fn flush(&mut self) -> Result<Vec<Box<dyn DecodedFrame>>, BackendError> {
        Ok(Vec::new())
    }

    fn can_accept(&self, _config: &DecoderConfig) -> bool {
        true
    }
}

/// An MP4 of `FRAMES` H.264 samples at 64x64, the smallest Apple's decoder
/// takes (core refuses less at `dsta`, as it does), each an IDR slice's
/// worth of bytes the stand-in never reads.
fn stream() -> Vec<u8> {
    let sps = Sps {
        profile: Profile::H264High,
        level_idc: 30,
        max_num_ref_frames: 1,
        width_in_mbs: WIDTH / 16,
        height_in_mbs: HEIGHT / 16,
        crop_right: 0,
        crop_bottom: 0,
        vui: Vui::default(),
    };
    let pps = Pps {
        profile: Profile::H264High,
        entropy: Entropy::Cabac,
        pic_init_qp: 26,
        transform_8x8_mode_flag: true,
    };
    let record = avcc(&sps.to_nal().unwrap(), &pps.to_nal().unwrap());
    let samples: Vec<Sample> = (0..FRAMES)
        .map(|n| Sample {
            data: vec![0, 0, 0, 3, 0x65, 0x88, u8::try_from(n).unwrap_or(0)],
            pts: i64::try_from(n).unwrap_or(0),
            dts: i64::try_from(n).unwrap_or(0),
            duration: 1,
            sync: true,
        })
        .collect();
    mp4_write::write(b"avc1", b"avcC", &record, WIDTH, HEIGHT, 30, &samples)
}

/// An AV1 MP4 of `FRAMES` samples at 64x64, its `av1C` 8-bit Main with no
/// sequence header, each sample bytes the stand-in never reads.
fn av1_stream() -> Vec<u8> {
    let samples: Vec<Sample> = (0..FRAMES)
        .map(|n| Sample {
            data: vec![0x12, 0x00, u8::try_from(n).unwrap_or(0)],
            pts: i64::try_from(n).unwrap_or(0),
            dts: i64::try_from(n).unwrap_or(0),
            duration: 1,
            sync: n == 0,
        })
        .collect();
    mp4_write::write(
        b"av01",
        b"av1C",
        &[0x81, 0x00, 0x0c, 0x00],
        WIDTH,
        HEIGHT,
        30,
        &samples,
    )
}

fn backend(fail: Option<i64>) -> Backend {
    backend_with(fail, None)
}

fn backend_with(fail: Option<i64>, odd_size: Option<(u32, u32)>) -> Backend {
    Backend {
        decoders: Some(Arc::new(PatternDecoders { fail, odd_size })),
        ..Backend::none("stand-in")
    }
}

#[test]
fn every_frame_comes_back_hashed_as_the_guests_buffer_holds_it() -> Result<(), String> {
    let reference: Vec<String> = (0..FRAMES)
        .map(|n| expected_md5(i64::try_from(n).unwrap_or(0), (WIDTH, HEIGHT)))
        .collect();
    for asynchronous in [false, true] {
        let run = decode_file(&backend(None), &stream(), asynchronous)?;
        assert_eq!(run.samples, FRAMES);
        assert_eq!(run.failed, 0);
        // -12782, as Apple's host answers `dfin` for AVD's decoders.
        assert_eq!(run.finished, -12782);
        let ids: Vec<i64> = run.frames.iter().map(|f| f.frame_id).collect();
        assert_eq!(ids, [0, 1, 2, 3, 4]);
        compare(&run, &reference)?;
        // One buffer lent a frame, and each one given back.
        assert_eq!(run.released, FRAMES);
        assert_eq!(run.counts.get("Dbuf"), Some(&FRAMES));
        assert_eq!(run.counts.get("Demt"), Some(&FRAMES));
    }
    Ok(())
}

#[test]
fn a_frame_that_does_not_decode_fails_the_comparison() -> Result<(), String> {
    let reference: Vec<String> = (0..FRAMES)
        .map(|n| expected_md5(i64::try_from(n).unwrap_or(0), (WIDTH, HEIGHT)))
        .collect();
    let run = decode_file(&backend(Some(2)), &stream(), false);
    let run = match run {
        // The guest's dfrm answers the decoder's error, which ends the run.
        Err(error) => {
            assert!(error.contains("dfrm 2"), "{error}");
            return Ok(());
        }
        Ok(run) => run,
    };
    assert_eq!(run.failed, 1);
    assert!(compare(&run, &reference).is_err());
    Ok(())
}

#[test]
fn an_empty_reference_fails_the_comparison() -> Result<(), String> {
    let run = decode_file(&backend(None), &stream(), false)?;
    assert!(compare(&run, &[]).is_err());
    Ok(())
}

#[test]
fn a_backend_without_decoders_lists_none_and_opens_no_session() {
    let error = decode_file(&Backend::none("nothing"), &stream(), false).err();
    assert_eq!(error.as_deref(), Some("decs lists no decoder for avc1"));
}

#[test]
fn the_session_opens_after_decs_with_the_listed_decoder() -> Result<(), String> {
    // Core answers a `,cid` naming another decoder than its own with
    // -12906, so a session that decodes was opened with the listed one.
    let run = decode_file(&backend(None), &stream(), false)?;
    assert_eq!(
        run.sequence.get(..2),
        Some(&["reply decs".to_owned(), "reply +dec".to_owned()][..])
    );
    assert_eq!(run.frames.len(), FRAMES);
    Ok(())
}

/// A `decs` reply listing `entries`, each a codec type and an ID.
fn decs_reply(entries: &[(&str, &str)]) -> Result<Vec<u8>, String> {
    let entries = entries
        .iter()
        .map(|&(codec, id)| {
            let mut entry = plist::Dictionary::new();
            entry.insert(
                "CMClassImplementationID".into(),
                plist::Value::String(id.into()),
            );
            entry.insert(
                "VTCodecType".into(),
                plist::Value::Array(vec![plist::Value::String(codec.into())]),
            );
            plist::Value::Dictionary(entry)
        })
        .collect();
    let mut reply = MessageWriter::new(
        FourCc::new(*b"decs"),
        MessageFlags::REPLY_TO_GUEST,
        None,
        100,
    );
    reply
        .property_list(FourCc::new(*b",dec"), Some(&plist::Value::Array(entries)))
        .map_err(|e| e.to_string())?;
    Ok(reply.finish())
}

#[test]
fn the_decoder_id_is_the_entry_for_the_streams_codec() -> Result<(), String> {
    let reply = decs_reply(&[
        ("hvc1", "host.hevc"),
        ("avc1", "host.avc"),
        ("avc1", "host.avc2"),
    ])?;
    assert_eq!(listed_decoder(&reply, *b"avc1"), Ok("host.avc".to_owned()));
    assert_eq!(listed_decoder(&reply, *b"hvc1"), Ok("host.hevc".to_owned()));
    assert_eq!(
        listed_decoder(&reply, *b"vp09"),
        Err("decs lists no decoder for vp09".to_owned())
    );
    Ok(())
}

#[test]
fn buffers_are_laid_out_from_the_dpba_alignments() {
    let nv12 = FourCc::new(*b"420v");
    let p010 = FourCc::new(*b"x420");
    // At AVD's 64 and 256, as every backend's Dpba asks: rows 256 bytes
    // past the samples rounded up to 64, chroma at a multiple of 256.
    let avd = Alignment {
        bytes_per_row: 64,
        plane: 256,
    };
    for (format, size, row, chroma) in [
        (nv12, (1280, 720), 1536, 1536 * 720),
        (
            nv12,
            (641, 361),
            960,
            (960 * 361usize).next_multiple_of(256),
        ),
        (
            p010,
            (643, 363),
            1600,
            (1600 * 363usize).next_multiple_of(256),
        ),
    ] {
        let (layout, len) = buffer_layout(format, size, avd);
        assert_eq!(layout.planes[0].bytes_per_row, row);
        assert_eq!(layout.planes[1].bytes_per_row, row);
        assert_eq!(layout.planes[1].offset, chroma);
        assert_eq!(
            len,
            chroma + row * usize::try_from(size.1.div_ceil(2)).unwrap_or(0)
        );
    }
    // Larger alignments round both up.
    let wide = Alignment {
        bytes_per_row: 512,
        plane: 4096,
    };
    let (layout, _) = buffer_layout(nv12, (641, 361), wide);
    assert_eq!(layout.planes[0].bytes_per_row, 1024);
    assert_eq!(
        layout.planes[1].offset,
        (1024 * 361usize).next_multiple_of(4096)
    );
}

#[test]
fn frames_of_another_size_are_hashed_at_their_own() -> Result<(), String> {
    // Frames 1 and 3 are 48x32, written at the top left of the session's
    // 64x64 buffers, the rest of which the host leaves alone.
    let small = (48, 32);
    let size = |n: i64| if n % 2 == 1 { small } else { (WIDTH, HEIGHT) };
    let ids = 0..i64::try_from(FRAMES).unwrap_or(0);
    let reference: Vec<String> = ids.clone().map(|n| expected_md5(n, size(n))).collect();
    let sizes: HashMap<i64, (u32, u32)> = ids.map(|n| (n, size(n))).collect();
    for asynchronous in [false, true] {
        let run = decode_file_sized(
            &backend_with(None, Some(small)),
            &stream(),
            asynchronous,
            sizes.clone(),
        )?;
        assert_eq!(run.failed, 0);
        compare(&run, &reference)?;
        // At the session's size, the small frames take in what the host
        // did not write.
        let unsized_run = decode_file(&backend_with(None, Some(small)), &stream(), asynchronous)?;
        assert!(compare(&unsized_run, &reference).is_err());
    }
    Ok(())
}

#[test]
fn an_av1_frame_of_another_size_is_hashed_in_the_buffer_dpba_asked_for() -> Result<(), String> {
    // The host asks for buffers of an AV1 frame's own size where it
    // changes, and the stand-in lends them so: a frame is hashed whole,
    // with no reference sizes given.
    let small = (48, 32);
    let size = |n: i64| if n % 2 == 1 { small } else { (WIDTH, HEIGHT) };
    let reference: Vec<String> = (0..i64::try_from(FRAMES).unwrap_or(0))
        .map(|n| expected_md5(n, size(n)))
        .collect();
    for asynchronous in [false, true] {
        let run = decode_file(
            &backend_with(None, Some(small)),
            &av1_stream(),
            asynchronous,
        )?;
        assert_eq!(run.failed, 0);
        compare(&run, &reference)?;
        // dsta's, then one for each change of size.
        assert_eq!(run.counts.get("Dpba"), Some(&5));
    }
    Ok(())
}
