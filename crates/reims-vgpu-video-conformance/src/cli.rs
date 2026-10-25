//! The command line each backend's `conformance` example runs. The suite
//! is the same everywhere; the example passes the opener of its backend,
//! which takes the device the command line names (a render node, a GPU's
//! index, or nothing where a backend has one device).
//!
//! ```text
//! conformance DEVICE summary
//! conformance DEVICE framemd5 STREAM [--async] [--description WxH [--hash WxH]] [--sequence FILE]
//! conformance DEVICE decode-check STREAM REFERENCE [STREAM REFERENCE]...
//! conformance DEVICE encode INPUT WxH PREFIX [OPTION]...
//! conformance DEVICE encode-check [--case PREFIX]...
//! conformance DEVICE jpeg-check [OPTION]... FILE.jpg
//! conformance DEVICE motion-check [--case PREFIX]...
//! ```
//!
//! - `summary` prints what the backend offers, as the host's log has it.
//! - `framemd5` decodes one MP4 through the host session and prints `frame
//!   INDEX id SAMPLE MD5` for each frame emitted, in the order it came;
//!   with `--sequence`, it writes the host's messages in the order they
//!   came to FILE, one a line ([`crate::decode::DecodeRun::sequence`]).
//! - `decode-check` decodes each stream synchronously and then
//!   asynchronously, and compares its frames with the reference's as a set
//!   ([`crate::suite`]), printing one `PASS` or `FAIL` line a run. It
//!   exits 1 when any run fails.
//! - `encode` encodes raw frames through the host session and writes what
//!   the guest got ([`crate::encode`]).
//! - `encode-check` draws frames, encodes them and checks what comes back
//!   ([`crate::encode_check`]). It exits 1 when any case fails, or when the
//!   backend lists no encoder.
//! - `jpeg-check` decodes one JPEG through the JPEG engine's decoder and
//!   compares it with reference planes ([`crate::jpeg`]).
//! - `motion-check` estimates frames of known motion through the host
//!   session ([`crate::motion::check`]), or the cases whose names start
//!   with a `--case` prefix. It exits 1 when any case fails, or when the
//!   backend has no motion estimation.
//!
//! `--no-zero-copy`, anywhere on the line, has the CPU copy every frame of
//! the host sessions' decodes into the guest's buffers, which a backend's
//! device writes itself otherwise, so that two runs compare the two
//! ([`crate::decode::set_zero_copy`]).

use std::path::Path;
use std::process::ExitCode;

use reims_vgpu_video_core::backend::Backend;

use crate::decode::decode_file_described;
use crate::suite::check_decode;

/// Opens the backend on the device the command line names.
pub type Open<'a> = &'a dyn Fn(&str) -> Result<Backend, String>;

