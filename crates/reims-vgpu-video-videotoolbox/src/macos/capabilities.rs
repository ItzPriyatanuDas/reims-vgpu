//! What this host's VideoToolbox decodes, from its own answers: whether a
//! codec decodes in hardware (`VTIsHardwareDecodeSupported`, after
//! `VTRegisterSupplementalVideoDecoderIfAvailable` for VP9), and each
//! profile's hardware and highest level from the decoder capabilities
//! dictionary Apple's host lists to its guests
//! (`VTCopyDecoderCapabilitiesDictionaryForCodecTypes`, [`super::ffi::private`]).
//!
//! Only hardware decoders are listed, as Apple's host lists to a guest, and
//! only the profiles whose streams core's 4:2:0 buffers hold, at 8 or 10
//! bits ([`four_two_zero`]).

use std::time::Duration;

use reims_vgpu_video_core::backend::{Codec, CodecCapabilities, ProfileSupport};

use super::cf::{self, Owned};
use super::ffi::private::private;
use super::ffi::video_toolbox::VTIsHardwareDecodeSupported;
use super::stream_format::{CODECS, codec_type, register_decoder};

/// How many times a codec's capabilities dictionary is read again while it
/// lists no hardware profile for a codec VTIsHardwareDecodeSupported says
/// decodes in hardware, and how long apart. Beside another process's
/// VideoToolbox tests, the dictionary listed HEVC's profiles, then
/// H.264's, with none of them hardware, for one read each, a few
/// milliseconds, among 25,473 reads in 90 s; every other read and the
/// hardware answer were as before (the M3 Max, macOS 27.0.1, 2026-10-24).
/// Ours: 10 reads 20 ms apart outlast that by far, and cost a codec that
/// really lists none 0.2 s once, as the backend is made.
const READ_AGAIN: (u32, Duration) = (10, Duration::from_millis(20));

/// The codec's hardware profiles from `read`, read again up to
/// [`READ_AGAIN`] times while it answers a dictionary with none.
fn settled_profiles(
    codec: Codec,
    mut read: impl FnMut() -> Option<Vec<ProfileSupport>>,
    (times, apart): (u32, Duration),
) -> Option<Vec<ProfileSupport>> {
    let mut listed = read();
    let mut again = 0;
    while again < times && listed.as_ref().is_some_and(Vec::is_empty) {
        std::thread::sleep(apart);
        listed = read();
        again += 1;
    }
    if again > 0 {
        match &listed {
            Some(profiles) if profiles.is_empty() => log::warn!(
                "videotoolbox: {codec:?} decodes in hardware, but its capabilities list no \
                 hardware profile, read {} times",
                again + 1
            ),
            _ => log::info!(
                "videotoolbox: {codec:?}'s capabilities listed no hardware profile at first, \
                 and listed some at read {}",
                again + 1
            ),
        }
    }
    listed
}

/// The profiles of `codec` whose streams are 4:2:0 (or monochrome) at 8 or
/// 10 bits, as the bitstream numbers them; the others (H.264's 4:2:2 and
/// 4:4:4 profiles 122, 244 and 44, HEVC's range extensions 4, AV1's High 1
/// and Professional 2, VP9's 1 and 3) need buffers core has no format for.
fn four_two_zero(codec: Codec) -> &'static [u32] {
    match codec {
        // Baseline, Main, Extended, High, High 10.
        Codec::H264 => &[66, 77, 88, 100, 110],
        // Main, Main 10, Main Still Picture.
        Codec::Hevc => &[1, 2, 3],
        // Main.
        Codec::Av1 => &[0],
        // Profile 0 (8-bit 4:2:0) and 2 (10- and 12-bit 4:2:0).
        Codec::Vp9 => &[0, 2],
    }
}

