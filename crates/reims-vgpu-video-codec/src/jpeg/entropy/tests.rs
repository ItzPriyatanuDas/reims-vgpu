//! The walk against images written here from T.81's coding procedures
//! (F.1.2) with Annex K's tables, and against damaged copies of them. The
//! same images, written to a directory by the ignored `write_the_cases`,
//! are what Apple's JPEG hardware was asked about.

use super::*;
use crate::jpeg::{
    DEFAULT_AC_CHROMINANCE, DEFAULT_AC_LUMINANCE, DEFAULT_DC_CHROMINANCE, DEFAULT_DC_LUMINANCE,
    parse,
};

/// Which of Annex K's table pairs a block is coded with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tables {
    Luma,
    Chroma,
}

/// What a scan is written from, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Token {
    /// A DC difference.
    Dc(Tables, i32),
    /// An AC coefficient after `run` zeros.
    Ac(Tables, u8, i32),
    /// Sixteen zeros.
    Zrl(Tables),
    /// The rest of the block is zero.
    Eob(Tables),
    /// Bits as they are, most significant first.
    Bits(u32, u32),
    /// The byte's padding, then a marker.
    Marker(u8),
    /// The byte's padding, then bytes as they are, unstuffed.
    Bytes(Vec<u8>),
}

/// A Huffman table's codes, by value: (code, length).
fn codes(table: &HuffmanTable<'_>) -> [(u32, u32); 256] {
    let mut codes = [(0, 0); 256];
    let (mut code, mut index) = (0u32, 0usize);
    for length in 1..=16u32 {
        for _ in 0..table.counts[length as usize - 1] {
            codes[usize::from(table.values[index])] = (code, length);
            code += 1;
            index += 1;
        }
        code <<= 1;
    }
    codes
}

/// The size category of a value, and its bits (F.1.2.1).
fn category(value: i32) -> (u32, u32) {
    let size = 32 - value.unsigned_abs().leading_zeros();
    let bits = if value < 0 { value - 1 } else { value };
    (size, (bits as u32) & ((1 << size) - 1))
}

/// Writes tokens as entropy-coded data, padding each byte with `pad`'s bit.
struct Writer {
    out: Vec<u8>,
    buffer: u32,
    count: u32,
    pad_with_ones: bool,
    tables: [[(u32, u32); 256]; 4],
}

impl Writer {
    fn new(pad_with_ones: bool) -> Writer {
        Writer {
            out: Vec::new(),
            buffer: 0,
            count: 0,
            pad_with_ones,
            tables: [
                codes(&DEFAULT_DC_LUMINANCE),
                codes(&DEFAULT_AC_LUMINANCE),
                codes(&DEFAULT_DC_CHROMINANCE),
                codes(&DEFAULT_AC_CHROMINANCE),
            ],
        }
    }

    fn bits(&mut self, value: u32, count: u32) {
        for i in (0..count).rev() {
            self.buffer = (self.buffer << 1) | ((value >> i) & 1);
            self.count += 1;
            if self.count == 8 {
                let byte = self.buffer as u8;
                self.out.push(byte);
                if byte == 0xff {
                    self.out.push(0x00);
                }
                self.buffer = 0;
                self.count = 0;
            }
        }
    }

    fn pad(&mut self) {
        let bit = u32::from(self.pad_with_ones);
        while self.count != 0 {
            self.bits(bit, 1);
        }
    }

    fn symbol(&mut self, tables: Tables, ac: bool, value: u8) {
        let index = match tables {
            Tables::Luma => 0,
            Tables::Chroma => 2,
        } + usize::from(ac);
        let (code, length) = self.tables[index][usize::from(value)];
        assert_ne!(length, 0, "Annex K has a code for {value:#x}");
        self.bits(code, length);
    }

