//! The stream nvJPEG is given: the image the codec crate's parser read,
//! written again from its tables, frame and scan, around the scan's
//! entropy-coded data as it was.
//!
//! nvJPEG's hardware decoder is not handed the guest's bytes, for three
//! reasons, each measured on the RTX 5080 Laptop GPU (libnvjpeg 13.2.3,
//! driver 615.71.09, 2026-10-25):
//! - an image that defines no Huffman table, as a Motion JPEG frame, is
//!   refused (`NVJPEG_STATUS_JPEG_NOT_SUPPORTED`): here it gets Annex K's,
//!   which the parser gives it;
//! - a quantization value over 255, which only a 16-bit table holds, is
//!   read as its low 8 bits: the decode succeeds with wrong pixels (up to
//!   111 off on a 640x480 image of 1-pixel stripes from `cjpeg -quality 5`,
//!   and equal, within 1, to a decode of the tables cut to 8 bits). Such an
//!   image is refused here, before nvJPEG sees it, as VA-API refuses it;
//! - what follows the scan (data after the EOI) and the segments a decode
//!   does not read are left out, so that nvJPEG reads what the walk of the
//!   scan checked.

use reims_vgpu_video_codec::jpeg::{
    DEFAULT_AC_LUMINANCE, DEFAULT_DC_LUMINANCE, HuffmanTable, Image,
};
use reims_vgpu_video_core::jpeg::JpegError;

/// The image as nvJPEG is to read it: SOI, the quantization tables the
/// frame's components name (8-bit), the frame header, the Huffman tables
/// the scan names, the restart interval, the scan header, the scan's
/// entropy-coded data and EOI. A quantization value over 255 is
/// [`JpegError::UnsupportedStream`] (module docs).
pub(super) fn rewrite(image: &Image<'_>) -> Result<Vec<u8>, JpegError> {
    let frame = &image.frame;
    let mut out = Vec::with_capacity(image.entropy_coded_data.len().saturating_add(1024));
    out.extend_from_slice(&[0xff, 0xd8]);

    // DQT: each table a component names, once, as 8-bit values in the
    // stream's zig-zag order.
    let mut written = [false; 4];
    for component in &frame.components {
        let index = usize::from(component.quantization_table);
        let Some(seen) = written.get_mut(index) else {
            return Err(JpegError::BadData("a quantization table past 3".into()));
        };
        if *seen {
            continue;
        }
        *seen = true;
        let Some(Some(table)) = image.quantization_tables.get(index) else {
            return Err(JpegError::BadData(format!(
                "quantization table {index} is not defined"
            )));
        };
        let mut payload = Vec::with_capacity(65);
        payload.push(component.quantization_table);
        for &value in &table.values {
            let Ok(value) = u8::try_from(value) else {
                return Err(JpegError::UnsupportedStream(
                    "a quantization value over 255, which nvJPEG's hardware decoder reads as \
                     its low 8 bits"
                        .into(),
                ));
            };
            payload.push(value);
        }
        segment(&mut out, 0xdb, &payload)?;
    }

    // SOF0, or SOF1 where the scan names a Huffman table past 1, which a
    // baseline frame may not (T.81 B.2.4.2). Both are 8-bit here: the
    // parser refuses 12-bit samples.
    let scan = &image.scan.components;
    let baseline = scan.iter().all(|c| c.dc_table < 2 && c.ac_table < 2);
    let mut header = vec![8];
    header.extend_from_slice(&frame.height.to_be_bytes());
    header.extend_from_slice(&frame.width.to_be_bytes());
    header.push(count(frame.components.len())?);
    for component in &frame.components {
        header.extend_from_slice(&[
            component.id,
            (component.horizontal << 4) | (component.vertical & 0x0f),
            component.quantization_table,
        ]);
    }
    segment(&mut out, if baseline { 0xc0 } else { 0xc1 }, &header)?;

    // DHT: each table the scan names, once per class: the stream's own, or
    // Annex K's where it defined none.
    let mut dc_written = [false; 4];
    let mut ac_written = [false; 4];
    for component in scan {
        for (class, id, tables, written) in [
            (0u8, component.dc_table, &image.dc_tables, &mut dc_written),
            (1u8, component.ac_table, &image.ac_tables, &mut ac_written),
        ] {
            let index = usize::from(id);
            let Some(seen) = written.get_mut(index) else {
                return Err(JpegError::BadData("a Huffman table past 3".into()));
            };
            if *seen {
                continue;
            }
            *seen = true;
            let Some(Some(table)) = tables.get(index) else {
                return Err(JpegError::BadData(format!(
                    "Huffman table {class}/{index} is not defined"
                )));
            };
            huffman(&mut out, class, id, table)?;
        }
    }

    if image.restart_interval > 0 {
        segment(&mut out, 0xdd, &image.restart_interval.to_be_bytes())?;
    }

    // SOS: a sequential scan of every component, in the frame's order.
    let mut sos = vec![count(scan.len())?];
    for component in scan {
        sos.extend_from_slice(&[
            component.selector,
            (component.dc_table << 4) | (component.ac_table & 0x0f),
        ]);
    }
    sos.extend_from_slice(&[0, 63, 0]);
    segment(&mut out, 0xda, &sos)?;
    out.extend_from_slice(image.entropy_coded_data);
    out.extend_from_slice(&[0xff, 0xd9]);
    Ok(out)
}

