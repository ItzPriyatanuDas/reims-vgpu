//! Decodes a VP9 stream with [`reims_vgpu_video_vaapi::Vp9Decoder`] and
//! prints each frame's MD5 in the order the decoder shows them. By default
//! it hashes the layout ffmpeg's own VP9 decoder outputs, `yuv420p` (or
//! `yuv420p10le`): the Y, Cb and Cr planes apart, 10-bit samples in the
//! bottom bits of their words. So `ffmpeg -c:v vp9 -f framemd5` prints the
//! same for the same stream, with no conversion on ffmpeg's side, and a
//! software decode is the reference (tools/vp9-check.sh). `--packed`
//! hashes the NV12 or P010 planes as read back instead.
//!
//! ```sh
//! vp9-framemd5 /dev/dri/renderD128 stream.ivf [options]
//! ```
//!
//! - An `.mp4` is read as VideoToolbox is handed a stream: the `vpcC`
//!   record, checked first, and each sample as stored. Anything else is an
//!   IVF file.
//! - `--stride-pad N` reads back into planes with `N` spare bytes a row.
//! - `--hold N` keeps the last `N` frames alive, as a caller that is slow
//!   to drop them would.
//! - `--reset-at N` forgets every reference before sample `N`, as a seek
//!   would; sample `N` must start with a key frame.
//! - `--dump FILE` also writes each frame's hashed bytes to `FILE`.
//!
//! Prints `frame <index> id <sample> <width>x<height> <md5>` per frame, and
//! the time spent to stderr.

#[cfg(target_os = "linux")]
use reims_vgpu_video_conformance::{ivf, md5, mp4};

#[cfg(target_os = "linux")]
#[path = "../common/readback.rs"]
mod readback;

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    readback::read_back_as_asked();
    match linux::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vp9-framemd5: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("vp9-framemd5 needs Linux and libva");
}

#[cfg(target_os = "linux")]
mod linux {
    use reims_vgpu_video_core::runtime::{Runtime, StdRuntime};
    use std::collections::VecDeque;
    use std::time::Duration;

    use reims_vgpu_video_vaapi::{Display, Frame, Vp9Decoder};

    use super::md5::{Md5, self_test};
    use super::{ivf, mp4};

    struct Options {
        node: String,
        input: String,
        stride_pad: usize,
        hold: usize,
        reset_at: Option<usize>,
        packed: bool,
        dump: Option<String>,
    }

    fn options() -> Result<Options, String> {
        let mut args = std::env::args().skip(1);
        let mut positional = Vec::new();
        let mut options = Options {
            node: String::new(),
            input: String::new(),
            stride_pad: 0,
            hold: 0,
            reset_at: None,
            packed: false,
            dump: None,
        };
        while let Some(arg) = args.next() {
            let mut number = |name: &str| -> Result<usize, String> {
                args.next()
                    .and_then(|v| v.parse().ok())
                    .ok_or(format!("{name} takes a number"))
            };
            match arg.as_str() {
                "--stride-pad" => options.stride_pad = number("--stride-pad")?,
                "--hold" => options.hold = number("--hold")?,
                "--reset-at" => options.reset_at = Some(number("--reset-at")?),
                "--packed" => options.packed = true,
                "--dump" => options.dump = Some(args.next().ok_or("--dump takes a file")?),
                _ => positional.push(arg),
            }
        }
        let [node, input] = <[String; 2]>::try_from(positional)
            .map_err(|_| "usage: vp9-framemd5 <render node> <stream> [options]")?;
        options.node = node;
        options.input = input;
        Ok(options)
    }

    struct Reader {
        y: Vec<u8>,
        uv: Vec<u8>,
        pad: usize,
        packed: bool,
        dump: Option<std::fs::File>,
        index: usize,
        readback: Duration,
    }