    fn token(&mut self, token: &Token) {
        match token {
            Token::Dc(tables, difference) => {
                let (size, bits) = category(*difference);
                self.symbol(*tables, false, size as u8);
                self.bits(bits, size);
            }
            Token::Ac(tables, run, value) => {
                let (size, bits) = category(*value);
                self.symbol(*tables, true, (run << 4) | size as u8);
                self.bits(bits, size);
            }
            Token::Zrl(tables) => self.symbol(*tables, true, 0xf0),
            Token::Eob(tables) => self.symbol(*tables, true, 0x00),
            Token::Bits(value, count) => self.bits(*value, *count),
            Token::Marker(marker) => {
                self.pad();
                self.out.extend_from_slice(&[0xff, *marker]);
            }
            Token::Bytes(bytes) => {
                self.pad();
                self.out.extend_from_slice(bytes);
            }
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.pad();
        self.out
    }
}

/// An image's frame: grey, or 4:2:0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Grey,
    Yuv420,
}

/// One image to write: its frame, restart interval and scan.
#[derive(Clone, Debug)]
pub(crate) struct Case {
    kind: Kind,
    width: u16,
    height: u16,
    restart_interval: u16,
    pub(crate) tokens: Vec<Token>,
    pub(crate) pad_with_ones: bool,
    /// Bytes after the EOI.
    pub(crate) trailer: Vec<u8>,
    /// Whether the EOI is written.
    pub(crate) eoi: bool,
}

impl Case {
    /// A valid image of `kind`, `width` by `height`, with a restart
    /// marker every `restart_interval` MCUs (none for 0). Its blocks have
    /// DC differences and AC coefficients of every size, so that codes of
    /// every length are read.
    pub(crate) fn valid(kind: Kind, width: u16, height: u16, restart_interval: u16) -> Case {
        let (mcu_width, mcu_height, layout): (u16, u16, &[(Tables, usize)]) = match kind {
            Kind::Grey => (8, 8, &[(Tables::Luma, 1)]),
            Kind::Yuv420 => (
                16,
                16,
                &[(Tables::Luma, 4), (Tables::Chroma, 1), (Tables::Chroma, 1)],
            ),
        };
        let mcus =
            usize::from(width.div_ceil(mcu_width)) * usize::from(height.div_ceil(mcu_height));
        let mut tokens = Vec::new();
        let mut serial = 0i32;
        let mut restarts = 0u8;
        let mut predictions = vec![0i32; layout.len()];
        for mcu in 0..mcus {
            if restart_interval != 0 && mcu != 0 && mcu % usize::from(restart_interval) == 0 {
                tokens.push(Token::Marker(0xd0 + restarts % 8));
                restarts += 1;
                predictions.fill(0);
            }
            for (&(tables, blocks), prediction) in layout.iter().zip(&mut predictions) {
                for _ in 0..blocks {
                    serial += 1;
                    // DC coefficients over -1000 to 1000, so that their
                    // differences are of every size up to 11.
                    let dc = (serial * 389) % 2001 - 1000;
                    tokens.push(Token::Dc(tables, dc - *prediction));
                    *prediction = dc;
                    // AC: sizes 1 to 10, runs 0 to 15, then an EOB or a
                    // last coefficient at the 63rd.
                    let ac_size = 1 + serial % 10;
                    tokens.push(Token::Ac(tables, 0, 1 << (ac_size - 1)));
                    tokens.push(Token::Ac(tables, (serial % 16) as u8, -3));
                    if serial % 5 == 0 {
                        tokens.push(Token::Zrl(tables));
                        tokens.push(Token::Ac(tables, 2, 600));
                    }
                    if serial % 7 == 0 {
                        // Fill to the 63rd: at k = 2 + run + (ZRL) + ...
                        let used = 2 + (serial % 16) + if serial % 5 == 0 { 19 } else { 0 };
                        let left = 63 - used;
                        let mut run = left - 1;
                        while run >= 16 {
                            tokens.push(Token::Zrl(tables));
                            run -= 16;
                        }
                        tokens.push(Token::Ac(tables, run as u8, 1));
                    } else {
                        tokens.push(Token::Eob(tables));
                    }
                }
            }
        }
        Case {
            kind,
            width,
            height,
            restart_interval,
            tokens,
            pad_with_ones: true,
            trailer: Vec::new(),
            eoi: true,
        }
    }

