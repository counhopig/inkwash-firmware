use crate::board::ChargeSnapshot;
use crate::canvas::{Canvas, WIDTH};
use crate::icons::{self, Icon};
use crate::rtc::DateTime;

const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

const WEEKDAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

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

    canvas.stroke_rect(16, 9, 14, 14, 2);
    canvas.fill_rect(21, 14, 4, 4, true);
    canvas.draw_text_prop(38, 8, 1, "INKWASH");

    let percent = battery_percent.unwrap_or(0);
    let battery_icon: &Icon = if charge.fault || charge.no_battery {
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

    const CLUSTER_GAP: usize = 8;
    let battery_x = WIDTH.saturating_sub(battery_icon.width as usize + 16);
    icons::draw_icon(canvas, battery_x, 7, battery_icon);
    let mut cursor_x = battery_x;
    if wifi_configured {
        cursor_x = cursor_x.saturating_sub(CLUSTER_GAP + icons::WIFI.width as usize);
        let wifi_y = 7 + battery_icon.rows.len() - icons::WIFI.rows.len();
        icons::draw_icon(canvas, cursor_x, wifi_y, &icons::WIFI);
    }

    if unread_inbox > 0 {
        let label = if unread_inbox > 99 {
            "99+".to_string()
        } else {
            unread_inbox.to_string()
        };
        let label_w = Canvas::text_prop_width(&label, 1);
        let box_w = label_w + 12;

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

    const CARD_TOP: usize = 151;
    const CARD_H: usize = 133;
    const CARD_W: usize = 176;

    const VALUE_MAX_WIDTH: usize = 152;

    const CAPTION_Y: usize = 98;

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

    canvas.draw_text_prop(16, 46, 5, &time);

    let m_idx = (dt.month as usize).saturating_sub(1).min(11);
    let md = format!("{} {}", MONTH_NAMES[m_idx], dt.day);
    let year_wed = format!("{} · {}", dt.year, WEEKDAYS[(dt.weekday as usize).min(6)]);
    let md_w = Canvas::text_prop_width(&md, 2);
    let year_wed_w = Canvas::text_prop_width(&year_wed, 1);

    canvas.draw_text_prop(WIDTH.saturating_sub(md_w + 16), 59, 2, &md);
    canvas.draw_text_prop(WIDTH.saturating_sub(year_wed_w + 16), 97, 1, &year_wed);
}

fn fit_scale(text: &str, max_width: usize, max_scale: usize) -> usize {
    (1..=max_scale)
        .rev()
        .find(|&scale| Canvas::text_prop_width(text, scale) <= max_width)
        .unwrap_or(1)
}

fn draw_value_centered(canvas: &mut Canvas, x: usize, y: usize, max_width: usize, text: &str) {
    let scale = fit_scale(text, max_width, 3);
    let width = Canvas::text_prop_width(text, scale);
    canvas.draw_text_prop(x + (max_width.saturating_sub(width)) / 2, y, scale, text);
}
