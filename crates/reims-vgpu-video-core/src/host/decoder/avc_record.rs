//! What core decides from an `avcC` record before any backend decodes
//! anything: the NAL unit length size, and whether the first SPS lets
//! pictures be coded as fields, which no backend here decodes. Core reads
//! just these, so that the guest sees one answer whatever the backend; the
//! codec crate, which depends on core, parses the rest.
//!
//! Apple's AVC decoder answers the same (measured on macOS 27.0.1, 26A434,
//! M3 Max, 2026-10-24, with x264's streams): `dsta` answers -12910 for
//! 3-byte lengths and decodes 1, 2 and 4; it answers -12911 for any SPS
//! with `frame_mbs_only_flag` 0, MBAFF or not, fields or frames; and
//! `dcan` answers 0 for 3-byte lengths or fields, or for another length
//! size or size than the session's. It also refuses an SPS that codes a
//! side under 64, which is its hardware's limit, not core's.

use super::rbsp::{Bits, unescape};

/// `lengthSizeMinusOne + 1` (ISO/IEC 14496-15, 5.3.3.1), or `None` for a
/// record too short to say.
pub(crate) fn length_size(record: &[u8]) -> Option<u8> {
    record.get(4).map(|byte| (byte & 3) + 1)
}

/// The NAL unit of the record's first SPS, or `None` for a record without
/// one.
fn first_sps(record: &[u8]) -> Option<&[u8]> {
    if record.get(5)? & 0x1f == 0 {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([*record.get(6)?, *record.get(7)?]));
    record.get(8..8 + length)
}

/// What core reads of an SPS (7.3.2.1.1).
struct SpsFields {
    frame_mbs_only_flag: bool,
}

