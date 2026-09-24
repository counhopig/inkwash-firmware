//! Panel geometry shared by the renderer and the EPD worker: the dirty
//! rectangle type, how pending partial refreshes merge, and how a rectangle is
//! cut out of a full 1-bpp frame for the driver's partial-refresh call.

pub const WIDTH: usize = 400;
pub const HEIGHT: usize = 300;
pub const BYTES_PER_ROW: usize = WIDTH / 8;
pub const FRAME_SIZE: usize = BYTES_PER_ROW * HEIGHT;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// Smallest rectangle covering both, clipped to the panel.
pub fn union_rect(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = (a.x + a.width).max(b.x + b.width).min(WIDTH as u16);
    let bottom = (a.y + a.height).max(b.y + b.height).min(HEIGHT as u16);
    Rect {
        x,
        y,
        width: right.saturating_sub(x),
        height: bottom.saturating_sub(y),
    }
}

/// Copies `rect` out of a full MSB-first 1-bpp frame into `out`, packed
/// MSB-first with each row padded to a whole byte.
pub fn pack_rect_from_frame(frame: &[u8], rect: Rect, out: &mut Vec<u8>) {
    let row_bytes = (rect.width as usize).div_ceil(8);
    out.clear();
    out.resize(row_bytes * rect.height as usize, 0);
    for (row, y) in (rect.y..rect.y + rect.height).enumerate() {
        for (column, x) in (rect.x..rect.x + rect.width).enumerate() {
            let index = y as usize * BYTES_PER_ROW + (x as usize) / 8;
            let mask = 1 << (7 - ((x as usize) & 7));
            if frame[index] & mask != 0 {
                out[row * row_bytes + column / 8] |= 1 << (7 - (column & 7));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u16, y: u16, width: u16, height: u16) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn frame_with(pixels: &[(usize, usize)]) -> Vec<u8> {
        let mut frame = vec![0u8; FRAME_SIZE];
        for &(x, y) in pixels {
            frame[y * BYTES_PER_ROW + x / 8] |= 1 << (7 - (x & 7));
        }
        frame
    }

    #[test]
    fn union_covers_both_rectangles() {
        assert_eq!(
            union_rect(rect(10, 20, 5, 5), rect(100, 2, 10, 40)),
            rect(10, 2, 100, 40)
        );
        assert_eq!(
            union_rect(rect(10, 20, 30, 30), rect(15, 25, 5, 5)),
            rect(10, 20, 30, 30)
        );
    }

    #[test]
    fn union_is_clipped_to_the_panel() {
        let merged = union_rect(rect(390, 290, 20, 20), rect(0, 0, 1, 1));
        assert_eq!(merged, rect(0, 0, WIDTH as u16, HEIGHT as u16));
    }

    #[test]
    fn pack_of_the_full_panel_is_the_frame() {
        let frame: Vec<u8> = (0..FRAME_SIZE).map(|i| (i * 37) as u8).collect();
        let mut out = Vec::new();
        pack_rect_from_frame(&frame, rect(0, 0, WIDTH as u16, HEIGHT as u16), &mut out);
        assert_eq!(out, frame);
    }

    #[test]
    fn pack_realigns_an_unaligned_rectangle() {
        let frame = frame_with(&[(3, 7), (12, 7), (3, 8), (13, 8)]);
        let mut out = vec![0xff; 3];
        pack_rect_from_frame(&frame, rect(3, 7, 10, 2), &mut out);
        assert_eq!(
            out,
            vec![0b1000_0000, 0b0100_0000, 0b1000_0000, 0b0000_0000]
        );
    }

    #[test]
    fn pack_ignores_pixels_outside_the_rectangle() {
        let frame = frame_with(&[(2, 7), (13, 7), (3, 6), (3, 9)]);
        let mut out = Vec::new();
        pack_rect_from_frame(&frame, rect(3, 7, 10, 2), &mut out);
        assert!(out.iter().all(|&byte| byte == 0));
    }
}
