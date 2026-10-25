//! The parser against images written here, segment by segment, from
//! T.81's syntax. Their entropy-coded data is arbitrary bytes: the parser
//! does not decode it, only finds where it ends.

use super::*;

/// An image being written, one marker segment at a time.
#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn soi() -> Self {
        Self(vec![0xff, 0xd8])
    }

    fn marker(mut self, marker: u8) -> Self {
        self.0.extend_from_slice(&[0xff, marker]);
        self
    }

    fn segment(mut self, marker: u8, payload: &[u8]) -> Self {
        let length = u16::try_from(payload.len() + 2).unwrap_or(u16::MAX);
        self.0.extend_from_slice(&[0xff, marker]);
        self.0.extend_from_slice(&length.to_be_bytes());
        self.0.extend_from_slice(payload);
        self
    }

    fn bytes(mut self, bytes: &[u8]) -> Self {
        self.0.extend_from_slice(bytes);
        self
    }

    fn jfif(self) -> Self {
        self.segment(0xe0, b"JFIF\0\x01\x02\0\0\x01\0\x01\0\0")
    }

    /// One 8-bit quantization table, `id`, of `value` throughout.
    fn dqt(self, id: u8, value: u8) -> Self {
        let mut payload = vec![id];
        payload.extend_from_slice(&[value; 64]);
        self.segment(0xdb, &payload)
    }

    fn dht(self, class: u8, id: u8, table: &HuffmanTable<'_>) -> Self {
        let mut payload = vec![(class << 4) | id];
        payload.extend_from_slice(&table.counts);
        payload.extend_from_slice(table.values);
        self.segment(0xc4, &payload)
    }

    /// Annex K's four tables, as cjpeg writes them.
    fn standard_tables(self) -> Self {
        self.dht(0, 0, &DEFAULT_DC_LUMINANCE)
            .dht(1, 0, &DEFAULT_AC_LUMINANCE)
            .dht(0, 1, &DEFAULT_DC_CHROMINANCE)
            .dht(1, 1, &DEFAULT_AC_CHROMINANCE)
    }

    /// A frame header: `marker`, `precision`, the size, and each component
    /// as (id, Hi, Vi, Tq).
    fn sof(
        self,
        marker: u8,
        precision: u8,
        width: u16,
        height: u16,
        components: &[(u8, u8, u8, u8)],
    ) -> Self {
        let mut payload = vec![precision];
        payload.extend_from_slice(&height.to_be_bytes());
        payload.extend_from_slice(&width.to_be_bytes());
        payload.push(u8::try_from(components.len()).unwrap_or(0));
        for &(id, h, v, tq) in components {
            payload.extend_from_slice(&[id, (h << 4) | v, tq]);
        }
        self.segment(marker, &payload)
    }

    /// A sequential scan of components given as (id, Td, Ta).
    fn sos(self, components: &[(u8, u8, u8)]) -> Self {
        let mut payload = vec![u8::try_from(components.len()).unwrap_or(0)];
        for &(id, td, ta) in components {
            payload.extend_from_slice(&[id, (td << 4) | ta]);
        }
        payload.extend_from_slice(&[0, 63, 0]);
        self.segment(0xda, &payload)
    }

    fn eoi(self) -> Vec<u8> {
        self.marker(0xd9).0
    }
}

const YUV420: [(u8, u8, u8, u8); 3] = [(1, 2, 2, 0), (2, 1, 1, 1), (3, 1, 1, 1)];
const SCAN3: [(u8, u8, u8); 3] = [(1, 0, 0), (2, 1, 1), (3, 1, 1)];

/// Entropy-coded data with a stuffed 0xFF and two restart markers.
const ENTROPY: &[u8] = &[0x12, 0xff, 0x00, 0x34, 0xff, 0xd0, 0x56, 0xff, 0xd1, 0x78];

/// A 4:2:0 baseline image, as cjpeg writes one.
fn yuv420(width: u16, height: u16) -> Writer {
    Writer::soi()
        .jfif()
        .dqt(0, 2)
        .dqt(1, 3)
        .sof(0xc0, 8, width, height, &YUV420)
        .standard_tables()
}

