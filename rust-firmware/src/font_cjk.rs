pub const CELL_BYTES_16: usize = 32;

pub const CELL_BYTES_12: usize = 24;

pub const WIDTH_16: usize = 17;

pub const WIDTH_12: usize = 13;

static FONT_16: &[u8] = include_bytes!("../assets/hzk16.bin");
static FONT_12: &[u8] = include_bytes!("../assets/hzk12.bin");

static INDEX: &[u8] = include_bytes!("../assets/cjk_index.bin");

fn cell_index(character: char) -> Option<u16> {
    let code = character as u32;
    if code > u32::from(u16::MAX) {
        return None;
    }
    let target = code as u16;
    let entries = INDEX.len() / 4;
    let mut lo = 0usize;
    let mut hi = entries;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let (cp, _) = entry(mid);
        if cp < target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo < entries {
        let (cp, cell) = entry(lo);
        if cp == target {
            return Some(cell);
        }
    }
    None
}

fn entry(i: usize) -> (u16, u16) {
    let off = i * 4;
    (
        u16::from_le_bytes([INDEX[off], INDEX[off + 1]]),
        u16::from_le_bytes([INDEX[off + 2], INDEX[off + 3]]),
    )
}

pub fn glyph16(character: char) -> Option<&'static [u8]> {
    let cell = cell_index(character)? as usize;
    let start = cell * CELL_BYTES_16;
    Some(&FONT_16[start..start + CELL_BYTES_16])
}

pub fn glyph12(character: char) -> Option<&'static [u8]> {
    let cell = cell_index(character)? as usize;
    let start = cell * CELL_BYTES_12;
    Some(&FONT_12[start..start + CELL_BYTES_12])
}

pub fn is_cjk(character: char) -> bool {
    cell_index(character).is_some()
}
