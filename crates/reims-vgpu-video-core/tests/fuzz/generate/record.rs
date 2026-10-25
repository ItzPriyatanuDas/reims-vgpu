//! Configuration records as a guest's `dsta` and `dcan` carry them:
//! `avcC` and `hvcC` records whose first SPS is written from scratch, with
//! fields of any value and coded sizes about Apple's smallest, 64 a side,
//! most often, then mutated, so that core's readers of the coded size get
//! past the record's header. Other codecs' records are bytes of anything,
//! as before.

use reims_vgpu_video_core::fourcc::FourCc;

use crate::support::{Rng, maybe_mutate};

/// A record for `codec`.
pub fn record(rng: &mut Rng, codec: FourCc) -> Vec<u8> {
    let mut record = match &codec.to_be_bytes() {
        b"avc1" => avcc(rng),
        b"hvc1" => hvcc(rng),
        _ => return rng.some_bytes(40),
    };
    let donor = record.clone();
    maybe_mutate(rng, &mut record, &donor, 2);
    record
}

/// Fields written most significant bit first, then the stop bit and
/// emulation prevention.
#[derive(Default)]
struct Writer {
    bits: Vec<bool>,
}

impl Writer {
    fn u(&mut self, n: u32, value: u64) {
        for i in (0..n.min(64)).rev() {
            self.bits.push((value >> i) & 1 == 1);
        }
    }

    fn ue(&mut self, value: u32) {
        let code = u64::from(value) + 1;
        let len = 64 - code.leading_zeros();
        self.u(len - 1, 0);
        self.u(len, code);
    }

    fn se(&mut self, value: i32) {
        let magnitude = u64::from(value.unsigned_abs());
        let code = if value > 0 {
            2 * magnitude - 1
        } else {
            2 * magnitude
        };
        self.ue(u32::try_from(code).unwrap_or(u32::MAX - 1));
    }

    /// The RBSP with its stop bit, escaped.
    fn finish(mut self) -> Vec<u8> {
        self.bits.push(true);
        while !self.bits.len().is_multiple_of(8) {
            self.bits.push(false);
        }
        let mut out = Vec::new();
        let mut zeros = 0;
        for byte in self.bits.chunks(8) {
            let byte = byte.iter().fold(0u8, |acc, &b| (acc << 1) | u8::from(b));
            if zeros >= 2 && byte <= 3 {
                out.push(3);
                zeros = 0;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            out.push(byte);
        }
        out
    }
}

/// A small value most often, now and then any.
fn small(rng: &mut Rng, below: u32) -> u32 {
    if rng.one_in(12) {
        rng.interesting_u32().min(u32::MAX - 1)
    } else {
        rng.range_u32(0, below)
    }
}

/// A side in units of `unit` samples, about 64 samples most often, less
/// one as the SPS codes it where `minus1`.
fn side(rng: &mut Rng, unit: u32, minus1: bool) -> u32 {
    let samples = match rng.below(4) {
        0 => rng.range_u32(1, 4096),
        1 => rng.interesting_u32().min(u32::MAX - 1),
        _ => rng.range_u32(40, 80),
    };
    let units = samples / unit.max(1);
    if minus1 {
        units.saturating_sub(1)
    } else {
        units * unit.max(1)
    }
}

/// An H.264 SPS NAL unit (7.3.2.1.1), escaped.
fn avc_sps(rng: &mut Rng) -> Vec<u8> {
    let profile_idc = if rng.one_in(8) {
        rng.next_u32() as u8
    } else {
        *rng.pick(&[66u8, 77, 88, 100, 110, 122, 244, 44, 83, 118, 128])
    };
    let mut w = Writer::default();
    w.ue(small(rng, 32));
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma_format_idc = small(rng, 4);
        w.ue(chroma_format_idc);
        if chroma_format_idc == 3 {
            w.u(1, u64::from(rng.bool()));
        }
        w.ue(small(rng, 7));
        w.ue(small(rng, 7));
        w.u(1, u64::from(rng.bool()));
        let matrix = rng.one_in(3);
        w.u(1, u64::from(matrix));
        if matrix {
            for list in 0..if chroma_format_idc == 3 { 12 } else { 8 } {
                let present = rng.bool();
                w.u(1, u64::from(present));
                if present {
                    for _ in 0..rng.range(1, if list < 6 { 16 } else { 64 }) {
                        w.se(rng.range(0, 64) as i32 - 32);
                    }
                }
            }
        }
    }
    w.ue(small(rng, 13));
    let order_count_type = small(rng, 3);
    w.ue(order_count_type);
    match order_count_type {
        0 => w.ue(small(rng, 13)),
        1 => {
            w.u(1, u64::from(rng.bool()));
            w.se(rng.interesting_i32());
            w.se(rng.interesting_i32());
            let cycle = small(rng, 300);
            w.ue(cycle);
            for _ in 0..cycle.min(300) {
                w.se(rng.interesting_i32());
            }
        }
        _ => {}
    }
    w.ue(small(rng, 17));
    w.u(1, u64::from(rng.bool()));
    w.ue(side(rng, 16, true));
    w.ue(side(rng, 16, true));
    let frame_mbs_only = !rng.one_in(6);
    w.u(1, u64::from(frame_mbs_only));
    if !frame_mbs_only {
        w.u(1, u64::from(rng.bool()));
    }
    w.u(rng.range_u32(0, 24), rng.next_u64());
    let mut unit = vec![
        0x67,
        profile_idc,
        rng.next_u32() as u8,
        rng.range(10, 63) as u8,
    ];
    unit.extend(w.finish());
    unit
}