#[test]
fn a_baseline_image_reads_whole() -> Result<(), Error> {
    let data = yuv420(333, 211)
        .segment(0xdd, &[0, 3])
        .sos(&SCAN3)
        .bytes(ENTROPY)
        .eoi();
    let image = parse(&data)?;
    assert_eq!(image.frame.process, Process::Baseline);
    assert_eq!((image.frame.width, image.frame.height), (333, 211));
    assert_eq!(image.frame.precision, 8);
    assert_eq!(image.frame.subsampling(), Some(Subsampling::Yuv420));
    assert_eq!(image.frame.mcu_size(), (16, 16));
    assert_eq!(image.frame.mcus(), (21, 14));
    assert_eq!(image.frame.mcu_count(), 294);
    assert_eq!(image.frame.component_size(0), Some((333, 211)));
    assert_eq!(image.frame.component_size(1), Some((167, 106)));
    assert_eq!(image.restart_interval, 3);
    assert_eq!(image.entropy_coded_data, ENTROPY);
    assert_eq!(image.quantization_tables[0].map(|t| t.values[63]), Some(2));
    assert_eq!(
        image.quantization_tables[1].map(|t| t.sixteen_bit),
        Some(false)
    );
    assert!(image.quantization_tables[2].is_none());
    assert_eq!(image.dc_tables[1], Some(DEFAULT_DC_CHROMINANCE));
    assert_eq!(image.ac_tables[0], Some(DEFAULT_AC_LUMINANCE));
    assert!(!image.default_huffman_tables);
    assert_eq!(
        image.scan.components,
        [
            ScanComponent {
                selector: 1,
                dc_table: 0,
                ac_table: 0
            },
            ScanComponent {
                selector: 2,
                dc_table: 1,
                ac_table: 1
            },
            ScanComponent {
                selector: 3,
                dc_table: 1,
                ac_table: 1
            },
        ]
    );
    assert_eq!(image.adobe_transform, None);
    Ok(())
}

#[test]
fn subsamplings_are_read_from_the_factors() -> Result<(), Error> {
    for (luma, expected, mcu) in [
        ((2, 2), Some(Subsampling::Yuv420), (16, 16)),
        ((2, 1), Some(Subsampling::Yuv422), (16, 8)),
        ((1, 2), Some(Subsampling::Yuv440), (8, 16)),
        ((1, 1), Some(Subsampling::Yuv444), (8, 8)),
        ((4, 1), Some(Subsampling::Yuv411), (32, 8)),
        ((3, 1), None, (24, 8)),
    ] {
        let data = Writer::soi()
            .dqt(0, 1)
            .sof(
                0xc0,
                8,
                64,
                64,
                &[(1, luma.0, luma.1, 0), (2, 1, 1, 0), (3, 1, 1, 0)],
            )
            .standard_tables()
            .sos(&SCAN3)
            .eoi();
        let frame = parse(&data)?.frame;
        assert_eq!(frame.subsampling(), expected, "{luma:?}");
        assert_eq!(frame.mcu_size(), mcu, "{luma:?}");
    }
    // Chroma sampled above 1x1 is none of them.
    let data = Writer::soi()
        .dqt(0, 1)
        .sof(0xc0, 8, 64, 64, &[(1, 2, 2, 0), (2, 2, 1, 0), (3, 1, 1, 0)])
        .standard_tables()
        .sos(&SCAN3)
        .eoi();
    assert_eq!(parse(&data)?.frame.subsampling(), None);
    Ok(())
}

#[test]
fn one_component_is_grey_with_one_block_an_mcu() -> Result<(), Error> {
    // Factors on a lone component do not change its MCU (A.2.2).
    let data = Writer::soi()
        .dqt(0, 1)
        .sof(0xc1, 8, 17, 9, &[(7, 2, 2, 0)])
        .standard_tables()
        .sos(&[(7, 0, 0)])
        .eoi();
    let image = parse(&data)?;
    assert_eq!(image.frame.process, Process::ExtendedSequential);
    assert_eq!(image.frame.subsampling(), Some(Subsampling::Grey));
    assert_eq!(image.frame.mcu_size(), (8, 8));
    assert_eq!(image.frame.mcus(), (3, 2));
    assert_eq!(image.entropy_coded_data, b"");
    Ok(())
}

