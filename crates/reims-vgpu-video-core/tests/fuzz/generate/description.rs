//! ImageDescriptions as a guest's `dsta`, `dcan` and `Efds` reply carry
//! them: core's own writer's, with extension atoms of any kind, then
//! mutated.

use reims_vgpu_video_core::fourcc::FourCc;
use reims_vgpu_video_core::wire::image_description::ImageDescription;

use crate::generate::sample_buffer::{codec, record_name};
use crate::support::{Rng, maybe_mutate};

/// A description of `codec` at `size`, with `record` as its
/// configuration record when one is given.
pub fn description(
    rng: &mut Rng,
    codec: FourCc,
    size: (u16, u16),
    record: Option<Vec<u8>>,
) -> Vec<u8> {
    let mut extensions = Vec::new();
    for _ in 0..rng.count(3) {
        let kind = if rng.bool() {
            FourCc::new(*rng.pick(&[*b"avcC", *b"hvcC", *b"av1C", *b"vpcC", *b"btrt", *b"colr"]))
        } else {
            FourCc(rng.next_u32())
        };
        extensions.push((kind, rng.some_bytes(48)));
    }
    if let Some(record) = record {
        let name = record_name(codec).as_bytes();
        let kind = FourCc::new([name[0], name[1], name[2], name[3]]);
        let at = rng.below(extensions.len() + 1);
        extensions.insert(at, (kind, record));
    }
    ImageDescription {
        codec,
        width: size.0,
        height: size.1,
        depth: 24,
        compressor_name: rng.some_bytes(40),
        extensions,
    }
    .to_bytes()
}

/// A size, small most often, now and then at the ends of 16 bits.
pub fn size(rng: &mut Rng) -> (u16, u16) {
    let side = |rng: &mut Rng| {
        if rng.one_in(6) {
            *rng.pick(&[0, 1, 63, 64, 65, 4096, 8192, 16384, 16385, u16::MAX])
        } else {
            rng.range(16, 640) as u16
        }
    };
    (side(rng), side(rng))
}

/// Description bytes of anything, mutated most often.
pub fn bytes(rng: &mut Rng) -> Vec<u8> {
    let codec = codec(rng);
    let size = size(rng);
    let record = rng.bool().then(|| rng.some_bytes(64));
    let mut bytes = description(rng, codec, size, record);
    maybe_mutate(rng, &mut bytes, b"avcChvcCav1CvpcC", 3);
    bytes
}