/// One codec's hardware profiles, as VideoToolbox's capabilities
/// dictionary lists them: `{VTCodecSupportDict: {"avc1":
/// {VTPerProfileSupport: {"100": {VTIsHardwareAccelerated, VTMaxDecodeLevel,
/// ...}}, VTSupportedProfiles: [...]}}}` (measured on macOS 27.0.1 26A434,
/// 2026-10-24). None when VideoToolbox has no such dictionary for it.
fn listed_profiles(codec: Codec) -> Option<Vec<ProfileSupport>> {
    let private = private();
    let copy = private.copy_decoder_capabilities?;
    let codec_support_key = private.codec_support?;
    let per_profile_key = private.per_profile_support?;
    let hardware_key = private.is_hardware_accelerated?;
    let level_key = private.max_decode_level?;
    let number = cf::number_i64(i64::from(codec_type(codec)));
    let codecs = cf::array(&[number.cf()]);
    // SAFETY: the function is VideoToolbox's (ffi::private), given a live
    // array of CFNumbers; it returns a dictionary the caller owns.
    let capabilities = unsafe { Owned::take(copy(codecs.as_ptr())) }?;
    let fourcc = codec.fourcc().to_be_bytes();
    let per_profile = (capabilities.cf().get(codec_support_key))
        .and_then(|support| support.get_str(std::str::from_utf8(&fourcc).ok()?))
        .and_then(|this_codec| this_codec.get(per_profile_key))?;
    let mut profiles: Vec<ProfileSupport> = (per_profile.entries().into_iter())
        .filter_map(|(profile, details)| {
            let profile: u32 = profile.string()?.parse().ok()?;
            let hardware = details.get(hardware_key).and_then(cf::Cf::bool);
            hardware.unwrap_or(false).then(|| ProfileSupport {
                profile,
                max_level: (details.get(level_key).and_then(cf::Cf::i64))
                    .and_then(|level| u32::try_from(level).ok()),
            })
        })
        .collect();
    profiles.sort_by_key(|profile| profile.profile);
    Some(profiles)
}

/// What VideoToolbox decodes in hardware here, of the codecs core knows:
/// for each, the 4:2:0 profiles it decodes in hardware and their levels.
/// A codec with a hardware decoder but no capabilities dictionary (VP9's)
/// lists its 4:2:0 profiles with no level, and core lists Apple's for
/// them.
///
/// The sizes are the largest VideoToolbox's level allows a decoder, which
/// is the only size it gives: [`max_size`].
pub(crate) fn decoders() -> Vec<CodecCapabilities> {
    CODECS
        .into_iter()
        .filter_map(|codec| {
            register_decoder(codec);
            // SAFETY: a plain call with a codec type.
            if unsafe { VTIsHardwareDecodeSupported(codec_type(codec)) } == 0 {
                return None;
            }
            let wanted = four_two_zero(codec);
            let listed = settled_profiles(codec, || listed_profiles(codec), READ_AGAIN);
            let profiles: Vec<ProfileSupport> = match listed {
                Some(listed) => listed
                    .into_iter()
                    .filter(|profile| wanted.contains(&profile.profile))
                    .collect(),
                None => wanted
                    .iter()
                    .map(|&profile| ProfileSupport {
                        profile,
                        max_level: None,
                    })
                    .collect(),
            };
            if profiles.is_empty() {
                return None;
            }
            let (max_width, max_height) = max_size(codec);
            Some(CodecCapabilities {
                codec,
                profiles,
                max_width,
                max_height,
            })
        })
        .collect()
}