    impl Reader {
        fn emit(&mut self, frame: &Frame) -> Result<(), String> {
            let (w, h) = (frame.width() as usize, frame.height() as usize);
            let sample = if frame.bit_depth() > 8 { 2 } else { 1 };
            // A row of the chroma plane holds a Cb/Cr pair for every two
            // luma samples, the last one too when the width is odd.
            let (y_row, uv_row) = (w * sample, w.div_ceil(2) * 2 * sample);
            let (y_stride, uv_stride) = (y_row + self.pad, uv_row + self.pad);
            let uv_rows = h.div_ceil(2);
            self.y.resize(y_stride * h, 0);
            self.uv.resize(uv_stride * uv_rows, 0);
            let start = StdRuntime.now();
            if sample == 2 {
                frame.copy_p010_to(&mut self.y, y_stride, &mut self.uv, uv_stride)
            } else {
                frame.copy_nv12_to(&mut self.y, y_stride, &mut self.uv, uv_stride)
            }
            .map_err(|e| e.to_string())?;
            self.readback += StdRuntime.elapsed_since(start);
            let mut md5 = Hashed {
                md5: Md5::new(),
                dump: self.dump.as_mut(),
            };
            // yuv420p10le keeps 10 bits at the bottom of each word, where
            // P010 keeps them at the top.
            let low_bits = sample == 2 && !self.packed;
            for row in self.y.chunks(y_stride).take(h) {
                md5.update_samples(&row[..y_row], sample, low_bits);
            }
            if self.packed {
                for row in self.uv.chunks(uv_stride).take(uv_rows) {
                    md5.update(&row[..uv_row]);
                }
            } else {
                for component in 0..2 {
                    for row in self.uv.chunks(uv_stride).take(uv_rows) {
                        let plane: Vec<u8> = row[..uv_row]
                            .chunks(sample)
                            .skip(component)
                            .step_by(2)
                            .flatten()
                            .copied()
                            .collect();
                        md5.update_samples(&plane, sample, low_bits);
                    }
                }
            }
            println!(
                "frame {} id {} {}x{} {}",
                self.index,
                frame.id(),
                w,
                h,
                md5.md5.hex()
            );
            self.index += 1;
            Ok(())
        }
    }

    /// The MD5 of what a frame hashes, and a copy of it in the dump file.
    struct Hashed<'a> {
        md5: Md5,
        dump: Option<&'a mut std::fs::File>,
    }

    impl Hashed<'_> {
        fn update(&mut self, bytes: &[u8]) {
            self.md5.update(bytes);
            if let Some(file) = self.dump.as_mut() {
                let _ = std::io::Write::write_all(file, bytes);
            }
        }

        /// Hashes a row of `sample`-byte samples, a 16-bit one's 10 bits
        /// moved from the top of the word to the bottom when `low_bits`.
        fn update_samples(&mut self, row: &[u8], sample: usize, low_bits: bool) {
            if sample == 2 && low_bits {
                let low: Vec<u8> = row
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .flat_map(|v| (u16::from_le_bytes(*v) >> 6).to_le_bytes())
                    .collect();
                self.update(&low);
            } else {
                self.update(row);
            }
        }
    }

    pub(crate) fn run() -> Result<(), String> {
        if !self_test() {
            return Err("MD5 fails its own test vectors".into());
        }
        let options = options()?;
        let display =
            Display::open(&options.node, StdRuntime::shared()).map_err(|e| e.to_string())?;
        let file = std::fs::read(&options.input).map_err(|e| format!("{}: {e}", options.input))?;
        let (mut decoder, samples) = if options.input.ends_with(".mp4") {
            let track = mp4::vp9_track(&file)?;
            let decoder = Vp9Decoder::new(&display, &track.avcc).map_err(|e| e.to_string())?;
            (decoder, track.samples)
        } else {
            (Vp9Decoder::new_stream(&display), ivf::vp9_frames(&file)?)
        };
        let mut reader = Reader {
            y: Vec::new(),
            uv: Vec::new(),
            pad: options.stride_pad,
            packed: options.packed,
            dump: options
                .dump
                .as_ref()
                .map(std::fs::File::create)
                .transpose()
                .map_err(|e| format!("--dump: {e}"))?,
            index: 0,
            readback: Duration::ZERO,
        };
        let mut held = VecDeque::new();
        let start = StdRuntime.now();
        for (id, &(offset, size)) in samples.iter().enumerate() {
            if options.reset_at == Some(id) {
                decoder.reset();
            }
            let sample = file
                .get(offset..offset + size)
                .ok_or("sample past the file's end")?;
            let frames = decoder
                .decode(sample, id as u64)
                .map_err(|e| format!("sample {id}: {e}"))?;
            for frame in frames {
                reader.emit(&frame)?;
                if options.hold > 0 {
                    held.push_back(frame);
                    if held.len() > options.hold {
                        held.pop_front();
                    }
                }
            }
        }
        let total = StdRuntime.elapsed_since(start);
        eprintln!(
            "{} samples, {} frames, profile {}, {:.3} s ({:.1} fps), readback {:.3} s",
            samples.len(),
            reader.index,
            decoder.profile().and_then(|p| p.name()).unwrap_or("none"),
            total.as_secs_f64(),
            reader.index as f64 / total.as_secs_f64(),
            reader.readback.as_secs_f64()
        );
        Ok(())
    }
}