/// Runs the command line in `args` (without the program's name) on the
/// backend `open` makes, as `program`.
pub fn run(program: &str, args: &[String], open: Open<'_>) -> ExitCode {
    log_to_stderr();
    if args.iter().any(|arg| arg == "--no-zero-copy") {
        crate::decode::set_zero_copy(false);
    }
    let args: Vec<String> = (args.iter())
        .filter(|arg| *arg != "--no-zero-copy")
        .cloned()
        .collect();
    let usage = || {
        eprintln!(
            "usage: {program} DEVICE summary\n       \
             {program} DEVICE framemd5 STREAM [--async] [--description WxH [--hash WxH]] [--sequence FILE]\n       \
             {program} DEVICE decode-check STREAM REFERENCE [STREAM REFERENCE]...\n       \
             {program} DEVICE encode INPUT WxH PREFIX [OPTION]...\n       \
             {program} DEVICE encode-check [--case PREFIX]...\n       \
             {program} DEVICE jpeg-check [OPTION]... FILE.jpg\n       \
             {program} DEVICE motion-check [--case PREFIX]...\n\
             --no-zero-copy, anywhere, has the CPU copy every decoded frame"
        );
        ExitCode::from(2)
    };
    let [device, command, rest @ ..] = args.as_slice() else {
        return usage();
    };
    // These open the backend themselves.
    match command.as_str() {
        "jpeg-check" => {
            let mut args = rest.to_vec();
            args.insert(args.len().saturating_sub(1), device.clone());
            return crate::jpeg::run_args(&args, open);
        }
        "encode" => {
            let mut args = vec![device.clone()];
            args.extend_from_slice(rest);
            return crate::encode::run_args(program, &args, open);
        }
        _ => {}
    }
    let backend = match open(device) {
        Ok(backend) => backend,
        Err(error) => {
            eprintln!("{program}: {device}: {error}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("{}", backend.summary());
    match command.as_str() {
        "summary" if rest.is_empty() => {
            println!("{}", backend.summary());
            ExitCode::SUCCESS
        }
        "framemd5" => match framemd5(&backend, rest) {
            Some(code) => code,
            None => usage(),
        },
        "decode-check" if !rest.is_empty() && rest.len() % 2 == 0 => {
            let mut failed = false;
            for [stream, reference] in rest.as_chunks::<2>().0 {
                for asynchronous in [false, true] {
                    let (passed, line) = check_decode(
                        &backend,
                        Path::new(stream),
                        Path::new(reference),
                        asynchronous,
                    );
                    println!("{line}");
                    failed |= !passed;
                }
            }
            if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        "encode-check" => {
            let mut only = Vec::new();
            let mut args = rest.iter();
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--case" => only.extend(args.next().cloned()),
                    _ => return usage(),
                }
            }
            let cases: Vec<_> = (crate::encode_check::cases(&backend).into_iter())
                .filter(|case| only.is_empty() || only.iter().any(|p| case.name.starts_with(p)))
                .collect();
            if cases.is_empty() {
                println!(
                    "result: {device} no encode case: {} lists no encoder",
                    backend.name
                );
                return ExitCode::FAILURE;
            }
            let mut passed = 0;
            for case in &cases {
                let (ok, line) = crate::encode_check::check(&backend, case);
                println!("{line}");
                passed += usize::from(ok);
            }
            println!("result: {device} encode {passed} of {} passed", cases.len());
            if passed == cases.len() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        "motion-check" => {
            let mut only = Vec::new();
            let mut args = rest.iter();
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--case" => only.extend(args.next().cloned()),
                    _ => return usage(),
                }
            }
            if backend.motion.is_none() {
                println!(
                    "result: {device} no motion estimation: {} has none",
                    backend.name
                );
                return ExitCode::FAILURE;
            }
            let cases: Vec<_> = (crate::motion::check::cases().into_iter())
                .filter(|case| only.is_empty() || only.iter().any(|p| case.name.starts_with(p)))
                .collect();
            let mut passed = 0;
            for case in &cases {
                let (ok, line) = crate::motion::check::check(&backend, case);
                println!("{line}");
                passed += usize::from(ok);
            }
            println!("result: {device} motion {passed} of {} passed", cases.len());
            if passed == cases.len() && !cases.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        _ => usage(),
    }
}

/// `framemd5 STREAM [--async] [--description WxH [--hash WxH]] [--sequence
/// FILE]`; `None` for the wrong arguments.
fn framemd5(backend: &Backend, args: &[String]) -> Option<ExitCode> {
    let mut asynchronous = false;
    let mut described = None;
    let mut hashed = None;
    let mut sequence = None;
    let mut input = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--async" => asynchronous = true,
            "--description" => {
                let (w, h) = args.next()?.split_once('x')?;
                described = Some((w.parse().ok()?, h.parse().ok()?));
            }
            "--hash" => {
                let (w, h) = args.next()?.split_once('x')?;
                hashed = Some((w.parse().ok()?, h.parse().ok()?));
            }
            "--sequence" => sequence = Some(args.next()?),
            _ if input.is_none() => input = Some(arg),
            _ => return None,
        }
    }
    let input = input?;
    let file = match std::fs::read(input) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("{input}: {error}");
            return Some(ExitCode::FAILURE);
        }
    };
    Some(
        match decode_file_described(backend, &file, asynchronous, described, hashed) {
            Ok(run) => {
                for frame in &run.frames {
                    match frame.written {
                        Some((columns, rows)) => println!(
                            "frame {} id {} {} written {columns}x{rows}",
                            frame.index, frame.frame_id, frame.md5
                        ),
                        None => {
                            println!("frame {} id {} {}", frame.index, frame.frame_id, frame.md5)
                        }
                    }
                }
                eprintln!("{}", run.summary());
                let mut lines = run.sequence.join("\n");
                lines.push('\n');
                match sequence.map(|path| std::fs::write(path, lines)) {
                    Some(Err(error)) => {
                        eprintln!("framemd5: {}: {error}", sequence.map_or("", |p| p.as_str()));
                        ExitCode::FAILURE
                    }
                    _ => ExitCode::SUCCESS,
                }
            }
            Err(error) => {
                eprintln!("framemd5: {error}");
                ExitCode::FAILURE
            }
        },
    )
}

/// Core's and the backend's log on stderr, from the level `RVGV_LOG` names
/// (`error`, `warn`, `info`, `debug` or `trace`) up, so that a run shows
/// what a QEMU's log would: the commands set up no logger otherwise.
fn log_to_stderr() {
    struct Stderr;
    impl log::Log for Stderr {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= log::max_level()
        }
        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                eprintln!("[{} {}] {}", record.level(), record.target(), record.args());
            }
        }
        fn flush(&self) {}
    }
    static STDERR: Stderr = Stderr;
    let Some(level) = std::env::var("RVGV_LOG")
        .ok()
        .and_then(|level| level.parse::<log::LevelFilter>().ok())
    else {
        return;
    };
    if log::set_logger(&STDERR).is_ok() {
        log::set_max_level(level);
    }
}