/// The largest picture a decoder of `codec` takes at the highest
/// level Apple's hardware decoders list, from each standard's limits:
/// H.264 and HEVC level 6.2's 8192x4320 (ITU-T H.264 Table A-1, H.265
/// Table A.8), AV1 level 6.3's 16384x8704 (AV1 Annex A.3), VP9 level 6.2's
/// 16384 (the VP9 levels table). VideoToolbox's decoders answer no size of
/// their own.
fn max_size(codec: Codec) -> (u32, u32) {
    match codec {
        Codec::H264 | Codec::Hevc => (8192, 4320),
        Codec::Av1 => (16384, 8704),
        Codec::Vp9 => (16384, 16384),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use reims_vgpu_video_core::backend::{
        BackendDecoder, BackendError, Codec, CompressedFrame, DecoderConfig, PixelFormat,
    };
    use reims_vgpu_video_core::runtime::{Runtime, StdRuntime};

    use super::super::decoder::{Decoder, DecoderChoice};
    use super::super::encoder::EncoderChoice;
    use super::super::tests::encode;
    use super::*;

    /// What the two queries answer for one codec: VTIsHardwareDecodeSupported,
    /// and the hardware profiles the capabilities dictionary lists with
    /// their highest levels, or none where it has no per-profile dictionary.
    type Answer = (Codec, bool, Option<Vec<(u32, Option<u32>)>>);

    /// Both queries for every codec core knows, as `decoders` asks them.
    fn answers() -> Vec<Answer> {
        CODECS
            .into_iter()
            .map(|codec| {
                register_decoder(codec);
                // SAFETY: a plain call with a codec type.
                let hardware = unsafe { VTIsHardwareDecodeSupported(codec_type(codec)) } != 0;
                let listed = listed_profiles(codec).map(|profiles| {
                    profiles
                        .into_iter()
                        .map(|p| (p.profile, p.max_level))
                        .collect()
                });
                (codec, hardware, listed)
            })
            .collect()
    }

    /// A dictionary that lists no hardware profile for a moment is read
    /// again, and the first read that lists some is taken.
    #[test]
    fn a_listing_with_no_hardware_profile_is_read_again() {
        let main = ProfileSupport {
            profile: 77,
            max_level: Some(52),
        };
        let quick = (10, Duration::from_millis(1));
        let mut reads = vec![Some(Vec::new()), Some(Vec::new()), Some(vec![main])].into_iter();
        let mut count = 0;
        let settled = settled_profiles(
            Codec::H264,
            || {
                count += 1;
                reads.next().unwrap_or(Some(Vec::new()))
            },
            quick,
        );
        assert_eq!(settled, Some(vec![main]));
        assert_eq!(count, 3);
        // One that lists none for good is read 11 times, and lists none.
        let mut count = 0;
        let settled = settled_profiles(
            Codec::H264,
            || {
                count += 1;
                Some(Vec::new())
            },
            quick,
        );
        assert_eq!((settled, count), (Some(Vec::new()), 11));
        // A listing with profiles, or no dictionary, is read once.
        for listed in [Some(vec![main]), None] {
            let mut count = 0;
            let settled = settled_profiles(
                Codec::Vp9,
                || {
                    count += 1;
                    listed.clone()
                },
                quick,
            );
            assert_eq!((settled, count), (listed, 1));
        }
    }

    /// A probe: the two queries asked again and again while four
    /// hardware decoder sessions decode ordinary H.264 and HEVC samples
    /// beside them, two keeping one session each and two opening a new one
    /// for each pass, and the backend's list made from them each time. Each
    /// raw answer that differs from the first, taken before the load, is
    /// printed and counted; the backend's list must never differ. Run by
    /// hand, for `RVGV_QUERY_SECONDS` (60 by default), alone or beside another
    /// process's run of this crate's tests, where the raw answers changed:
    ///
    /// ```sh
    /// cargo test --offline --release \
    ///   -p reims-vgpu-video-videotoolbox -- --ignored --nocapture \
    ///   decoder_queries_beside_hardware_decodes
    /// ```
    #[test]
    #[ignore = "a probe, run by hand: the decoder queries for a minute beside hardware decodes"]
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding: the load is std threads that decode beside the queries"
    )]
    fn decoder_queries_beside_hardware_decodes() -> Result<(), BackendError> {
        let seconds = std::env::var("RVGV_QUERY_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60);
        let before = answers();
        let listed_before = decoders();
        println!("before the load: {before:?}");
        let streams: Vec<_> = [Codec::H264, Codec::Hevc]
            .into_iter()
            .map(|codec| {
                let (samples, record) = encode(
                    codec,
                    PixelFormat::Nv12VideoRange,
                    EncoderChoice::Software,
                    Vec::new(),
                    &[],
                )?;
                Ok((codec, samples, record))
            })
            .collect::<Result<_, BackendError>>()?;
        let streams = Arc::new(streams);
        let stop = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(AtomicUsize::new(0));
        let failures = Arc::new(AtomicUsize::new(0));
        let sessions = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..4)
            .map(|index| {
                let (streams, stop) = (streams.clone(), stop.clone());
                let (frames, failures, sessions) =
                    (frames.clone(), failures.clone(), sessions.clone());
                std::thread::spawn(move || {
                    let (codec, samples, record) = &streams[index % 2];
                    let config = DecoderConfig {
                        codec: *codec,
                        width: 640,
                        height: 360,
                        configuration: record.clone(),
                    };
                    let keeps = index < 2;
                    let mut decoder = None;
                    while !stop.load(Ordering::Relaxed) {
                        if decoder.is_none() || !keeps {
                            match Decoder::open(&config, DecoderChoice::Hardware) {
                                Ok(opened) => {
                                    sessions.fetch_add(1, Ordering::Relaxed);
                                    decoder = Some(opened);
                                }
                                Err(error) => {
                                    eprintln!("{codec:?}: open: {error}");
                                    failures.fetch_add(1, Ordering::Relaxed);
                                    std::thread::sleep(Duration::from_millis(50));
                                    continue;
                                }
                            }
                        }
                        let Some(open) = decoder.as_mut() else {
                            continue;
                        };
                        for sample in samples {
                            match open.decode(&CompressedFrame {
                                frame_id: sample.frame_id,
                                data: &sample.data,
                                sample_sizes: &[],
                                presentation_time: sample.presentation_time,
                                decode_time: sample.decode_time,
                                duration: sample.duration,
                                new_config: None,
                            }) {
                                Ok(out) => frames.fetch_add(out.len(), Ordering::Relaxed),
                                Err(_) => failures.fetch_add(1, Ordering::Relaxed),
                            };
                        }
                        match open.flush() {
                            Ok(out) => frames.fetch_add(out.len(), Ordering::Relaxed),
                            Err(_) => failures.fetch_add(1, Ordering::Relaxed),
                        };
                    }
                })
            })
            .collect();
        let mut seen: HashMap<Vec<Answer>, usize> = HashMap::new();
        let mut listed_otherwise = 0usize;
        let mut asked = 0usize;
        let mut slowest = Duration::ZERO;
        let started = StdRuntime.now();
        while StdRuntime.elapsed_since(started) < Duration::from_secs(seconds) {
            let asking = StdRuntime.now();
            let now = answers();
            slowest = slowest.max(StdRuntime.elapsed_since(asking));
            asked += 1;
            if now != before && !seen.contains_key(&now) {
                println!(
                    "at {:.3} s, after {} frames: {now:?}",
                    StdRuntime.elapsed_since(started).as_secs_f64(),
                    frames.load(Ordering::Relaxed)
                );
            }
            *seen.entry(now).or_default() += 1;
            let listed = decoders();
            if listed != listed_before {
                listed_otherwise += 1;
                println!(
                    "at {:.3} s, the backend's list: {listed:?}",
                    StdRuntime.elapsed_since(started).as_secs_f64()
                );
            }
        }
        stop.store(true, Ordering::Relaxed);
        for thread in threads {
            let _ = thread.join();
        }
        let after = answers();
        println!(
            "{asked} answers in {:.1} s, the slowest {:.1} ms; {} sessions opened, {} frames \
             decoded, {} failures; {} distinct answers, {} as before the load; the backend's \
             list otherwise {listed_otherwise} times",
            StdRuntime.elapsed_since(started).as_secs_f64(),
            slowest.as_secs_f64() * 1000.0,
            sessions.load(Ordering::Relaxed),
            frames.load(Ordering::Relaxed),
            failures.load(Ordering::Relaxed),
            seen.len(),
            seen.get(&before).copied().unwrap_or(0)
        );
        println!("after the load: {after:?}");
        assert_eq!(
            listed_otherwise, 0,
            "the backend's list changed under the load"
        );
        assert_eq!(after, before);
        Ok(())
    }

    /// A second probe: the process's first queries, asked by eight
    /// threads at once, as a test binary's first tests ask them when each
    /// makes a backend, VP9's decoder registered by whichever gets there
    /// first. Each thread's answers, ten in a row, are compared with the
    /// answer once all are done. Run it in many fresh processes, since only
    /// a process's first queries race:
    ///
    /// ```sh
    /// for i in $(seq 200); do TEST_BINARY --ignored --exact \
    ///   macos::capabilities::tests::first_decoder_queries_at_once; done
    /// ```
    #[test]
    #[ignore = "a probe, run by hand in many processes: a process's first decoder queries at once"]
    #[expect(
        clippy::disallowed_methods,
        reason = "test scaffolding: std threads that ask at once, as a test binary's first tests do"
    )]
    fn first_decoder_queries_at_once() {
        let start = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    (0..10).map(|_| answers()).collect::<Vec<_>>()
                })
            })
            .collect();
        let asked: Vec<Vec<Vec<Answer>>> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap_or_default())
            .collect();
        let settled = answers();
        let mut differ = 0;
        for (thread, answers) in asked.iter().enumerate() {
            for (index, answer) in answers.iter().enumerate() {
                if *answer != settled {
                    differ += 1;
                    println!("a first answer differs: thread {thread}, {index}: {answer:?}");
                }
            }
        }
        println!("the settled answer: {settled:?}; {differ} of the 80 first answers differ");
        assert_eq!(differ, 0);
    }
}