    pub(crate) fn bytes(&self) -> Vec<u8> {
        let mut out = vec![0xff, 0xd8];
        let segment = |out: &mut Vec<u8>, marker: u8, payload: &[u8]| {
            out.extend_from_slice(&[0xff, marker]);
            out.extend_from_slice(&u16::try_from(payload.len() + 2).unwrap_or(0).to_be_bytes());
            out.extend_from_slice(payload);
        };
        for id in 0..2u8 {
            let mut dqt = vec![id];
            dqt.extend_from_slice(&[4; 64]);
            segment(&mut out, 0xdb, &dqt);
        }
        let components: &[(u8, u8, u8)] = match self.kind {
            Kind::Grey => &[(1, 0x11, 0)],
            Kind::Yuv420 => &[(1, 0x22, 0), (2, 0x11, 1), (3, 0x11, 1)],
        };
        let mut sof = vec![8];
        sof.extend_from_slice(&self.height.to_be_bytes());
        sof.extend_from_slice(&self.width.to_be_bytes());
        sof.push(components.len() as u8);
        for &(id, factors, table) in components {
            sof.extend_from_slice(&[id, factors, table]);
        }
        segment(&mut out, 0xc0, &sof);
        for (spec, table) in [
            (0x00, DEFAULT_DC_LUMINANCE),
            (0x10, DEFAULT_AC_LUMINANCE),
            (0x01, DEFAULT_DC_CHROMINANCE),
            (0x11, DEFAULT_AC_CHROMINANCE),
        ] {
            let mut dht = vec![spec];
            dht.extend_from_slice(&table.counts);
            dht.extend_from_slice(table.values);
            segment(&mut out, 0xc4, &dht);
        }
        if self.restart_interval != 0 {
            segment(&mut out, 0xdd, &self.restart_interval.to_be_bytes());
        }
        let mut sos = vec![components.len() as u8];
        for (i, &(id, _, _)) in components.iter().enumerate() {
            sos.extend_from_slice(&[id, if i == 0 { 0x00 } else { 0x11 }]);
        }
        sos.extend_from_slice(&[0, 63, 0]);
        segment(&mut out, 0xda, &sos);
        let mut writer = Writer::new(self.pad_with_ones);
        for token in &self.tokens {
            writer.token(token);
        }
        out.extend_from_slice(&writer.finish());
        if self.eoi {
            out.extend_from_slice(&[0xff, 0xd9]);
        }
        out.extend_from_slice(&self.trailer);
        out
    }

    /// The index of the `n`th token that matches.
    fn nth(&self, n: usize, matches: impl Fn(&Token) -> bool) -> usize {
        let found = self
            .tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| matches(t))
            .nth(n)
            .map(|(i, _)| i);
        assert!(found.is_some(), "the case has no token {n} of that kind");
        found.unwrap_or_default()
    }

    fn with(mut self, edit: impl FnOnce(&mut Case)) -> Case {
        edit(&mut self);
        self
    }
}

fn is_marker(token: &Token) -> bool {
    matches!(token, Token::Marker(_))
}

fn is_dc(token: &Token) -> bool {
    matches!(token, Token::Dc(..))
}

/// The tokens of the last MCU of a 4:2:0 image: six blocks, from its
/// last luma block's DC back.
fn last_mcu_start(case: &Case) -> usize {
    let blocks = case.tokens.iter().filter(|t| is_dc(t)).count();
    case.nth(blocks - 6, is_dc)
}

/// What the walk says of an image written from `case`.
fn walked(case: &Case) -> Result<ScanWalk, Error> {
    let bytes = case.bytes();
    let image = parse(&bytes)?;
    walk(&image)
}

