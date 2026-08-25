//! Home-screen layout, kept free of any EPD/FFI dependency so the exact
//! same pixels can be rendered on a PC (see `tools/preview`) and on the
//! device. `display::EpdDisplay::render_home` calls `render` here, then
//! pushes the resulting framebuffer to the panel.

use crate::board::ChargeSnapshot;
use crate::canvas::{Canvas, WIDTH};
use crate::icons::{self, Icon};
use crate::rtc::DateTime;

const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Weekday abbreviations indexed by the RTC's 0=Sunday..6=Saturday.
const WEEKDAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

/// The idle/background screen: clock, Wi-Fi/battery status, next-alarm
/// summary (time, countdown), and a todos summary (open count,
/// due-today count). `main.rs` redraws this after returning from any
/// modal screen (the navigation drawer, settings menu, alarm ring).
#[allow(clippy::too_many_arguments)]
pub fn render(
    canvas: &mut Canvas,
    clock: Option<&DateTime>,
    next_alarm_time: Option<&str>,
    next_alarm_date: Option<&str>,
    next_alarm_days_left: Option<i64>,
    todo_pending: usize,
    todo_due_today: usize,
    unread_inbox: usize,
    wifi_configured: bool,
    battery_percent: Option<u8>,
    charge: ChargeSnapshot,
) {
    canvas.clear();
    // Brand block: a small ink square as the logotype mark (echoes the
    // desktop tool's mark), then the wordmark.
    canvas.stroke_rect(16, 9, 14, 14, 2);
    canvas.fill_rect(21, 14, 4, 4, true);
    canvas.draw_text_prop(38, 8, 1, "INKWASH");

    // Status cluster is right-aligned like every other header, built from
    // icon glyphs (right to left: battery, wifi, inbox-badge). Wi-Fi's icon
    // is omitted entirely when not configured (absence is the "off" signal)
    // rather than drawn in some fainter style: at this pixel size a
    // hollow/outlined variant was tried and was visually indistinguishable
    // from the filled one once actually rendered.
    let percent = battery_percent.unwrap_or(0);
    let battery_icon: &Icon = if charge.fault || charge.no_battery {
        // Neither condition is a normal charge level - showing a
        // percent-derived icon here would imply a reading that isn't
        // meaningful. No dedicated fault/no-battery glyph exists at this
        // icon size, so fall back to the outline (already the device's
        // "pay attention" cue) rather than a falsely-confident percent
        // tier; `charging_state` logs the specific condition.
        &icons::BATTERY_OUTLINE
    } else if charge.charging {
        if percent < 34 {
            &icons::CHARGING_LOW
        } else if percent < 67 {
            &icons::CHARGING_MEDIUM
        } else {
            &icons::CHARGING_HIGH
        }
    } else if charge.full {
        // Full on the charger's own (debounced) signal is more
        // trustworthy than a voltage-derived percent, so show the
        // filled cell regardless of where the percent curve sits.
        &icons::BATTERY_FULL
    } else if percent < 10 {
        &icons::BATTERY_OUTLINE
    } else if percent < 40 {
        &icons::BATTERY_LOW
    } else if percent < 70 {
        &icons::BATTERY_MEDIUM
    } else if percent < 95 {
        &icons::BATTERY_HIGH
    } else {
        &icons::BATTERY_FULL
    };
    // Right-anchor the battery to the header rule (its right edge =
    // x=384), then lay out each preceding element to its left with a fixed
    // 8px gutter, so the cluster never overlaps: battery → wifi → badge.
    const CLUSTER_GAP: usize = 8;
    let battery_x = WIDTH.saturating_sub(battery_icon.width as usize + 16);
    icons::draw_icon(canvas, battery_x, 7, battery_icon);
    let mut cursor_x = battery_x;
    if wifi_configured {
        cursor_x = cursor_x.saturating_sub(CLUSTER_GAP + icons::WIFI.width as usize);
        let wifi_y = 7 + battery_icon.rows.len() - icons::WIFI.rows.len();
        icons::draw_icon(canvas, cursor_x, wifi_y, &icons::WIFI);
    }
    // Unread-inbox badge: a small boxed count in the status cluster so new
    // notifications are visible at a glance without opening the INBOX page.
    // It sits left of whichever element was drawn last, reusing the same
    // gutter, so it can't collide with the wifi glyph.
    if unread_inbox > 0 {
        let label = if unread_inbox > 99 {
            "99+".to_string()
        } else {
            unread_inbox.to_string()
        };
        let label_w = Canvas::text_prop_width(&label, 1);
        let box_w = label_w + 12;
        // The 8x16 glyph is 16px tall, so a box that starts at the same y as
        // the text would clip it. Give the box 2px of padding top and bottom
        // and center the 16px text inside it.
        let box_y = 5usize;
        let box_h = 20usize;
        let box_x = cursor_x.saturating_sub(CLUSTER_GAP + box_w);
        canvas.stroke_rect(box_x, box_y, box_w, box_h, 2);
        canvas.draw_text_prop(box_x + 6, box_y + 2, 1, &label);
    }

    canvas.fill_rect(16, 29, WIDTH - 32, 1, true);
    if let Some(dt) = clock {
        draw_clock(canvas, dt);
    } else {
        let dash_w = Canvas::text_prop_width("--:--", 3);
        let dash_x = (WIDTH - dash_w) / 2;
        canvas.draw_text_prop(dash_x, 52, 3, "--:--");
    }

    // Cards reach down to a slim real bottom margin (300-284=16px) so the
    // page fills the panel instead of floating up off the bottom edge with
    // a large dead band under it. Both cards share the same two-line
    // shape - a big primary value and one caption below it - so they
    // read as a matched pair instead of one running a line longer.
    const CARD_TOP: usize = 151;
    const CARD_H: usize = 133;
    const CARD_W: usize = 176;
    // Value column width available before text would run into the
    // card's own right edge (or, worse, the neighboring card).
    const VALUE_MAX_WIDTH: usize = 152;
    // Caption sits centered in the leftover space below the value
    // (value ink ends around CARD_TOP+83, the card's bottom border is
    // at CARD_TOP+130) rather than right under it - at +88 the caption
    // hugged the value with a lot of dead air beneath it, unbalanced.
    const CAPTION_Y: usize = 98;

    // Card title strip: the accent bar, the all-caps title, then a hairline
    // rule under it so the "title band" reads as a distinct header zone
    // from the big value below - mirroring how the page header top line
    // separates the header from the body.
    canvas.stroke_rect(16, CARD_TOP, CARD_W, CARD_H, 2);
    canvas.fill_rect(16, CARD_TOP, 5, CARD_H, true);
    canvas.draw_text_prop(32, CARD_TOP + 14, 1, "NEXT ALARM");
    canvas.fill_rect(22, CARD_TOP + 33, CARD_W - 7, 1, true);
    match next_alarm_time {
        Some(time) => {
            draw_value_centered(canvas, 32, CARD_TOP + 44, VALUE_MAX_WIDTH, time);
            let caption = match (next_alarm_date, next_alarm_days_left) {
                (None, _) => "EVERY DAY".to_string(),
                (Some(_), Some(0)) => "TODAY".to_string(),
                (Some(date), Some(n)) if n > 0 => format!("NEXT {date}  D+{n}"),
                _ => "EVERY DAY".to_string(),
            };
            let w = Canvas::text_prop_width(&caption, 1);
            canvas.draw_text_prop(
                32 + (VALUE_MAX_WIDTH.saturating_sub(w)) / 2,
                CARD_TOP + CAPTION_Y,
                1,
                &caption,
            );
        }
        None => {
            draw_value_centered(canvas, 32, CARD_TOP + 44, VALUE_MAX_WIDTH, "NONE");
            let caption = "NO ALARMS SET";
            let w = Canvas::text_prop_width(caption, 1);
            // Center on the card's full midline (16 + CARD_W/2) rather
            // than the inner value column, so the long caption visually
            // matches the big "NONE" rather than hugging the left edge.
            let card_center_x = 16 + CARD_W / 2;
            canvas.draw_text_prop(card_center_x - w / 2, CARD_TOP + CAPTION_Y, 1, caption);
        }
    }

    let right_x = 16 + CARD_W + 16;
    canvas.stroke_rect(right_x, CARD_TOP, CARD_W, CARD_H, 2);
    canvas.fill_rect(right_x, CARD_TOP, 5, CARD_H, true);
    canvas.draw_text_prop(right_x + 16, CARD_TOP + 14, 1, "OPEN TODOS");
    canvas.fill_rect(right_x + 6, CARD_TOP + 33, CARD_W - 7, 1, true);
    let todo_count = todo_pending.to_string();
    draw_value_centered(
        canvas,
        right_x + 16,
        CARD_TOP + 44,
        VALUE_MAX_WIDTH,
        &todo_count,
    );
    let due_caption = format!("DUE TODAY {}", todo_due_today);
    let w = Canvas::text_prop_width(&due_caption, 1);
    canvas.draw_text_prop(
        right_x + 16 + (VALUE_MAX_WIDTH.saturating_sub(w)) / 2,
        CARD_TOP + CAPTION_Y,
        1,
        &due_caption,
    );
}