/// An 8x8 grey image, written here from T.81's syntax: quantization values
/// of 1, Annex K's luminance tables, and one block whose DC difference is 0
/// and whose AC is all zero, so that every sample decodes to 128. The
/// decoder's trial decode at start.
pub(super) fn flat_grey() -> Vec<u8> {
    let mut image = vec![0xff, 0xd8];
    let mut dqt = vec![0];
    dqt.extend_from_slice(&[1; 64]);
    // Each segment is far under 65535 bytes.
    let _ = segment(&mut image, 0xdb, &dqt);
    // SOF0: 8 bits, 8x8, one component (id 1, 1x1, table 0).
    let _ = segment(&mut image, 0xc0, &[8, 0, 8, 0, 8, 1, 1, 0x11, 0]);
    let _ = huffman(&mut image, 0, 0, &DEFAULT_DC_LUMINANCE);
    let _ = huffman(&mut image, 1, 0, &DEFAULT_AC_LUMINANCE);
    // SOS: component 1 on tables 0 and 0, spectral selection 0 to 63.
    let _ = segment(&mut image, 0xda, &[1, 1, 0x00, 0, 63, 0]);
    // DC category 0 ("00"), then EOB ("1010"), padded with ones.
    image.push(0x2b);
    image.extend_from_slice(&[0xff, 0xd9]);
    image
}

/// A count of components as its header's byte: at most 4 in a frame or
/// scan the parser read.
fn count(len: usize) -> Result<u8, JpegError> {
    u8::try_from(len).map_err(|_| JpegError::BadData("too many components".into()))
}

/// Appends a marker segment: the marker, its length and `payload`.
fn segment(out: &mut Vec<u8>, marker: u8, payload: &[u8]) -> Result<(), JpegError> {
    let length = payload
        .len()
        .checked_add(2)
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| JpegError::BadData("a segment over 65535 bytes".into()))?;
    out.extend_from_slice(&[0xff, marker]);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