/// Each case, by the name its file takes.
fn cases() -> Vec<(&'static str, Case)> {
    let grey = Case::valid(Kind::Grey, 64, 64, 0);
    let yuv = Case::valid(Kind::Yuv420, 64, 64, 0);
    let restarts = Case::valid(Kind::Yuv420, 64, 64, 2);
    let half = yuv.tokens.len() / 2;
    vec![
        ("valid-grey", grey.clone()),
        ("valid-420", yuv.clone()),
        ("valid-420-restarts", restarts.clone()),
        ("valid-grey-odd", Case::valid(Kind::Grey, 61, 35, 3)),
        ("valid-420-odd", Case::valid(Kind::Yuv420, 61, 35, 1)),
        // Fill bytes before the EOI are part of the marker (B.1.1.2).
        (
            "valid-fill-before-eoi",
            yuv.clone()
                .with(|c| c.tokens.push(Token::Bytes(vec![0xff, 0xff]))),
        ),
        // Truncation.
        (
            "cut-no-eoi",
            yuv.clone().with(|c| {
                c.tokens.truncate(half);
                c.eoi = false;
            }),
        ),
        (
            "cut-with-eoi",
            yuv.clone().with(|c| c.tokens.truncate(half)),
        ),
        ("no-eoi", yuv.clone().with(|c| c.eoi = false)),
        (
            "last-mcu-missing",
            yuv.clone().with(|c| {
                let at = last_mcu_start(c);
                c.tokens.truncate(at);
            }),
        ),
        // A code no table has: sixteen ones where a DC code goes.
        (
            "bad-dc-code",
            yuv.clone().with(|c| {
                let at = c.nth(9, is_dc);
                c.tokens[at] = Token::Bits(0xffff, 16);
            }),
        ),
        // A run past the 63rd coefficient: four ZRLs from the first.
        (
            "ac-past-63",
            yuv.clone().with(|c| {
                let at = c.nth(9, is_dc) + 1;
                c.tokens
                    .splice(at..at, std::iter::repeat_n(Token::Zrl(Tables::Luma), 4));
            }),
        ),
        // DC differences that take the coefficient past 2047.
        (
            "dc-past-2047",
            yuv.clone().with(|c| {
                for n in [12, 13, 14] {
                    let at = c.nth(n, is_dc);
                    c.tokens[at] = Token::Dc(Tables::Luma, 2047);
                }
            }),
        ),
        // Restart markers.
        (
            "restart-missing",
            restarts.clone().with(|c| {
                let at = c.nth(3, is_marker);
                c.tokens.remove(at);
            }),
        ),
        (
            "restart-out-of-turn",
            restarts.clone().with(|c| {
                let at = c.nth(3, is_marker);
                c.tokens[at] = Token::Marker(0xd5);
            }),
        ),
        (
            "restart-extra",
            restarts.clone().with(|c| {
                let at = c.nth(3, is_marker);
                c.tokens.insert(at, Token::Marker(0xd3));
            }),
        ),
        (
            "restart-without-dri",
            yuv.clone().with(|c| {
                let at = c.nth(12, is_dc);
                c.tokens.insert(at, Token::Marker(0xd0));
            }),
        ),
        (
            "restart-after-last-mcu",
            restarts
                .clone()
                .with(|c| c.tokens.push(Token::Marker(0xd7))),
        ),
        // What follows the last MCU.
        (
            "data-after-last-mcu",
            yuv.clone()
                .with(|c| c.tokens.push(Token::Bytes(vec![0x5a; 16]))),
        ),
        (
            "zeros-after-last-mcu",
            yuv.clone()
                .with(|c| c.tokens.push(Token::Bytes(vec![0x00; 16]))),
        ),
        (
            "zero-padding",
            yuv.clone().with(|c| c.pad_with_ones = false),
        ),
        // More of what may follow the last MCU, and an interval's end.
        (
            "extra-block-after-last-mcu",
            yuv.clone().with(|c| {
                c.tokens
                    .extend([Token::Dc(Tables::Luma, 5), Token::Eob(Tables::Luma)]);
            }),
        ),
        (
            "extra-mcu-after-last-mcu",
            yuv.clone().with(|c| {
                let at = last_mcu_start(c);
                let mcu: Vec<Token> = c.tokens[at..].to_vec();
                c.tokens.extend(mcu);
            }),
        ),
        (
            "ones-after-last-mcu",
            yuv.clone().with(|c| c.tokens.push(Token::Bits(0xffff, 16))),
        ),
        (
            "ac-past-63-after-last-mcu",
            yuv.clone().with(|c| {
                c.tokens.push(Token::Dc(Tables::Luma, 0));
                c.tokens
                    .extend(std::iter::repeat_n(Token::Zrl(Tables::Luma), 4));
            }),
        ),
        (
            "zeros-then-garbage-after-last-mcu",
            yuv.clone().with(|c| {
                let mut bytes = vec![0x00; 16];
                bytes.extend_from_slice(&[0x5a; 4]);
                c.tokens.push(Token::Bytes(bytes));
            }),
        ),
        (
            "one-garbage-byte-after-last-mcu",
            yuv.clone()
                .with(|c| c.tokens.push(Token::Bytes(vec![0x5a]))),
        ),
        (
            "extra-mcu-before-restart",
            restarts.clone().with(|c| {
                let at = c.nth(3, is_marker);
                let start = (0..at)
                    .rev()
                    .filter(|&i| is_dc(&c.tokens[i]))
                    .nth(5)
                    .unwrap_or(0);
                let mcu: Vec<Token> = c.tokens[start..at].to_vec();
                c.tokens.splice(at..at, mcu);
            }),
        ),
        (
            "zeros-before-restart",
            restarts.clone().with(|c| {
                let at = c.nth(3, is_marker);
                c.tokens.insert(at, Token::Bytes(vec![0x00; 8]));
            }),
        ),
        (
            "garbage-before-restart",
            restarts.clone().with(|c| {
                let at = c.nth(3, is_marker);
                c.tokens.insert(at, Token::Bytes(vec![0x5a; 8]));
            }),
        ),
        (
            "garbage-2-after-last-mcu",
            yuv.clone()
                .with(|c| c.tokens.push(Token::Bytes(vec![0x5a; 2]))),
        ),
        (
            "garbage-3-after-last-mcu",
            yuv.clone()
                .with(|c| c.tokens.push(Token::Bytes(vec![0x5a; 3]))),
        ),
        (
            "ones-1-after-last-mcu",
            yuv.clone()
                .with(|c| c.tokens.push(Token::Bytes(vec![0xff, 0x00]))),
        ),
        (
            "ones-2-after-last-mcu",
            yuv.clone()
                .with(|c| c.tokens.push(Token::Bytes(vec![0xff, 0x00, 0xff, 0x00]))),
        ),
        (
            "garbage-1-then-zeros-after-last-mcu",
            yuv.clone().with(|c| {
                let mut bytes = vec![0x5a];
                bytes.extend_from_slice(&[0x00; 16]);
                c.tokens.push(Token::Bytes(bytes));
            }),
        ),
        (
            "zeros-then-garbage-1-after-last-mcu",
            yuv.clone().with(|c| {
                let mut bytes = vec![0x00; 16];
                bytes.push(0x5a);
                c.tokens.push(Token::Bytes(bytes));
            }),
        ),
        (
            "garbage-1-then-zeros-1-after-last-mcu",
            yuv.clone()
                .with(|c| c.tokens.push(Token::Bytes(vec![0x5a, 0x00]))),
        ),
        (
            "restart-then-garbage-after-last-mcu",
            restarts.clone().with(|c| {
                c.tokens.push(Token::Marker(0xd7));
                c.tokens.push(Token::Bytes(vec![0x5a; 4]));
            }),
        ),
        (
            "block-then-zero-bits-after-last-mcu",
            yuv.clone().with(|c| {
                c.tokens.extend([
                    Token::Dc(Tables::Luma, 5),
                    Token::Eob(Tables::Luma),
                    Token::Bits(0, 3),
                ]);
            }),
        ),
        (
            "block-then-ones-byte-after-last-mcu",
            yuv.clone().with(|c| {
                c.tokens.extend([
                    Token::Dc(Tables::Luma, 5),
                    Token::Eob(Tables::Luma),
                    Token::Bits(0xff, 8),
                ]);
            }),
        ),
        (
            "grey-garbage-1-after-last-mcu",
            grey.clone()
                .with(|c| c.tokens.push(Token::Bytes(vec![0x5a]))),
        ),
        (
            "grey-zero-1-after-last-mcu",
            grey.clone()
                .with(|c| c.tokens.push(Token::Bytes(vec![0x00]))),
        ),
        (
            "zero-1-before-restart",
            restarts.clone().with(|c| {
                let at = c.nth(3, is_marker);
                c.tokens.insert(at, Token::Bytes(vec![0x00]));
            }),
        ),
        (
            "garbage-1-before-restart",
            restarts.clone().with(|c| {
                let at = c.nth(3, is_marker);
                c.tokens.insert(at, Token::Bytes(vec![0x5a]));
            }),
        ),
        (
            "cut-at-restart-with-eoi",
            restarts.clone().with(|c| {
                let at = c.nth(3, is_marker);
                c.tokens.truncate(at);
            }),
        ),
        (
            "garbage-after-eoi",
            yuv.clone().with(|c| c.trailer = vec![0x5a; 64]),
        ),
    ]
}

