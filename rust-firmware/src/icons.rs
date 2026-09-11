use crate::canvas::Canvas;

pub struct Icon {
    pub width: u8,
    pub rows: &'static [u32],
}

pub fn draw_icon(canvas: &mut Canvas, x: usize, y: usize, icon: &Icon) {
    for (row, bits) in icon.rows.iter().enumerate() {
        for col in 0..icon.width as usize {
            if bits & (1 << (31 - col)) != 0 {
                canvas.set_pixel(x + col, y + row, true);
            }
        }
    }
}

pub const WIFI: Icon = Icon {
    width: 18,
    rows: &[
        0x0003c000, 0x0003c000, 0x0003c000, 0x0003c000, 0x0003c000, 0x0003c000, 0x01e3c000,
        0x01e3c000, 0x01e3c000, 0x01e3c000, 0x01e3c000, 0x01e3c000, 0xf1e3c000, 0xf1e3c000,
        0xf1e3c000, 0xf1e3c000, 0xf1e3c000, 0xf1e3c000,
    ],
};

pub const BATTERY_OUTLINE: Icon = Icon {
    width: 12,
    rows: &[
        0x1f800000, 0x1f800000, 0xfff00000, 0xc0300000, 0xc0300000, 0xc0300000, 0xc0300000,
        0xc0300000, 0xc0300000, 0xc0300000, 0xc0300000, 0xc0300000, 0xc0300000, 0xc0300000,
        0xc0300000, 0xc0300000, 0xc0300000, 0xfff00000,
    ],
};

pub const BATTERY_LOW: Icon = Icon {
    width: 12,
    rows: &[
        0x1f800000, 0x1f800000, 0xfff00000, 0xc0300000, 0xc0300000, 0xc0300000, 0xc0300000,
        0xc0300000, 0xc0300000, 0xc0300000, 0xc0300000, 0xc0300000, 0xc0300000, 0xfff00000,
        0xfff00000, 0xfff00000, 0xfff00000, 0xfff00000,
    ],
};

pub const BATTERY_MEDIUM: Icon = Icon {
    width: 12,
    rows: &[
        0x1f800000, 0x1f800000, 0xfff00000, 0xc0300000, 0xc0300000, 0xc0300000, 0xc0300000,
        0xc0300000, 0xc0300000, 0xc0300000, 0xfff00000, 0xfff00000, 0xfff00000, 0xfff00000,
        0xfff00000, 0xfff00000, 0xfff00000, 0xfff00000,
    ],
};

pub const BATTERY_HIGH: Icon = Icon {
    width: 12,
    rows: &[
        0x1f800000, 0x1f800000, 0xfff00000, 0xc0300000, 0xc0300000, 0xc0300000, 0xc0300000,
        0xfff00000, 0xfff00000, 0xfff00000, 0xfff00000, 0xfff00000, 0xfff00000, 0xfff00000,
        0xfff00000, 0xfff00000, 0xfff00000, 0xfff00000,
    ],
};

pub const BATTERY_FULL: Icon = Icon {
    width: 12,
    rows: &[
        0x1f800000, 0x1f800000, 0xfff00000, 0xfff00000, 0xfff00000, 0xfff00000, 0xfff00000,
        0xfff00000, 0xfff00000, 0xfff00000, 0xfff00000, 0xfff00000, 0xfff00000, 0xfff00000,
        0xfff00000, 0xfff00000, 0xfff00000, 0xfff00000,
    ],
};

pub const CHARGING_LOW: Icon = Icon {
    width: 12,
    rows: &[
        0x1f800000, 0x1f800000, 0xfff00000, 0xc0300000, 0xd8300000, 0xd8300000, 0xd8300000,
        0xcc300000, 0xcc300000, 0xcc300000, 0xc6300000, 0xc6300000, 0xc6300000, 0xc0300000,
        0xc0300000, 0xc0300000, 0xc0300000, 0xfff00000,
    ],
};

pub const CHARGING_MEDIUM: Icon = Icon {
    width: 12,
    rows: &[
        0x1f800000, 0x1f800000, 0xfff00000, 0xc0300000, 0xd8300000, 0xd8300000, 0xd8300000,
        0xcc300000, 0xcc300000, 0xcc300000, 0xc6300000, 0xc6300000, 0xc6300000, 0xc0300000,
        0xc0300000, 0xc0300000, 0xc0300000, 0xfff00000,
    ],
};

pub const CHARGING_HIGH: Icon = Icon {
    width: 12,
    rows: &[
        0x1f800000, 0x1f800000, 0xfff00000, 0xc0300000, 0xd8300000, 0xd8300000, 0xd8300000,
        0xcc300000, 0xcc300000, 0xcc300000, 0xc6300000, 0xc6300000, 0xc6300000, 0xc0300000,
        0xc0300000, 0xc0300000, 0xc0300000, 0xfff00000,
    ],
};