/// Appends a DHT segment of one table.
fn huffman(
    out: &mut Vec<u8>,
    class: u8,
    id: u8,
    table: &HuffmanTable<'_>,
) -> Result<(), JpegError> {
    let mut payload = Vec::with_capacity(17usize.saturating_add(table.values.len()));
    payload.push((class << 4) | (id & 0x0f));
    payload.extend_from_slice(&table.counts);
    payload.extend_from_slice(table.values);
    segment(out, 0xc4, &payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reims_vgpu_video_codec::jpeg::{parse, walk};

    /// A grey 8x8 image, written here from T.81's syntax: one DQT segment
    /// of `precision` (0 for 8-bit values, 1 for 16-bit) holding `values`,
    /// Annex K's luminance tables unless `dht` is false, and one block whose
    /// DC difference is 0 and whose AC is all zero.
    fn grey(precision: u8, values: &[u16; 64], dht: bool) -> Vec<u8> {
        let mut image = vec![0xff, 0xd8];
        let mut dqt = vec![precision << 4];
        for &value in values {
            if precision == 0 {
                dqt.push(u8::try_from(value).unwrap_or(u8::MAX));
            } else {
                dqt.extend_from_slice(&value.to_be_bytes());
            }
        }
        segment(&mut image, 0xdb, &dqt).ok();
        // SOF0: 8 bits, 8x8, one component (id 1, 1x1, table 0).
        segment(&mut image, 0xc0, &[8, 0, 8, 0, 8, 1, 1, 0x11, 0]).ok();
        if dht {
            huffman(&mut image, 0, 0, &DEFAULT_DC_LUMINANCE).ok();
            huffman(&mut image, 1, 0, &DEFAULT_AC_LUMINANCE).ok();
        }
        segment(&mut image, 0xda, &[1, 1, 0x00, 0, 63, 0]).ok();
        // DC category 0 ("00"), then EOB ("1010"), padded with ones.
        image.push(0x2b);
        image.extend_from_slice(&[0xff, 0xd9]);
        image
    }

    fn rewritten(data: &[u8]) -> Result<Vec<u8>, JpegError> {
        let image = parse(data).map_err(|e| JpegError::BadData(e.to_string()))?;
        rewrite(&image)
    }

    /// The marker segments of `data` up to its SOS, as (marker, payload).
    fn segments(data: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut found = Vec::new();
        let mut at = 2;
        while let [0xff, marker, high, low, ..] = data[at..] {
            let length = usize::from(u16::from_be_bytes([high, low]));
            found.push((marker, data[at + 4..at + 2 + length].to_vec()));
            if marker == 0xda {
                break;
            }
            at += 2 + length;
        }
        found
    }

    #[test]
    fn a_quantization_value_over_255_is_refused_before_nvjpeg() {
        let mut values = [1u16; 64];
        values[63] = 256;
        let refused = rewritten(&grey(1, &values, true));
        assert!(
            matches!(refused, Err(JpegError::UnsupportedStream(ref why)) if why.contains("over 255")),
            "{refused:?}"
        );
    }

    #[test]
    fn a_sixteen_bit_table_of_small_values_is_written_in_eight_bits() {
        let mut values = [1u16; 64];
        values[0] = 255;
        let data = rewritten(&grey(1, &values, true)).unwrap_or_default();
        let dqt: Vec<_> = segments(&data)
            .into_iter()
            .filter(|(marker, _)| *marker == 0xdb)
            .collect();
        assert_eq!(dqt.len(), 1);
        let payload = &dqt[0].1;
        assert_eq!(payload.len(), 65, "Pq 0: one byte a value");
        assert_eq!(payload[0], 0x00);
        assert_eq!(payload[1], 255);
        assert!(payload[2..].iter().all(|&v| v == 1));
    }

    #[test]
    fn an_image_without_huffman_tables_gets_annex_k() {
        let data = rewritten(&grey(0, &[1; 64], false)).unwrap_or_default();
        let dht: Vec<_> = segments(&data)
            .into_iter()
            .filter(|(marker, _)| *marker == 0xc4)
            .map(|(_, payload)| payload)
            .collect();
        let mut dc = vec![0x00];
        dc.extend_from_slice(&DEFAULT_DC_LUMINANCE.counts);
        dc.extend_from_slice(DEFAULT_DC_LUMINANCE.values);
        let mut ac = vec![0x10];
        ac.extend_from_slice(&DEFAULT_AC_LUMINANCE.counts);
        ac.extend_from_slice(DEFAULT_AC_LUMINANCE.values);
        assert_eq!(dht, [dc, ac]);
    }

    #[test]
    fn the_trial_image_parses_and_its_scan_walks() {
        let data = flat_grey();
        let image = parse(&data);
        assert!(
            image.as_ref().is_ok_and(|image| walk(image).is_ok()),
            "{image:?}"
        );
        assert_eq!(rewritten(&data).ok(), Some(data));
    }

    #[test]
    fn the_rewritten_image_reads_as_the_original() {
        let original = grey(0, &[3; 64], true);
        let data = rewritten(&original).unwrap_or_default();
        let (Ok(before), Ok(after)) = (parse(&original), parse(&data)) else {
            unreachable!("both parse");
        };
        assert_eq!(after.frame, before.frame);
        assert_eq!(after.quantization_tables, before.quantization_tables);
        assert_eq!(after.dc_tables[0], before.dc_tables[0]);
        assert_eq!(after.ac_tables[0], before.ac_tables[0]);
        assert_eq!(after.scan, before.scan);
        assert_eq!(after.entropy_coded_data, before.entropy_coded_data);
        assert_eq!(&data[data.len() - 2..], [0xff, 0xd9]);
    }
}