fn draw_clock(canvas: &mut Canvas, dt: &DateTime) {
    let time = format!("{:02}:{:02}", dt.hour, dt.minute);
    // Bigger clock on the left; the date stacks as two lines on the
    // right, sized down a step per line (matching the scale other
    // secondary captions use elsewhere on this screen) so its width
    // stays well clear of the clock's ink even on the widest
    // time/weekday combinations - a scale-2 second line can run wide
    // enough to overlap the clock digits.
    canvas.draw_text_prop(16, 46, 5, &time);

    let m_idx = (dt.month as usize).saturating_sub(1).min(11);
    let md = format!("{} {}", MONTH_NAMES[m_idx], dt.day);
    let year_wed = format!("{} · {}", dt.year, WEEKDAYS[(dt.weekday as usize).min(6)]);
    let md_w = Canvas::text_prop_width(&md, 2);
    let year_wed_w = Canvas::text_prop_width(&year_wed, 1);
    // The two-line date block (32px + 6px gap + 16px = 54px tall) is
    // centered against the clock's 80px height (16px glyph * scale 5)
    // rather than sharing its top edge - top-aligning left it hugging
    // the top of the row with dead space below, unbalanced next to the
    // clock that fills the full height. The text itself is unframed -
    // a border around two lines of plain type read as visual noise on
    // 1bpp, and the separation from the clock is already carried by the
    // empty horizontal space between them. Right edge of each line is
    // pinned to x=384 (the OPEN TODOS card's right edge) so the clock
    // and the date both line up with the cards below them; the clock
    // itself is pinned to the NEXT ALARM card's left edge.
    canvas.draw_text_prop(WIDTH.saturating_sub(md_w + 16), 59, 2, &md);
    canvas.draw_text_prop(WIDTH.saturating_sub(year_wed_w + 16), 97, 1, &year_wed);
}

/// Largest scale in `1..=max_scale` at which `text` fits within
/// `max_width` pixels, falling back to 1 (never drawn narrower, just
/// possibly clipped in a pathological case) if even that doesn't fit.
fn fit_scale(text: &str, max_width: usize, max_scale: usize) -> usize {
    (1..=max_scale)
        .rev()
        .find(|&scale| Canvas::text_prop_width(text, scale) <= max_width)
        .unwrap_or(1)
}

/// Draws `text` at the largest scale (up to 3) that fits `max_width`,
/// centered within that width starting at `x`. A single-digit todo count
/// and an 11-character one-shot alarm date both use this same card slot;
/// left-pinning both at `x` made the short one look orphaned against the
/// long one's near-full-width line, so the value is centered in the
/// available column instead - a short value now reads as "a number
/// deliberately centered in its card", not "text that happened to be
/// short".
fn draw_value_centered(canvas: &mut Canvas, x: usize, y: usize, max_width: usize, text: &str) {
    let scale = fit_scale(text, max_width, 3);
    let width = Canvas::text_prop_width(text, scale);
    canvas.draw_text_prop(x + (max_width.saturating_sub(width)) / 2, y, scale, text);
}
