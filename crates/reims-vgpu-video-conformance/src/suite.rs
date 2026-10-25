//! The checks: each run of a stream against what it should give.

use std::collections::HashMap;
use std::path::Path;

use reims_vgpu_video_core::backend::Backend;

use crate::decode::{DecodeRun, decode_file, decode_file_sized};

/// The frame hashes of a reference decode: the last field of each line
/// that is not empty or a comment (`#`), so that a bare list of MD5s and
/// ffmpeg's `framemd5` output both read.
pub fn reference_hashes(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split([' ', '\t', ',']).rfind(|f| !f.is_empty()))
        .map(str::to_owned)
        .collect()
}

/// The size each frame of a reference was hashed at, by its frame ID, when
/// every line that is not empty or a comment is `ID WxH MD5`: the sample
/// the frame was decoded from, its size, and the MD5 of that much of it.
/// A stream whose frames change size has such a reference, and each frame is
/// then hashed at its own size ([`decode_file_sized`]).
pub fn reference_sizes(text: &str) -> Option<HashMap<i64, (u32, u32)>> {
    let mut sizes = HashMap::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [id, size, _md5] = fields[..] else {
            return None;
        };
        let (width, height) = size.split_once('x')?;
        sizes.insert(
            id.parse().ok()?,
            (width.parse().ok()?, height.parse().ok()?),
        );
    }
    (!sizes.is_empty()).then_some(sizes)
}

/// How a run compares with its reference. Frames come out in decode order,
/// as VideoToolbox's decoders emit them, so they are compared as a set:
/// every hash must be the reference's, as many times. A reference of no
/// frame (an empty file, a software decode that failed) compares nothing,
/// and fails.
pub fn compare(run: &DecodeRun, reference: &[String]) -> Result<(), String> {
    if reference.is_empty() {
        return Err(format!(
            "{} frames, and a reference of none to compare them with",
            run.frames.len()
        ));
    }
    let mut got: Vec<&str> = run.frames.iter().map(|f| f.md5.as_str()).collect();
    let mut want: Vec<&str> = reference.iter().map(String::as_str).collect();
    got.sort_unstable();
    want.sort_unstable();
    if got == want {
        return Ok(());
    }
    let missing = want.iter().filter(|h| !got.contains(h)).count();
    let extra = got.iter().filter(|h| !want.contains(h)).count();
    Err(format!(
        "{} frames, reference {}: {missing} of the reference's not decoded, {extra} decoded not in it",
        got.len(),
        want.len()
    ))
}

/// One stream decoded once, synchronously or not, against its reference:
/// a line that starts `PASS` or `FAIL`, and whether it passed.
pub fn check_decode(
    backend: &Backend,
    stream: &Path,
    reference: &Path,
    asynchronous: bool,
) -> (bool, String) {
    let label = format!(
        "{}.{}",
        stream.file_stem().map_or_else(
            || stream.display().to_string(),
            |s| s.to_string_lossy().into_owned()
        ),
        if asynchronous { "async" } else { "sync" }
    );
    let file = match std::fs::read(stream) {
        Ok(file) => file,
        Err(error) => {
            return (
                false,
                format!("FAIL {label}: {}: {error}", stream.display()),
            );
        }
    };
    let (reference, sizes) = match std::fs::read_to_string(reference) {
        Ok(text) => (reference_hashes(&text), reference_sizes(&text)),
        Err(error) => {
            return (
                false,
                format!("FAIL {label}: {}: {error}", reference.display()),
            );
        }
    };
    let run = match sizes {
        Some(sizes) => decode_file_sized(backend, &file, asynchronous, sizes),
        None => decode_file(backend, &file, asynchronous),
    };
    match run {
        Err(error) => (false, format!("FAIL {label}: {error}")),
        Ok(run) => match compare(&run, &reference) {
            Ok(()) => (
                true,
                format!(
                    "PASS {label}: {} frames match\n     {}",
                    run.frames.len(),
                    run.summary()
                ),
            ),
            Err(why) => (
                false,
                format!("FAIL {label}: {why}\n     {}", run.summary()),
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_read_as_bare_lists_and_as_framemd5() {
        let bare = "d41d8cd98f00b204e9800998ecf8427e\n\n900150983cd24fb0d6963f7d28e17f72\n";
        assert_eq!(
            reference_hashes(bare),
            [
                "d41d8cd98f00b204e9800998ecf8427e",
                "900150983cd24fb0d6963f7d28e17f72"
            ]
        );
        let framemd5 = "#format: frame checksums\n#stream#, dts, pts, duration, size, hash\n\
                        0,          0,          0,        1,  1382400, 3a2b0c\n";
        assert_eq!(reference_hashes(framemd5), ["3a2b0c"]);
    }

    #[test]
    fn a_reference_with_each_frames_size_reads_its_sizes() {
        let sized = "# frames of two sizes\n0 640x360 aa\n1 569x320 bb\n";
        let sizes = reference_sizes(sized);
        assert_eq!(
            sizes,
            Some(HashMap::from([(0, (640, 360)), (1, (569, 320))]))
        );
        assert_eq!(reference_hashes(sized), ["aa", "bb"]);
        // A bare list, ffmpeg's framemd5 and a line without a size have none.
        assert_eq!(reference_sizes("aa\nbb\n"), None);
        assert_eq!(
            reference_sizes("0,          0,          0,        1,  1382400, 3a2b0c\n"),
            None
        );
        assert_eq!(reference_sizes("0 640x360 aa\n1 bb cc\n"), None);
    }
}