#[test]
fn an_image_without_huffman_tables_takes_annex_ks() -> Result<(), Error> {
    let data = Writer::soi()
        .dqt(0, 1)
        .sof(0xc0, 8, 16, 16, &[(1, 2, 2, 0), (2, 1, 1, 0), (3, 1, 1, 0)])
        .sos(&SCAN3)
        .bytes(&[1, 2, 3])
        .eoi();
    let image = parse(&data)?;
    assert!(image.default_huffman_tables);
    assert_eq!(image.dc_tables[0], Some(DEFAULT_DC_LUMINANCE));
    assert_eq!(image.ac_tables[1], Some(DEFAULT_AC_CHROMINANCE));
    assert_eq!(image.dc_tables[2], None);
    Ok(())
}

#[test]
fn annex_ks_tables_are_whole_and_their_codes_fit() {
    for (table, size) in [
        (DEFAULT_DC_LUMINANCE, 12),
        (DEFAULT_DC_CHROMINANCE, 12),
        (DEFAULT_AC_LUMINANCE, 162),
        (DEFAULT_AC_CHROMINANCE, 162),
    ] {
        let total: usize = table.counts.iter().map(|&c| usize::from(c)).sum();
        assert_eq!((total, table.values.len()), (size, size));
        let class = if size == 12 { 0 } else { 1 };
        assert!(syntax_check(class, &table));
    }
}

/// Whether a stream defining `table` in `class` parses: the parser's
/// checks of its codes and values.
fn syntax_check(class: u8, table: &HuffmanTable<'_>) -> bool {
    let data = Writer::soi()
        .dqt(0, 1)
        .sof(0xc0, 8, 8, 8, &[(1, 1, 1, 0)])
        .dht(0, 0, &DEFAULT_DC_LUMINANCE)
        .dht(1, 0, &DEFAULT_AC_LUMINANCE)
        .dht(class, 0, table)
        .sos(&[(1, 0, 0)])
        .eoi();
    parse(&data).is_ok()
}