/// The record's first SPS's fields, up to `frame_mbs_only_flag`, or
/// `None` when the record has no SPS, or one that ends before the flag.
fn read_sps(record: &[u8]) -> Option<SpsFields> {
    let rbsp = unescape(first_sps(record)?);
    let mut bits = Bits::new(&rbsp);
    bits.skip(8)?; // the NAL unit header
    let profile_idc = rbsp.get(1).copied()?;
    bits.skip(24)?; // profile_idc, the constraint flags, level_idc
    bits.ue()?; // seq_parameter_set_id
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma_format_idc = bits.ue()?;
        if chroma_format_idc == 3 {
            bits.skip(1)?;
        }
        bits.ue()?;
        bits.ue()?;
        bits.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        if bits.bit()? {
            for list in 0..if chroma_format_idc == 3 { 12 } else { 8 } {
                if bits.bit()? {
                    // scaling_list() (7.3.2.1.1.1), read only to pass it.
                    let size = if list < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            // delta_scale is within -128..=127 (7.4.2.1.1);
                            // one past it, which the codec crate refuses,
                            // would overflow the sum.
                            let delta = bits.se()?;
                            if !(-128..=127).contains(&delta) {
                                return None;
                            }
                            next = (last + delta).rem_euclid(256);
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    bits.ue()?; // log2_max_frame_num_minus4
    match bits.ue()? {
        0 => {
            bits.ue()?;
        }
        1 => {
            bits.skip(1)?;
            bits.se()?;
            bits.se()?;
            let cycle = bits.ue()?;
            if cycle > 255 {
                return None;
            }
            for _ in 0..cycle {
                bits.se()?;
            }
        }
        _ => {}
    }
    bits.ue()?; // max_num_ref_frames
    bits.skip(1)?; // gaps_in_frame_num_value_allowed_flag
    bits.ue()?; // pic_width_in_mbs_minus1
    bits.ue()?; // pic_height_in_map_units_minus1
    Some(SpsFields {
        frame_mbs_only_flag: bits.bit()?,
    })
}

/// Whether the record's first SPS lets pictures be fields: its
/// `frame_mbs_only_flag` is 0 (7.3.2.1.1). `None` when the record has no
/// SPS, or one that ends before the flag; the backend then refuses the
/// record as it does any it cannot read.
pub(crate) fn fields_allowed(record: &[u8]) -> Option<bool> {
    read_sps(record).map(|sps| !sps.frame_mbs_only_flag)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `avcC` record of one SPS and no PPS, with `length_size_minus_one`.
    fn record(sps: &[u8], length_size_minus_one: u8) -> Vec<u8> {
        let mut out = vec![
            1,
            sps[1],
            sps[2],
            sps[3],
            0xfc | length_size_minus_one,
            0xe1,
        ];
        out.extend_from_slice(&u16::try_from(sps.len()).unwrap_or(u16::MAX).to_be_bytes());
        out.extend_from_slice(sps);
        out.push(0);
        out
    }

    /// An SPS written from scratch: `profile_idc`, then ue and bit fields
    /// as `fields` lists them, then the stop bit.
    fn sps(profile_idc: u8, fields: &[(u32, Option<u32>)]) -> Vec<u8> {
        let mut bits: Vec<bool> = Vec::new();
        let mut put = |n: u32, value: u32| {
            for i in (0..n).rev() {
                bits.push((value >> i) & 1 == 1);
            }
        };
        put(8, 0x67);
        put(8, u32::from(profile_idc));
        put(8, 0);
        put(8, 40);
        for &(value, width) in fields {
            match width {
                // A fixed-length field of that many bits.
                Some(width) => put(width, value),
                // An Exp-Golomb code.
                None => {
                    let code = u64::from(value) + 1;
                    let len = 64 - code.leading_zeros();
                    put(len - 1, 0);
                    put(len, u32::try_from(code).unwrap_or(u32::MAX));
                }
            }
        }
        put(1, 1);
        while !bits.len().is_multiple_of(8) {
            bits.push(false);
        }
        bits.chunks(8)
            .map(|byte| byte.iter().fold(0u8, |acc, &b| (acc << 1) | u8::from(b)))
            .collect()
    }

    const UE: Option<u32> = None;
    const BIT: Option<u32> = Some(1);

    /// Main: id, frame num, order count type 0 and its lsb, references,
    /// gaps, 80x45 macroblocks, then `frame_mbs_only_flag`.
    fn main_sps(frame_mbs_only: u32) -> Vec<u8> {
        sps(
            77,
            &[
                (0, UE),
                (0, UE),
                (0, UE),
                (2, UE),
                (1, UE),
                (0, BIT),
                (79, UE),
                (44, UE),
                (frame_mbs_only, BIT),
                (1, BIT),
            ],
        )
    }

    #[test]
    fn the_length_size_is_read_from_the_record() {
        for minus_one in 0..4 {
            assert_eq!(
                length_size(&record(&main_sps(1), minus_one)),
                Some(minus_one + 1)
            );
        }
        assert_eq!(length_size(&[1, 77, 0]), None);
    }

    #[test]
    fn frame_mbs_only_flag_is_read_from_the_first_sps() {
        assert_eq!(fields_allowed(&record(&main_sps(1), 3)), Some(false));
        assert_eq!(fields_allowed(&record(&main_sps(0), 3)), Some(true));
    }

    /// High: chroma 1, depths 0, no bypass, a matrix with the first list
    /// sent (16 deltas of 0 after a first of -8, so the rest are 0) and the
    /// others not; then order count type 1 with a cycle of two, and 120x68
    /// macroblocks, with `frame_mbs_only_flag` as given.
    fn high_sps(frame_mbs_only: u32) -> Vec<u8> {
        let mut fields = vec![
            (0, UE),
            (1, UE),
            (0, UE),
            (0, UE),
            (0, BIT),
            (1, BIT),
            (1, BIT),
        ];
        fields.push((16, UE)); // se(-8): code 16
        fields.extend(std::iter::repeat_n((0, BIT), 7));
        fields.extend([
            (0, UE),
            (1, UE),
            (0, BIT),
            (0, UE),
            (0, UE),
            (2, UE),
            (3, UE),
            (4, UE),
            (4, UE),
            (0, BIT),
            (119, UE),
            (67, UE),
            (frame_mbs_only, BIT),
        ]);
        sps(100, &fields)
    }

    #[test]
    fn a_high_sps_with_scaling_lists_and_order_count_type_1_is_read_past() {
        assert_eq!(fields_allowed(&record(&high_sps(0), 3)), Some(true));
        assert_eq!(fields_allowed(&record(&high_sps(1), 3)), Some(false));
    }

    #[test]
    fn a_record_without_an_sps_or_with_a_short_one_says_nothing() {
        assert_eq!(fields_allowed(&[1, 77, 0, 40, 0xff, 0xe0, 0]), None);
        let short = record(&[0x67, 77, 0, 40], 3);
        assert_eq!(fields_allowed(&short), None);
    }

    #[test]
    fn a_scaling_list_delta_past_its_range_says_nothing() {
        // High, chroma 1, a matrix whose first list's first delta_scale is
        // 2^31 - 1, past -128..=127: the sum with 8 overflowed (the host
        // fuzzer's input 173 of seed 0x83ccbf5bec65d684, this Mac,
        // 2026-10-24).
        for delta in [i32::MAX, i32::MIN + 1, 128, -129] {
            let code = if delta > 0 {
                2 * delta.unsigned_abs() - 1
            } else {
                2 * delta.unsigned_abs()
            };
            let mut fields = vec![
                (0, UE),
                (1, UE),
                (0, UE),
                (0, UE),
                (0, BIT),
                (1, BIT),
                (1, BIT),
                (code, UE),
            ];
            fields.extend(std::iter::repeat_n((0, UE), 40));
            let sps = sps(100, &fields);
            assert_eq!(fields_allowed(&record(&sps, 3)), None, "{delta}");
        }
    }

    #[test]
    fn an_sps_that_ends_anywhere_says_nothing_and_does_not_panic() {
        let whole = high_sps(1);
        for end in 0..whole.len() {
            let _ = fields_allowed(&record(&whole[..end.max(4)], 3));
        }
        let mut cut = record(&whole, 3);
        cut.truncate(10);
        assert_eq!(fields_allowed(&cut), None);
    }
}