#[test]
fn valid_images_walk_to_their_last_mcu() -> Result<(), Error> {
    assert_eq!(
        walked(&Case::valid(Kind::Grey, 64, 64, 0))?,
        ScanWalk {
            mcus: 64,
            restarts: 0
        }
    );
    assert_eq!(
        walked(&Case::valid(Kind::Yuv420, 64, 64, 2))?,
        ScanWalk {
            mcus: 16,
            restarts: 7
        }
    );
    assert_eq!(
        walked(&Case::valid(Kind::Yuv420, 61, 35, 1))?,
        ScanWalk {
            mcus: 12,
            restarts: 11
        }
    );
    // Sixteen restarts wrap from RST7 back to RST0.
    assert_eq!(
        walked(&Case::valid(Kind::Grey, 64, 64, 4))?,
        ScanWalk {
            mcus: 64,
            restarts: 15
        }
    );
    Ok(())
}

/// What Apple's JPEG hardware answered for each case, by name, for the
/// files `write_the_cases` wrote (this Mac, an M3 Max on macOS 27.0.1,
/// 2026-10-24).
const APPLE: &[(&str, Status)] = &[
    ("valid-grey", Status::Decodes),
    ("valid-420", Status::Decodes),
    ("valid-420-restarts", Status::Decodes),
    ("valid-grey-odd", Status::Decodes),
    ("valid-420-odd", Status::Decodes),
    ("valid-fill-before-eoi", Status::Decodes),
    ("cut-no-eoi", Status::BadMedia),
    ("cut-with-eoi", Status::DeviceError),
    ("no-eoi", Status::BadMedia),
    ("last-mcu-missing", Status::DeviceError),
    ("bad-dc-code", Status::BadMedia),
    ("ac-past-63", Status::BadMedia),
    ("dc-past-2047", Status::Decodes),
    ("restart-missing", Status::BadMedia),
    ("restart-out-of-turn", Status::BadMedia),
    ("restart-extra", Status::BadMedia),
    ("restart-without-dri", Status::BadMedia),
    ("restart-after-last-mcu", Status::Decodes),
    ("data-after-last-mcu", Status::BadMedia),
    ("zeros-after-last-mcu", Status::Decodes),
    ("zero-padding", Status::Decodes),
    ("extra-block-after-last-mcu", Status::Decodes),
    ("extra-mcu-after-last-mcu", Status::BadMedia),
    ("ones-after-last-mcu", Status::BadMedia),
    ("ac-past-63-after-last-mcu", Status::BadMedia),
    ("zeros-then-garbage-after-last-mcu", Status::BadMedia),
    ("one-garbage-byte-after-last-mcu", Status::Decodes),
    ("extra-mcu-before-restart", Status::BadMedia),
    ("zeros-before-restart", Status::BadMedia),
    ("garbage-before-restart", Status::BadMedia),
    ("garbage-2-after-last-mcu", Status::Decodes),
    ("garbage-3-after-last-mcu", Status::BadMedia),
    ("ones-1-after-last-mcu", Status::BadMedia),
    ("ones-2-after-last-mcu", Status::BadMedia),
    ("garbage-1-then-zeros-after-last-mcu", Status::BadMedia),
    ("zeros-then-garbage-1-after-last-mcu", Status::Decodes),
    ("garbage-1-then-zeros-1-after-last-mcu", Status::BadMedia),
    ("restart-then-garbage-after-last-mcu", Status::BadMedia),
    ("block-then-zero-bits-after-last-mcu", Status::Decodes),
    ("block-then-ones-byte-after-last-mcu", Status::BadMedia),
    ("grey-garbage-1-after-last-mcu", Status::BadMedia),
    ("grey-zero-1-after-last-mcu", Status::Decodes),
    ("zero-1-before-restart", Status::BadMedia),
    ("garbage-1-before-restart", Status::BadMedia),
    ("cut-at-restart-with-eoi", Status::DeviceError),
    ("garbage-after-eoi", Status::Decodes),
];