#[test]
fn a_huffman_table_with_too_many_codes_is_malformed() {
    // Two codes of length 1 would take the all-ones code.
    let full = HuffmanTable {
        counts: [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        values: &[0, 1],
    };
    assert!(!syntax_check(1, &full));
    let fits = HuffmanTable {
        counts: [1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        values: &[0, 1],
    };
    assert!(syntax_check(1, &fits));
}

#[test]
fn what_a_baseline_decoder_does_not_take_is_refused() {
    let with_frame = |marker: u8, precision: u8| {
        Writer::soi()
            .dqt(0, 1)
            .sof(
                marker,
                precision,
                16,
                16,
                &YUV420.map(|(id, h, v, _)| (id, h, v, 0)),
            )
            .standard_tables()
            .sos(&SCAN3)
            .eoi()
    };
    for (marker, precision, what) in [
        (0xc2, 8, "progressive coding"),
        (0xc3, 8, "lossless coding"),
        (0xc5, 8, "hierarchical coding"),
        (0xc9, 8, "arithmetic coding"),
        (0xcd, 8, "arithmetic coding"),
        (0xf7, 8, "JPEG-LS"),
        (0xc1, 12, "12-bit samples"),
    ] {
        assert_eq!(
            parse(&with_frame(marker, precision)),
            Err(Error::Unsupported(what)),
            "{marker:#x}"
        );
    }
    // A DAC segment is arithmetic coding's.
    let dac = Writer::soi().segment(0xcc, &[0x01, 0x10]).eoi();
    assert_eq!(parse(&dac), Err(Error::Unsupported("arithmetic coding")));
    // Baseline is 8-bit only.
    assert_eq!(parse(&with_frame(0xc0, 12)), Err(Error::SofPrecision));
}

#[test]
fn an_image_in_several_scans_is_refused() {
    // One component a scan, as a non-interleaved image has it.
    let first_of_three = yuv420(16, 16).sos(&[(1, 0, 0)]).bytes(&[1]).eoi();
    assert_eq!(
        parse(&first_of_three),
        Err(Error::Unsupported("an image in more than one scan"))
    );
    let two_scans = yuv420(16, 16)
        .sos(&SCAN3)
        .bytes(&[1])
        .sos(&SCAN3)
        .bytes(&[2])
        .eoi();
    assert_eq!(
        parse(&two_scans),
        Err(Error::Unsupported("an image in more than one scan"))
    );
}

#[test]
fn a_height_left_to_a_dnl_is_refused() {
    let data = yuv420(16, 0).sos(&SCAN3).bytes(&[1]).eoi();
    assert_eq!(
        parse(&data),
        Err(Error::Unsupported("a height set after the scan (DNL)"))
    );
    let after = yuv420(16, 16)
        .sos(&SCAN3)
        .bytes(&[1])
        .segment(0xdc, &[0, 16])
        .eoi();
    assert_eq!(
        parse(&after),
        Err(Error::Unsupported("a height set after the scan (DNL)"))
    );
}

#[test]
fn segments_after_the_scan_are_passed_over() -> Result<(), Error> {
    let data = yuv420(16, 16)
        .sos(&SCAN3)
        .bytes(&[9, 9])
        .dqt(0, 7)
        .segment(0xfe, b"a comment")
        .eoi();
    let image = parse(&data)?;
    assert_eq!(image.entropy_coded_data, [9, 9]);
    // The scan took the table in force when it began.
    assert_eq!(image.quantization_tables[0].map(|t| t.values[0]), Some(2));
    Ok(())
}

#[test]
fn fill_bytes_application_segments_and_comments_are_passed_over() -> Result<(), Error> {
    let data = Writer::soi()
        .bytes(&[0xff, 0xff])
        .segment(0xe1, b"Exif\0\0 anything")
        .segment(0xfe, b"a comment")
        .segment(0xee, b"Adobe\0\x64\0\0\0\0\x01")
        .dqt(0, 1)
        .dqt(1, 1)
        .sof(0xc0, 8, 16, 16, &YUV420)
        .standard_tables()
        .marker(0x01)
        .sos(&SCAN3)
        .bytes(&[0x10, 0xff, 0xff, 0xd2, 0x20])
        .bytes(&[0xff, 0xff])
        .eoi();
    let image = parse(&data)?;
    assert_eq!(image.adobe_transform, Some(1));
    // Fill bytes before a restart marker stay in the data.
    assert_eq!(image.entropy_coded_data, [0x10, 0xff, 0xff, 0xd2, 0x20]);
    Ok(())
}

#[test]
fn sixteen_bit_quantization_tables_read_as_such() -> Result<(), Error> {
    let mut payload = vec![0x10];
    for k in 0..64u16 {
        payload.extend_from_slice(&(300 + k).to_be_bytes());
    }
    let data = Writer::soi()
        .segment(0xdb, &payload)
        .sof(0xc0, 8, 8, 8, &[(1, 1, 1, 0)])
        .standard_tables()
        .sos(&[(1, 0, 0)])
        .eoi();
    let table = parse(&data)?.quantization_tables[0].ok_or(Error::Truncated)?;
    assert!(table.sixteen_bit);
    assert_eq!((table.values[0], table.values[63]), (300, 363));
    Ok(())
}

#[test]
fn malformed_images_are_refused() {
    let cases: [(&str, Error, Vec<u8>); 13] = [
        ("no SOI", Error::NoSoi, Writer::default().marker(0xd9).0),
        ("EOI first", Error::EoiBeforeScan, Writer::soi().eoi()),
        (
            "no quantization table",
            Error::UndefinedQuantizationTable,
            Writer::soi()
                .sof(0xc0, 8, 8, 8, &[(1, 1, 1, 0)])
                .standard_tables()
                .sos(&[(1, 0, 0)])
                .eoi(),
        ),
        (
            "an undefined Huffman table",
            Error::UndefinedHuffmanTable,
            Writer::soi()
                .dqt(0, 1)
                .sof(0xc0, 8, 8, 8, &[(1, 1, 1, 0)])
                .dht(0, 0, &DEFAULT_DC_LUMINANCE)
                .sos(&[(1, 0, 0)])
                .eoi(),
        ),
        (
            "a zero quantization value",
            Error::DqtZeroValue,
            Writer::soi().dqt(0, 0).eoi(),
        ),
        (
            "a scan before the frame",
            Error::SosBeforeSof,
            Writer::soi().dqt(0, 1).sos(&[(1, 0, 0)]).eoi(),
        ),
        (
            "scan components out of order",
            Error::SosComponentNotInFrame,
            yuv420(16, 16).sos(&[(2, 1, 1), (1, 0, 0), (3, 1, 1)]).eoi(),
        ),
        (
            "baseline's tables are 0 and 1",
            Error::SosTableSelector,
            yuv420(16, 16).sos(&[(1, 2, 0), (2, 1, 1), (3, 1, 1)]).eoi(),
        ),
        (
            "a second frame header",
            Error::SecondSof,
            yuv420(16, 16).sof(0xc0, 8, 16, 16, &YUV420).eoi(),
        ),
        (
            "a stray byte between segments",
            Error::NotAMarker,
            Writer::soi().bytes(&[0x00]).dqt(0, 1).eoi(),
        ),
        (
            "a segment shorter than its length field",
            Error::SegmentLength,
            Writer::soi().bytes(&[0xff, 0xdb, 0x00, 0x01]).eoi(),
        ),
        (
            "a scan header's length is not its components'",
            Error::SosLength,
            yuv420(16, 16)
                .segment(0xda, &[3, 1, 0x00, 2, 0x11, 0, 63, 0])
                .eoi(),
        ),
        (
            "a scan component twice",
            Error::SosComponentNotInFrame,
            yuv420(16, 16).sos(&[(1, 0, 0), (1, 0, 0), (3, 1, 1)]).eoi(),
        ),
    ];
    for (what, error, data) in cases {
        assert_eq!(parse(&data), Err(error), "{what}");
    }
}

#[test]
fn huffman_tables_past_what_8_bit_coefficients_take_are_refused() {
    // Sixteen five-bit DC codes, categories 0 to 11 and four again.
    let dc_values: Vec<u8> = (0..12).chain(0..4).collect();
    let mut dc_counts = [0u8; 16];
    dc_counts[4] = 16;
    // Every AC value once, then 38 again, in eight-bit codes.
    let mut ac_values: Vec<u8> = (0..16u8)
        .flat_map(|run| (1..=10).map(move |size| (run << 4) | size))
        .chain([0x00, 0xf0])
        .collect();
    ac_values.extend_from_within(..38);
    let mut ac_counts = [0u8; 16];
    ac_counts[7] = 200;
    let size_15 = [0x0f, 0x00];
    let mut two_codes = [0u8; 16];
    two_codes[1] = 2;
    let tables = [
        (
            0,
            HuffmanTable {
                counts: dc_counts,
                values: &dc_values,
            },
            "16 DC codes",
            Error::DcTableOver12Codes,
        ),
        (
            1,
            HuffmanTable {
                counts: ac_counts,
                values: &ac_values,
            },
            "200 AC codes",
            Error::AcTableValues,
        ),
        (
            1,
            HuffmanTable {
                counts: two_codes,
                values: &size_15,
            },
            "an AC size of 15",
            Error::AcTableValues,
        ),
        (
            1,
            HuffmanTable {
                counts: [0; 16],
                values: &[],
            },
            "an empty AC table",
            Error::EmptyHuffmanTable,
        ),
    ];
    for (class, table, what, error) in tables {
        let data = yuv420(16, 16)
            .dht(class, 0, &table)
            .sos(&SCAN3)
            .bytes(ENTROPY)
            .eoi();
        assert_eq!(parse(&data), Err(error), "{what}");
    }
    // Every AC value once is a table, as Annex K's are.
    ac_values.truncate(162);
    ac_counts[7] = 162;
    let whole = HuffmanTable {
        counts: ac_counts,
        values: &ac_values,
    };
    let data = yuv420(16, 16)
        .dht(1, 0, &whole)
        .sos(&SCAN3)
        .bytes(ENTROPY)
        .eoi();
    assert!(parse(&data).is_ok(), "{:?}", parse(&data));
}

#[test]
fn an_image_that_ends_early_is_truncated() {
    let whole = yuv420(16, 16).sos(&SCAN3).bytes(ENTROPY).eoi();
    // Cut in the scan's data, in a segment, and just before the EOI.
    for cut in [whole.len() - 2, whole.len() - 6, 40, 3] {
        assert_eq!(parse(&whole[..cut]), Err(Error::Truncated), "cut at {cut}");
    }
    // Data after the EOI is not the image's.
    let mut padded = whole.clone();
    padded.extend_from_slice(&[0; 100]);
    assert_eq!(parse(&padded).map(|i| i.entropy_coded_data), Ok(ENTROPY));
}

mod fuzz;
