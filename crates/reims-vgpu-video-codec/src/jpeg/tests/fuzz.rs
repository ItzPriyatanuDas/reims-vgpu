//! JPEG's parser and scan walk under seeded fuzzing. The images are the
//! tests' own segment writer's: tables, frame headers of every process,
//! scans and restart intervals, in an order and with values that stray as
//! far as each image's noise says, then mutated. An image that parses is
//! checked against what VA-API's decoder reads of it: every table its
//! scan and frame name is there, and within what an 8-bit baseline
//! decode takes. Its scan is then walked, as VA-API's decoder walks it
//! before the hardware sees it. A third of the images carry a scan of the
//! walk's own tests' writer (`entropy::tests`), whose data decodes,
//! damaged token by token, so that the walk reads deep into it.

use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::fuzz_support::{Rng, Target, long, maybe_mutate, mutate_rounds, quick};

const IMAGES: Target = Target::new("jpeg-images");

/// Images that parsed, and scans walked to their end, which say how deep
/// the inputs get.
static PARSED: AtomicU64 = AtomicU64::new(0);
static WALKED: AtomicU64 = AtomicU64::new(0);

#[test]
fn images() {
    let report = quick(&IMAGES, 20_000, image_case);
    eprintln!(
        "jpeg: {} of {} images parsed, {} scans walked",
        PARSED.load(Ordering::Relaxed),
        report.inputs,
        WALKED.load(Ordering::Relaxed)
    );
}

#[test]
#[ignore = "long: RVGV_FUZZ_CASES, RVGV_FUZZ_SECONDS, RVGV_FUZZ_SEED"]
fn images_long() {
    let report = long(&IMAGES, image_case);
    eprintln!(
        "jpeg: {} of {} images parsed, {} scans walked",
        PARSED.load(Ordering::Relaxed),
        report.inputs,
        WALKED.load(Ordering::Relaxed)
    );
}

/// How far an image strays: 0 not at all, 1 now and then, 2 often.
#[derive(Clone, Copy)]
struct Noise(u8);

impl Noise {
    fn of(rng: &mut Rng) -> Self {
        Self(rng.below(3) as u8)
    }

    fn strays(self, rng: &mut Rng) -> bool {
        match self.0 {
            0 => false,
            1 => rng.one_in(16),
            _ => rng.one_in(4),
        }
    }
}

/// A Huffman table's payload: Annex K's, or counts and values of any
/// shape, up to the 256 values a DHT may hold.
fn huffman(rng: &mut Rng, noise: Noise, class: u8) -> (Vec<u8>, Vec<u8>) {
    if !noise.strays(rng) {
        let table = match (class, rng.bool()) {
            (0, true) => DEFAULT_DC_LUMINANCE,
            (0, false) => DEFAULT_DC_CHROMINANCE,
            (_, true) => DEFAULT_AC_LUMINANCE,
            (_, false) => DEFAULT_AC_CHROMINANCE,
        };
        return (table.counts.to_vec(), table.values.to_vec());
    }
    let mut counts = [0u8; 16];
    let total = rng.below(260);
    for _ in 0..total {
        let length = rng.below(16);
        counts[length] = counts[length].saturating_add(1);
    }
    let values = counts.iter().map(|&c| usize::from(c)).sum();
    (counts.to_vec(), rng.bytes(values))
}

/// One image, segment by segment.
fn image(rng: &mut Rng, noise: Noise) -> Vec<u8> {
    let components: &[(u8, u8, u8, u8)] = match rng.below(4) {
        0 => &[(1, 1, 1, 0)],
        1 => &YUV420,
        2 => &[(1, 2, 1, 0), (2, 1, 1, 1), (3, 1, 1, 1)],
        _ => &[(1, 1, 1, 0), (2, 1, 1, 1), (3, 1, 1, 1)],
    };
    let mut components = components.to_vec();
    if noise.strays(rng) {
        components = (0..rng.range(0, 5))
            .map(|_| {
                (
                    rng.below(256) as u8,
                    rng.below(6) as u8,
                    rng.below(6) as u8,
                    rng.below(5) as u8,
                )
            })
            .collect();
    }
    let size = |rng: &mut Rng| {
        if noise.strays(rng) {
            *rng.pick(&[0, 1, 7, 8, 9, 16, 17, 4096, 16384, 65535])
        } else {
            rng.range(1, 700) as u16
        }
    };
    let (width, height) = (size(rng), size(rng));
    let mut w = Writer::soi();
    if rng.bool() {
        w = w.jfif();
    }
    if rng.one_in(6) {
        // Adobe's APP14, with a transform.
        w = w.segment(
            0xee,
            &[
                b'A',
                b'd',
                b'o',
                b'b',
                b'e',
                0,
                100,
                0,
                0,
                0,
                0,
                rng.below(4) as u8,
            ],
        );
    }
    for id in 0..2 + rng.count(2) as u8 {
        if rng.one_in(4) {
            let mut payload = vec![0x10 | id];
            payload.extend((0..64).flat_map(|_| (rng.range(1, 600) as u16).to_be_bytes()));
            w = w.segment(0xdb, &payload);
        } else {
            w = w.dqt(id, rng.range(1, 255) as u8);
        }
    }
    if rng.below(4) != 0 {
        for (class, id) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            let (counts, values) = huffman(rng, noise, class);
            let mut payload = vec![(class << 4) | id];
            payload.extend(counts);
            payload.extend(values);
            w = w.segment(0xc4, &payload);
        }
    }
    if rng.one_in(4) {
        w = w.segment(0xdd, &(rng.below(64) as u16).to_be_bytes());
    }
    let marker = if noise.strays(rng) {
        0xc0 + rng.below(16) as u8
    } else {
        *rng.pick(&[0xc0, 0xc0, 0xc1])
    };
    let precision = if noise.strays(rng) {
        *rng.pick(&[8, 12, 16, 0])
    } else {
        8
    };
    w = w.sof(marker, precision, width, height, &components);
    if rng.one_in(8) {
        w = w.segment(0xfe, b"a comment");
    }
    let mut scan: Vec<(u8, u8, u8)> = components
        .iter()
        .map(|&(id, _, _, tq)| (id, tq.min(1), tq.min(1)))
        .collect();
    if noise.strays(rng) {
        scan = (0..rng.range(0, 5))
            .map(|_| (rng.below(256) as u8, rng.below(5) as u8, rng.below(5) as u8))
            .collect();
    }
    w = w.sos(&scan);
    let mut entropy = rng.some_bytes(200);
    // Stuffed bytes and restart markers, as an encoder writes them.
    for _ in 0..rng.count(4) {
        let at = rng.below(entropy.len() + 1);
        let marker = *rng.pick(&[0x00, 0xd0, 0xd1, 0xd7, 0xff]);
        entropy.splice(at..at, [0xff, marker]);
    }
    w = w.bytes(&entropy);
    if rng.one_in(8) {
        // A second scan, or a DNL, after the first.
        w = w.segment(*rng.pick(&[0xda, 0xdc]), &[1, 1, 0, 0, 63, 0]);
    }
    if noise.strays(rng) && rng.bool() {
        w.0
    } else {
        w.eoi()
    }
}

