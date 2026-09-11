use crate::font8x16;
use crate::font_cjk;

pub const WIDTH: usize = 400;
pub const HEIGHT: usize = 300;
const BYTES_PER_ROW: usize = WIDTH / 8;
const FRAME_SIZE: usize = BYTES_PER_ROW * HEIGHT;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

pub struct Canvas {
    frame: Vec<u8>,
}

impl Canvas {
    pub fn new() -> Self {
        Self {
            frame: vec![0xFF; FRAME_SIZE],
        }
    }

    pub fn frame(&self) -> &[u8] {
        &self.frame
    }

    pub fn clear(&mut self) {
        self.frame.fill(0xFF);
    }

    pub fn set_pixel(&mut self, x: usize, y: usize, black: bool) {
        if x >= WIDTH || y >= HEIGHT {
            return;
        }
        let index = y * BYTES_PER_ROW + x / 8;
        let mask = 1 << (7 - (x & 7));
        if black {
            self.frame[index] &= !mask;
        } else {
            self.frame[index] |= mask;
        }
    }

    pub fn fill_rect(&mut self, x: usize, y: usize, width: usize, height: usize, black: bool) {
        for yy in y..y.saturating_add(height) {
            for xx in x..x.saturating_add(width) {
                self.set_pixel(xx, yy, black);
            }
        }
    }

    pub fn stroke_rect(
        &mut self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        thickness: usize,
    ) {
        if width == 0 || height == 0 || thickness == 0 {
            return;
        }
        let t = thickness.min(width).min(height);
        self.fill_rect(x, y, width, t, true);
        self.fill_rect(x, y + height.saturating_sub(t), width, t, true);
        self.fill_rect(x, y, t, height, true);
        self.fill_rect(x + width.saturating_sub(t), y, t, height, true);
    }

    pub fn draw_text_prop(&mut self, x: usize, y: usize, scale: usize, text: &str) -> usize {
        self.draw_text_prop_ink(x, y, scale, text, true)
    }

    pub fn draw_text_small(&mut self, x: usize, y: usize, text: &str) -> usize {
        let mut cursor = x;
        for character in text.chars() {
            if let Some(glyph) = font_cjk::glyph12(character) {
                for row in 0..12usize {
                    let (hi, lo) = (glyph[row * 2], glyph[row * 2 + 1]);
                    for column in 0..12usize {
                        let bit = if column < 8 {
                            hi >> (7 - column) & 1
                        } else {
                            lo >> (15 - column) & 1
                        };
                        if bit != 0 {
                            self.set_pixel(cursor + column, y + row, true);
                        }
                    }
                }
                cursor += font_cjk::WIDTH_12;
                continue;
            }
            let (rows, width) = crate::font5x7::glyph5x7(character);
            for (row, bits) in rows.iter().enumerate() {
                for column in 0..width as usize {
                    if bits & (1 << (4 - column)) != 0 {
                        self.set_pixel(cursor + column, y + row, true);
                    }
                }
            }
            cursor += width as usize + 1;
        }
        cursor - x
    }

    pub fn text_small_width(text: &str) -> usize {
        text.chars()
            .map(|c| {
                if font_cjk::is_cjk(c) {
                    font_cjk::WIDTH_12
                } else {
                    (crate::font5x7::glyph5x7(c).1 as usize) + 1
                }
            })
            .sum()
    }

    pub fn text_prop_width(text: &str, scale: usize) -> usize {
        text.chars()
            .map(|c| {
                if font_cjk::is_cjk(c) {
                    font_cjk::WIDTH_16 * scale
                } else {
                    (font8x16::glyph(c).1 as usize + 1) * scale
                }
            })
            .sum()
    }

    fn draw_text_prop_ink(
        &mut self,
        x: usize,
        y: usize,
        scale: usize,
        text: &str,
        black: bool,
    ) -> usize {
        let mut cursor = x;
        for character in text.chars() {
            if let Some(glyph) = font_cjk::glyph16(character) {
                for (row, bytes) in glyph.chunks_exact(2).enumerate() {
                    let (hi, lo) = (bytes[0], bytes[1]);
                    for column in 0..16usize {
                        let bit = if column < 8 {
                            hi >> (7 - column) & 1
                        } else {
                            lo >> (15 - column) & 1
                        };
                        if bit != 0 {
                            self.fill_rect(
                                cursor + column * scale,
                                y + row * scale,
                                scale,
                                scale,
                                black,
                            );
                        }
                    }
                }
                cursor += font_cjk::WIDTH_16 * scale;
                continue;
            }
            let (rows, width) = font8x16::glyph(character);
            for (row, bits) in rows.iter().enumerate() {
                for column in 0..width as usize {
                    if bits & (1 << (15 - column)) != 0 {
                        self.fill_rect(
                            cursor + column * scale,
                            y + row * scale,
                            scale,
                            scale,
                            black,
                        );
                    }
                }
            }
            cursor += (width as usize + 1) * scale;
        }
        cursor - x
    }
}

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