/// An `avcC` record of one SPS, now and then two, and no PPS.
fn avcc(rng: &mut Rng) -> Vec<u8> {
    let units: Vec<Vec<u8>> = (0..1 + usize::from(rng.one_in(6)))
        .map(|_| avc_sps(rng))
        .collect();
    let first = &units[0];
    let mut record = vec![
        1,
        first[1],
        first[2],
        first[3],
        0xfc | *rng.pick(&[0u8, 1, 3]),
        0xe0 | u8::try_from(units.len()).unwrap_or(1),
    ];
    for unit in &units {
        record.extend_from_slice(&u16::try_from(unit.len()).unwrap_or(0).to_be_bytes());
        record.extend_from_slice(unit);
    }
    record.push(0);
    record
}

/// An HEVC SPS NAL unit (H.265 7.3.2.2.1), through its sizes and a little
/// past them, escaped.
fn hevc_sps(rng: &mut Rng) -> Vec<u8> {
    let mut w = Writer::default();
    w.u(4, rng.below(16) as u64);
    let max_sub_layers_minus1 = if rng.one_in(10) {
        7
    } else {
        rng.below(7) as u32
    };
    w.u(3, u64::from(max_sub_layers_minus1));
    w.u(1, u64::from(rng.bool()));
    // profile_tier_level(1, max_sub_layers_minus1): the general profile,
    // most often Main or Main 10 with zeros, and its level.
    if rng.one_in(4) {
        w.u(40, rng.next_u64());
        w.u(48, rng.next_u64());
    } else {
        w.u(8, *rng.pick(&[1u64, 2, 3]));
        w.u(32, 0x6000_0000);
        w.u(48, 0x9000_0000_0000);
    }
    w.u(8, *rng.pick(&[30u64, 60, 90, 93, 120, 153, 186]));
    let present: Vec<(bool, bool)> = (0..max_sub_layers_minus1.min(7))
        .map(|_| (rng.bool(), rng.bool()))
        .collect();
    for &(profile, level) in &present {
        w.u(1, u64::from(profile));
        w.u(1, u64::from(level));
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1.min(8)..8 {
            w.u(2, rng.below(4) as u64);
        }
    }
    for &(profile, level) in &present {
        if profile {
            w.u(24, rng.next_u64());
            w.u(64, rng.next_u64());
        }
        if level {
            w.u(8, rng.below(256) as u64);
        }
    }
    w.ue(small(rng, 16));
    let chroma_format_idc = small(rng, 4);
    w.ue(chroma_format_idc);
    if chroma_format_idc == 3 {
        w.u(1, u64::from(rng.bool()));
    }
    let unit = *rng.pick(&[8u32, 8, 16, 32, 1]);
    w.ue(side(rng, unit, false));
    w.ue(side(rng, unit, false));
    w.u(rng.range_u32(0, 32), rng.next_u64());
    let mut out = vec![33 << 1, 1];
    out.extend(w.finish());
    out
}

/// An `hvcC` record (ISO/IEC 14496-15, 8.3.3.1): its fixed part, then a
/// VPS of anything now and then, the SPS, and a PPS of anything now and
/// then, each in an array of its own.
fn hvcc(rng: &mut Rng) -> Vec<u8> {
    let mut units = Vec::new();
    if rng.bool() {
        let mut vps = vec![32 << 1, 1];
        vps.extend(rng.some_bytes(16));
        units.push(vps);
    }
    units.push(hevc_sps(rng));
    if rng.one_in(3) {
        units.push(hevc_sps(rng));
    }
    if rng.bool() {
        let mut pps = vec![34 << 1, 1];
        pps.extend(rng.some_bytes(8));
        units.push(pps);
    }
    let mut record = vec![1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 93];
    record.extend_from_slice(&[0xf0, 0, 0xfc, 0xfd, 0xf8, 0xf8, 0, 0]);
    record.push(0x0c | *rng.pick(&[0u8, 1, 3]));
    record.push(u8::try_from(units.len()).unwrap_or(0));
    for unit in &units {
        record.push(0x80 | (unit[0] >> 1));
        record.extend_from_slice(&[0, 1]);
        record.extend_from_slice(&u16::try_from(unit.len()).unwrap_or(0).to_be_bytes());
        record.extend_from_slice(unit);
    }
    record
}