/// Apple's answers, as far as the walk decides them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    /// 0.
    Decodes,
    /// kIOReturnBadMedia, which the engine gives for every error but
    /// [`Error::ScanEndsEarly`].
    BadMedia,
    /// kIOReturnDeviceError, for [`Error::ScanEndsEarly`].
    DeviceError,
}

fn status(result: &Result<ScanWalk, Error>) -> Status {
    match result {
        Ok(_) => Status::Decodes,
        Err(Error::ScanEndsEarly) => Status::DeviceError,
        Err(_) => Status::BadMedia,
    }
}

#[test]
fn each_case_is_answered_as_apples_hardware_answered_it() {
    let cases = cases();
    assert_eq!(cases.len(), APPLE.len());
    for (name, case) in cases {
        let apple = APPLE
            .iter()
            .find(|(n, _)| *n == name)
            .map(|&(_, status)| status);
        let result = walked(&case);
        assert_eq!(Some(status(&result)), apple, "{name}: {result:?}");
    }
}

/// Measured: one byte after a 4:2:0 image's last MCU, whose byte has 4
/// bits of padding, decoded on Apple's hardware for each value below 0x80
/// and was kIOReturnBadMedia for each from 0x80. The bit after the padding
/// is what tells: a 0 makes the one block more's DC code end with the
/// data, a 1 a longer code the data cuts.
#[test]
fn a_byte_after_the_last_mcu_is_taken_as_apples_hardware_takes_it() {
    let yuv = Case::valid(Kind::Yuv420, 64, 64, 0);
    for value in 0..=255u8 {
        let bytes = if value == 0xff {
            vec![0xff, 0x00]
        } else {
            vec![value]
        };
        let case = yuv.clone().with(|c| c.tokens.push(Token::Bytes(bytes)));
        let expected = if value < 0x80 {
            Status::Decodes
        } else {
            Status::BadMedia
        };
        let result = walked(&case);
        assert_eq!(status(&result), expected, "{value:#04x}: {result:?}");
    }
}