/// An image whose scan decodes, of the walk's tests' writer, grey or
/// 4:2:0, with or without restart intervals, then damaged token by token:
/// a token dropped or repeated, bits, markers or bytes put among them, the
/// padding of either bit, no EOI, and bytes after the end.
fn coded_image(rng: &mut Rng) -> Vec<u8> {
    use crate::jpeg::entropy::tests::{Case, Kind, Token};
    let kind = if rng.bool() { Kind::Grey } else { Kind::Yuv420 };
    let side = |rng: &mut Rng| rng.range(1, 80) as u16;
    let (width, height) = (side(rng), side(rng));
    let mut case = Case::valid(kind, width, height, *rng.pick(&[0, 0, 1, 2, 3, 7]));
    for _ in 0..rng.count(4) {
        if case.tokens.is_empty() {
            break;
        }
        let at = rng.below(case.tokens.len());
        match rng.below(5) {
            0 => {
                case.tokens.remove(at);
            }
            1 => {
                let token = case.tokens[at].clone();
                case.tokens.insert(at, token);
            }
            2 => case.tokens[at] = Token::Bits(rng.next_u32(), rng.range(1, 16) as u32),
            3 => {
                let marker = *rng.pick(&[0xd0, 0xd1, 0xd7, 0xd9, 0xda, 0x00, 0xff]);
                case.tokens.insert(at, Token::Marker(marker));
            }
            _ => case.tokens.insert(at, Token::Bytes(rng.some_bytes(8))),
        }
    }
    case.pad_with_ones = !rng.one_in(4);
    case.eoi = !rng.one_in(8);
    if rng.one_in(3) {
        case.trailer = rng.some_bytes(40);
    }
    case.bytes()
}

/// An image parses or is refused; one that parses names only tables it
/// has, within what an 8-bit baseline decode takes, and its scan walks or
/// is refused.
fn image_case(rng: &mut Rng) {
    let noise = Noise::of(rng);
    let mut bytes = if rng.one_in(3) {
        coded_image(rng)
    } else {
        image(rng, noise)
    };
    if noise.strays(rng) {
        let donor = bytes.clone();
        let rounds = 1 + rng.count(6);
        mutate_rounds(rng, &mut bytes, &donor, rounds);
    } else if rng.one_in(4) {
        let donor = bytes.clone();
        maybe_mutate(rng, &mut bytes, &donor, 1);
    }
    let Ok(image) = parse(&bytes) else {
        return;
    };
    PARSED.fetch_add(1, Ordering::Relaxed);
    let frame = &image.frame;
    assert!(frame.width > 0 && frame.height > 0, "an image of no size");
    assert!(!frame.components.is_empty(), "a frame of no components");
    for component in &frame.components {
        assert!(
            image
                .quantization_tables
                .get(usize::from(component.quantization_table))
                .is_some_and(Option::is_some),
            "a component whose quantization table is not there"
        );
    }
    for component in &image.scan.components {
        assert!(
            frame.components.iter().any(|c| c.id == component.selector),
            "a scan component of no frame component"
        );
        let table = |tables: &'_ [Option<HuffmanTable<'_>>; 4], index: u8| {
            tables
                .get(usize::from(index))
                .and_then(Option::as_ref)
                .map(|t| t.values.len())
        };
        let dc = table(&image.dc_tables, component.dc_table);
        let ac = table(&image.ac_tables, component.ac_table);
        if image.default_huffman_tables {
            assert!(component.dc_table < 2 && component.ac_table < 2);
        } else {
            assert!(
                dc.is_some_and(|values| (1..=12).contains(&values)),
                "a scan's DC table missing, empty or past 12 codes"
            );
            assert!(
                ac.is_some_and(|values| (1..=162).contains(&values)),
                "a scan's AC table missing, empty or past 162 codes"
            );
        }
    }
    let _ = frame.subsampling();
    let _ = frame.mcu_count();
    for index in 0..frame.components.len() {
        let _ = frame.component_size(index);
    }
    if walk(&image).is_ok() {
        WALKED.fetch_add(1, Ordering::Relaxed);
    }
}
