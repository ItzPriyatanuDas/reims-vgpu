//! Packs a software decode's planar 4:2:0 frames into NV12 or P010, each
//! at its own size, and hashes both layouts: the VA-API checks' references
//! for the stand-in guest's frames. Ours.
//!
//! ```sh
//! ffprobe -v error -c:v DECODER -i STREAM -select_streams v:0 \
//!   -show_entries frame=width,height -of csv=p=0 > SIZES
//! ffmpeg -c:v DECODER -i STREAM -noautoscale -fps_mode passthrough \
//!   -pix_fmt yuv420p|yuv420p10le -f rawvideo - | planar-packed BITS SIZES
//! ```
//!
//! BITS is 8 or 10. SIZES has one `WIDTH,HEIGHT` line a frame, in the
//! order the frames come on stdin. Each frame gives a line, `frame N WxH
//! PLANAR PACKED`: the MD5 of its planar bytes, which is ffmpeg's
//! `framemd5` of the same decode in the same format, so that a caller can
//! check the frames were cut where ffmpeg's were, and the MD5 of the frame
//! packed as the guest's buffer holds it
//! ([`reims_vgpu_video_conformance::planar`]). A size that does not read, a
//! frame cut short, or bytes left over after the last frame is an error.

use std::io::Read;

use reims_vgpu_video_conformance::md5::Md5;
use reims_vgpu_video_conformance::planar::{Depth, frame_len, pack};

fn md5(bytes: &[u8]) -> String {
    let mut md5 = Md5::new();
    md5.update(bytes);
    md5.hex()
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [bits, sizes] = <[String; 2]>::try_from(args)
        .map_err(|_| "usage: planar-packed BITS SIZES < FRAMES".to_owned())?;
    let depth = bits
        .parse()
        .ok()
        .and_then(Depth::of_bits)
        .ok_or("BITS is 8 or 10")?;
    let sizes = std::fs::read_to_string(&sizes).map_err(|e| format!("{sizes}: {e}"))?;
    let mut input = std::io::stdin().lock();
    let mut frame = Vec::new();
    for (index, line) in sizes.lines().enumerate() {
        let size = line
            .split_once(',')
            .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
            .ok_or_else(|| format!("size {index}: {line:?} is not WIDTH,HEIGHT"))?;
        let len = frame_len(size, depth).ok_or_else(|| format!("size {index}: too large"))?;
        frame.resize(len, 0);
        input
            .read_exact(&mut frame)
            .map_err(|e| format!("frame {index}, {}x{}: {e}", size.0, size.1))?;
        let packed = pack(&frame, size, depth).map_err(|e| format!("frame {index}: {e}"))?;
        println!(
            "frame {index} {}x{} {} {}",
            size.0,
            size.1,
            md5(&frame),
            md5(&packed)
        );
    }
    let mut rest = [0; 1];
    match input.read(&mut rest) {
        Ok(0) => Ok(()),
        Ok(_) => Err("bytes left after the last frame SIZES names".into()),
        Err(e) => Err(e.to_string()),
    }
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("planar-packed: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