/// Measured: runs of 0 to 64 zero bytes after the last MCU of a 64x64
/// image, 4:2:0 and grey, on Apple's hardware, `.` where it decoded and
/// `R` where it answered kIOReturnBadMedia (this Mac, as [`APPLE`]). Below
/// 33 bytes a run is taken by where the two blocks decoded from it stop,
/// which repeats every 3 bytes but once; from 33 it is past the window
/// ([`super::TRAILING_BYTES`]).
#[test]
fn runs_of_zero_bytes_after_the_last_mcu_are_taken_as_apples_hardware_takes_them() {
    let measured = [
        (
            Kind::Yuv420,
            "..R..R..R..R..R..R..R..R...R..R..RRRRRRRRRRRRRRRRRRRRRRRRRRRRRRRR",
        ),
        (
            Kind::Grey,
            "..R..R..R..R..R..R..R..R...R..R..RR.RRRRRRRRRRRRRRRRRRRRRRRRRRRRR",
        ),
    ];
    for (kind, apple) in measured {
        let valid = Case::valid(kind, 64, 64, 0);
        for (n, answer) in apple.bytes().enumerate() {
            let case = valid
                .clone()
                .with(|c| c.tokens.push(Token::Bytes(vec![0; n])));
            let result = walked(&case);
            // The one run Apple took that the window refuses.
            let expected = if answer == b'.' && !(kind == Kind::Grey && n == 35) {
                Status::Decodes
            } else {
                Status::BadMedia
            };
            assert_eq!(status(&result), expected, "{kind:?}, {n} zeros: {result:?}");
        }
    }
}

