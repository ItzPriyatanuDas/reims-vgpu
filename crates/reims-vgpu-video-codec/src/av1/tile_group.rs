//! The tile group OBU (5.11.1): which tiles it holds, and where each
//! tile's data is, as a stateless accelerator takes them.

use super::Error;
use super::bits::{BitReader, le};
use super::frame::TileInfo;
use crate::num::ToUsize;

/// One tile of a tile group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tile {
    /// Its tile row.
    pub row: u32,
    /// Its tile column.
    pub column: u32,
    /// Its data's offset in [`TileGroup::data`], past its size field.
    pub offset: usize,
    /// Its data's length.
    pub size: usize,
}

/// A tile group: its tiles, and the data they are in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileGroup<'a> {
    /// `tg_start`.
    pub tg_start: u32,
    /// `tg_end`.
    pub tg_end: u32,
    /// The tile data: the OBU's payload after the tile group's own
    /// header and its byte alignment.
    pub data: &'a [u8],
    /// Each tile, in order.
    pub tiles: Vec<Tile>,
}

impl<'a> TileGroup<'a> {
    /// Parses a tile group from `payload`: a tile group OBU's payload, or
    /// what follows a frame OBU's header.
    pub(crate) fn parse(payload: &'a [u8], tiles: &TileInfo) -> Result<Self, Error> {
        let num_tiles = tiles.num_tiles();
        let mut r = BitReader::new(payload);
        let start_and_end_present = num_tiles > 1 && r.bit()?;
        let (tg_start, tg_end) = if start_and_end_present {
            let bits = tiles
                .cols_log2
                .checked_add(tiles.rows_log2)
                .ok_or(Error::Overflow("tileBits"))?;
            (r.bits(bits)?, r.bits(bits)?)
        } else {
            // A frame of no tiles has no last one to end the group at.
            let last = num_tiles.checked_sub(1);
            (0, last.ok_or(Error::OutOfRange("tg_start or tg_end"))?)
        };
        if tg_start > tg_end || tg_end >= num_tiles {
            return Err(Error::OutOfRange("tg_start or tg_end"));
        }
        r.byte_alignment()?;
        let data = payload.get(r.bytes_used()..).ok_or(Error::Truncated)?;
        let size_bytes = usize::from(tiles.tile_size_bytes);
        let mut group = Self {
            tg_start,
            tg_end,
            data,
            tiles: Vec::new(),
        };
        let mut at = 0usize;
        for tile_num in tg_start..=tg_end {
            // TileCols is at least 1 where NumTiles passes tg_end.
            let (row, column) = tile_num
                .checked_div(tiles.cols)
                .zip(tile_num.checked_rem(tiles.cols))
                .ok_or(Error::OutOfRange("TileCols"))?;
            let rest = data.get(at..).ok_or(Error::Truncated)?;
            let size = if tile_num == tg_end {
                rest.len()
            } else {
                // tile_size_minus_1, then the tile, after the size's
                // bytes, which `le` read from the data.
                let size = (u64::from(le(rest, size_bytes)?) + 1).to_usize();
                at = at.checked_add(size_bytes).ok_or(Error::Truncated)?;
                size
            };
            let end = at
                .checked_add(size)
                .filter(|&end| size != 0 && end <= data.len())
                .ok_or(Error::Truncated)?;
            group.tiles.push(Tile {
                row,
                column,
                offset: at,
                size,
            });
            at = end;
        }
        Ok(group)
    }

    /// Whether the frame's last tile is in this group.
    pub(crate) fn is_last(&self, tiles: &TileInfo) -> bool {
        tiles.num_tiles().checked_sub(1) == Some(self.tg_end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_by_two() -> TileInfo {
        TileInfo {
            cols: 2,
            rows: 2,
            cols_log2: 1,
            rows_log2: 1,
            uniform: true,
            width_in_sbs: vec![1, 1],
            height_in_sbs: vec![1, 1],
            context_update_tile_id: 0,
            tile_size_bytes: 2,
            num_tiles: 4,
        }
    }

    #[test]
    fn each_tile_but_the_last_has_its_size_first() -> Result<(), Error> {
        // tile_start_and_end_present_flag 1, tg_start 01, tg_end 11, and
        // three zero bits of alignment; then tile 1 (2 bytes, size field
        // 0x0001), tile 2 (1 byte), tile 3 the rest.
        let payload = [
            0b1011_1000,
            0x01,
            0x00,
            0xa1,
            0xa2,
            0x00,
            0x00,
            0xb1,
            0xc1,
            0xc2,
        ];
        let group = TileGroup::parse(&payload, &two_by_two())?;
        assert_eq!((group.tg_start, group.tg_end), (1, 3));
        assert_eq!(group.data, &payload[1..]);
        let tiles: Vec<(u32, u32, usize, usize)> = group
            .tiles
            .iter()
            .map(|t| (t.row, t.column, t.offset, t.size))
            .collect();
        assert_eq!(tiles, [(0, 1, 2, 2), (1, 0, 6, 1), (1, 1, 7, 2)]);
        assert!(group.is_last(&two_by_two()));
        Ok(())
    }

    #[test]
    fn one_tile_takes_the_whole_payload() -> Result<(), Error> {
        let one = TileInfo {
            cols: 1,
            rows: 1,
            tile_size_bytes: 4,
            num_tiles: 1,
            ..TileInfo::default()
        };
        let payload = [0xaa, 0xbb, 0xcc];
        let group = TileGroup::parse(&payload, &one)?;
        assert_eq!(group.tiles.len(), 1);
        assert_eq!((group.tiles[0].offset, group.tiles[0].size), (0, 3));
        Ok(())
    }

    #[test]
    fn a_tile_size_past_the_data_is_truncated() {
        let payload = [0b0000_0000, 0x09, 0x00, 0xa1];
        assert_eq!(
            TileGroup::parse(&payload, &two_by_two()),
            Err(Error::Truncated)
        );
    }
}