/// Data after the last MCU of a 64x64 image, drawn from a fixed seed
/// after the rule was written: `count` runs for each kind, of 1 to 40
/// bytes, each byte drawn from all of them, from those under 0x80 or from
/// 0x80 up, or zero four times in five. Each 0xFF is stuffed.
fn random_trailers(kind: Kind, count: usize) -> Vec<Case> {
    // xorshift64, ours: any fixed sequence does.
    let mut state = 0x2026_1024_u64 ^ if kind == Kind::Grey { 0x9e37_79b9 } else { 0 };
    let mut next = move |below: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % below
    };
    let lengths = [
        1, 2, 3, 4, 5, 6, 8, 10, 12, 16, 20, 24, 28, 30, 31, 32, 33, 34, 36, 40,
    ];
    let valid = Case::valid(kind, 64, 64, 0);
    (0..count)
        .map(|_| {
            let length = lengths[next(lengths.len() as u64) as usize];
            let style = next(4);
            let mut bytes = Vec::new();
            for _ in 0..length {
                let byte = match style {
                    0 => next(256),
                    1 => next(128),
                    2 => 128 + next(128),
                    _ if next(5) < 4 => 0,
                    _ => next(256),
                } as u8;
                bytes.push(byte);
                if byte == 0xff {
                    bytes.push(0);
                }
            }
            valid.clone().with(|c| c.tokens.push(Token::Bytes(bytes)))
        })
        .collect()
}

/// Measured: [`random_trailers`]' runs, 4:2:0 then grey, on Apple's
/// hardware, `.` where it decoded and `R` where it answered
/// kIOReturnBadMedia (this Mac, as [`APPLE`]).
const APPLE_RANDOM: [(Kind, &str); 2] = [
    (
        Kind::Yuv420,
        "RR.RRRRR.R.RRRRRR.RR.RR......RRR.RRRRR..RRR.RRRR..RRRRRRRR..",
    ),
    (
        Kind::Grey,
        "RRRRRRR..RRR.RRRRR.RR.R.R.RRRRR.RR.RRRRRRRRR.RRRR.RRRRRRR..R",
    ),
];

#[test]
fn random_data_after_the_last_mcu_is_taken_as_apples_hardware_takes_it() {
    for (kind, apple) in APPLE_RANDOM {
        let cases = random_trailers(kind, apple.len());
        for (n, (case, answer)) in cases.iter().zip(apple.bytes()).enumerate() {
            let expected = if answer == b'.' {
                Status::Decodes
            } else {
                Status::BadMedia
            };
            let result = walked(case);
            assert_eq!(status(&result), expected, "{kind:?} {n}: {result:?}");
        }
    }
}

/// Writes [`random_trailers`]' runs to `RVGV_JPEG_CASES`, as
/// `random-420-NN.jpg` and `random-grey-NN.jpg`, and prints what the walk
/// answers for each, in [`APPLE_RANDOM`]'s form.
#[test]
#[ignore = "writes files: RVGV_JPEG_CASES=DIR"]
fn write_the_random_trailers() -> std::io::Result<()> {
    let Some(dir) = std::env::var_os("RVGV_JPEG_CASES") else {
        return Ok(());
    };
    let dir = std::path::PathBuf::from(dir);
    for (kind, name) in [(Kind::Yuv420, "420"), (Kind::Grey, "grey")] {
        let mut ours = String::new();
        for (n, case) in random_trailers(kind, 60).iter().enumerate() {
            std::fs::write(dir.join(format!("random-{name}-{n:02}.jpg")), case.bytes())?;
            ours.push(if status(&walked(case)) == Status::Decodes {
                '.'
            } else {
                'R'
            });
        }
        println!("ours {name} {ours}");
    }
    Ok(())
}

#[test]
fn stuffed_bytes_and_long_codes_are_read() -> Result<(), Error> {
    // Every 0xFF of the data is stuffed, and some AC codes are 16 bits.
    let case = Case::valid(Kind::Yuv420, 128, 128, 0);
    let bytes = case.bytes();
    let image = parse(&bytes)?;
    assert!(
        image
            .entropy_coded_data
            .windows(2)
            .any(|w| w == [0xff, 0x00]),
        "the data has stuffed bytes"
    );
    walk(&image)?;
    Ok(())
}

/// Writes each case to `RVGV_JPEG_CASES`, as `NAME.jpg`, for a decoder
/// outside the tests to be asked about.
#[test]
#[ignore = "writes files: RVGV_JPEG_CASES=DIR"]
fn write_the_cases() -> std::io::Result<()> {
    let Some(dir) = std::env::var_os("RVGV_JPEG_CASES") else {
        return Ok(());
    };
    let dir = std::path::PathBuf::from(dir);
    for (name, case) in cases() {
        std::fs::write(dir.join(format!("{name}.jpg")), case.bytes())?;
    }
    Ok(())
}
