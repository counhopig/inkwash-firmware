//! PC preview of the on-device screens. Draws exactly the same pixels the
//! firmware would (the render modules are `#[path]`-included in place from
//! `rust-firmware/src/`) and writes PNGs, so home-screen layout can be
//! iterated without flashing a device.
//!
//! Run: `cargo run --release` from `tools/preview/` (host toolchain). Writes
//! `*.png` into the current directory.

#[path = "../../../rust-firmware/src/canvas.rs"]
mod canvas;

#[path = "../../../rust-firmware/src/font8x16.rs"]
mod font8x16;

#[path = "../../../rust-firmware/src/font_cjk.rs"]
mod font_cjk;

#[path = "../../../rust-firmware/src/font5x7.rs"]
mod font5x7;

#[path = "../../../rust-firmware/src/icons.rs"]
mod icons;

#[path = "../../../rust-firmware/src/home.rs"]
mod home;

/// Minimal stand-ins for the firmware-only types `home.rs` depends on.
mod board {
    pub struct ChargeSnapshot {
        pub power_present: bool,
        pub charging: bool,
        pub full: bool,
        pub fault: bool,
        pub no_battery: bool,
    }
}

mod rtc {
    #[derive(Clone, Copy)]
    pub struct DateTime {
        pub year: u16,
        pub month: u8,
        pub day: u8,
        pub weekday: u8,
        pub hour: u8,
        pub minute: u8,
        pub second: u8,
        pub voltage_low: bool,
    }
}

use canvas::{Canvas, HEIGHT, WIDTH};
use rtc::DateTime;

fn save(path: &str, canvas: &Canvas) {
    let img = image::GrayImage::from_fn(WIDTH as u32, HEIGHT as u32, |x, y| {
        // Frame bit 1 = white, 0 = black; map to 255 (white) / 0 (black).
        let byte = canvas.frame()[y as usize * (WIDTH / 8) + (x as usize / 8)];
        let white = byte & (1 << (7 - (x as usize & 7))) != 0;
        image::Luma([if white { 255 } else { 0 }])
    });
    img.save(path).expect("write png");
    println!("wrote {path}");
}

fn dt() -> DateTime {
    DateTime {
        year: 2026,
        month: 8,
        day: 20,
        weekday: 4, // THU
        hour: 14,
        minute: 50,
        second: 0,
        voltage_low: false,
    }
}

fn full_battery() -> board::ChargeSnapshot {
    board::ChargeSnapshot {
        power_present: false,
        charging: false,
        full: false,
        fault: false,
        no_battery: false,
    }
}

fn main() {
    // Scene 1: home screen, everything populated (README's home.png).
    let mut c = Canvas::new();
    home::render(
        &mut c,
        Some(&dt()),
        Some("07:30"),
        Some("AUG 20"),
        Some(0),
        3,
        1,
        3,
        true,
        Some(85),
        full_battery(),
    );
    save("home.png", &c);

    // Scene 2: no clock (RTC lost), no alarm, no wifi, low battery.
    let mut c = Canvas::new();
    home::render(
        &mut c,
        None,
        None,
        None,
        None,
        0,
        0,
        0,
        false,
        Some(8),
        board::ChargeSnapshot {
            power_present: false,
            charging: false,
            full: false,
            fault: false,
            no_battery: false,
        },
    );
    save("home-empty.png", &c);

    // Scene 3: charging + one-shot alarm in a few days.
    let mut c = Canvas::new();
    home::render(
        &mut c,
        Some(&dt()),
        Some("SEP 1"),
        Some("SEP 1"),
        Some(12),
        1,
        0,
        7,
        true,
        Some(45),
        board::ChargeSnapshot {
            power_present: true,
            charging: true,
            full: false,
            fault: false,
            no_battery: false,
        },
    );
    save("home-charging.png", &c);

    // Sub-screen scenes: share the home screen's header (brand mark +
    // wordmark + right-aligned title + rule) so every page matches the
    // home visual language. The list row chrome mirrors `ui::draw_rows`
    // (stroke + left accent bar on the selection).
    fn header(canvas: &mut Canvas, title: &str) {
        canvas.stroke_rect(16, 9, 14, 14, 2);
        canvas.fill_rect(21, 14, 4, 4, true);
        canvas.draw_text_prop(38, 8, 1, "INKWASH");
        let width = canvas::Canvas::text_prop_width(title, 1);
        canvas.draw_text_prop(384usize.saturating_sub(width), 8, 1, title);
        canvas.fill_rect(16, 29, 368, 1, true);
    }

    // Settings list, selection on the second row.
    let mut c = Canvas::new();
    c.clear();
    header(&mut c, "SETTINGS");
    let items = ["SYNC INTERVAL", "TIME ZONE", "GO TO", "ABOUT"];
    let mut y = 39usize;
    for (i, item) in items.iter().enumerate() {
        if i == 1 {
            c.stroke_rect(16, y, 368, 35, 2);
            c.fill_rect(16, y, 5, 35, true);
        }
        c.draw_text_prop(50, y + 10, 1, item);
        y += 37;
    }
    save("page-list.png", &c);

    // Number picker (mirrors `ui::pick_number` layout).
    let mut c = Canvas::new();
    c.clear();
    header(&mut c, "HOUR");
    let label = "07";
    let nw = canvas::Canvas::text_prop_width(label, 5);
    let box_w = nw + 64;
    let box_x = 200usize.saturating_sub(box_w / 2);
    let cap_w = canvas::Canvas::text_prop_width("CHOOSE VALUE", 1);
    c.draw_text_prop((400usize.saturating_sub(cap_w)) / 2, 87, 1, "CHOOSE VALUE");
    c.stroke_rect(box_x, 123, box_w, 120, 3);
    c.fill_rect(box_x, 123, 7, 120, true);
    c.draw_text_prop(box_x + 7 + (box_w - 7 - nw) / 2, 143, 5, label);
    save("page-number.png", &c);

    // GO TO navigation drawer: overlaid on the home screen, with a thick
    // vertical rule down its right edge (mirrors screens::draw_navigation_bar).
    let mut c = Canvas::new();
    home::render(
        &mut c,
        Some(&dt()),
        Some("07:30"),
        Some("AUG 20"),
        Some(0),
        3,
        1,
        0,
        true,
        Some(85),
        full_battery(),
    );
    c.fill_rect(16, 34, 176, 250, false);
    c.draw_text_prop(24, 42, 1, "GO TO");
    c.fill_rect(24, 58, 160, 1, true);
    let dests = ["HOME", "CALENDAR", "INBOX", "ALARMS", "TODOS", "SETTINGS"];
    for (i, d) in dests.iter().enumerate() {
        let y = 64 + i * 33;
        if i == 2 {
            c.stroke_rect(22, y, 164, 31, 2);
            c.fill_rect(22, y, 5, 31, true);
        }
        c.draw_text_prop(30, y + 10, 1, d);
    }
    c.fill_rect(16 + 176 - 3, 34, 3, 266, true);
    save("goto.png", &c);

    // Calendar month grid (README's calendar.png, mirrors
    // screens::draw_month_grid). August 2026 starts on a Saturday.
    let mut c = Canvas::new();
    c.clear();
    header(&mut c, "CALENDAR");
    c.draw_text_prop(16, 38, 2, "2026 / 08");
    const CAL_LABELS: [&str; 7] = ["SU", "MO", "TU", "WE", "TH", "FR", "SA"];
    const CAL_COL: usize = 53;
    const CAL_ORIGIN_X: usize = 18;
    const CAL_ORIGIN_Y: usize = 75;
    const CAL_ROW: usize = 32;
    for (i, l) in CAL_LABELS.iter().enumerate() {
        c.draw_text_prop(CAL_ORIGIN_X + i * CAL_COL, CAL_ORIGIN_Y, 1, l);
    }
    c.fill_rect(16, 99, 368, 1, true);
    let mut col = 6usize; // 2026-08-01 = Saturday
    let mut row = 1usize;
    for day in 1..=31 {
        let x = CAL_ORIGIN_X + col * CAL_COL;
        let y = CAL_ORIGIN_Y + row * CAL_ROW;
        if day == 20 {
            c.stroke_rect(x.saturating_sub(6), y.saturating_sub(4), 34, 30, 2);
            c.fill_rect(x.saturating_sub(6), y.saturating_sub(4), 4, 30, true);
            let w = canvas::Canvas::text_prop_width(&day.to_string(), 1);
            c.fill_rect(x, y + 16, w, 1, true);
            c.fill_rect(x, y + 18, 4, 4, true);
        }
        c.draw_text_prop(x, y, 1, &day.to_string());
        col += 1;
        if col > 6 {
            col = 0;
            row += 1;
        }
    }
    save("calendar.png", &c);

    // Week view (README's week-view.png, mirrors screens::week_view):
    // compact day cards, an outlined/accented opened day, and bullet-led
    // todo stacks separated by short inset rules.
    fn wrap_text_small(text: &str, max_width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        for word in text.split_whitespace() {
            let mut remaining = word;
            loop {
                let candidate = if current.is_empty() {
                    remaining.to_string()
                } else {
                    format!("{current} {remaining}")
                };
                if canvas::Canvas::text_small_width(&candidate) <= max_width {
                    current = candidate;
                    break;
                }
                if !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                    continue;
                }
                let mut split = remaining.len();
                while split > 0 && canvas::Canvas::text_small_width(&remaining[..split]) > max_width
                {
                    split = remaining[..split].char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
                }
                if split == 0 {
                    split = remaining.chars().next().map(|c| c.len_utf8()).unwrap_or(0);
                }
                lines.push(remaining[..split].to_string());
                remaining = &remaining[split..];
                if remaining.is_empty() {
                    break;
                }
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
        lines
    }

    fn wrap_text_prop(text: &str, max_width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        for word in text.split_whitespace() {
            let mut remaining = word;
            loop {
                let candidate = if current.is_empty() {
                    remaining.to_string()
                } else {
                    format!("{current} {remaining}")
                };
                if canvas::Canvas::text_prop_width(&candidate, 1) <= max_width {
                    current = candidate;
                    break;
                }
                if !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                    continue;
                }
                let mut split = remaining.len();
                while split > 0 && canvas::Canvas::text_prop_width(&remaining[..split], 1) > max_width
                {
                    split = remaining[..split].char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
                }
                if split == 0 {
                    split = remaining.chars().next().map(|c| c.len_utf8()).unwrap_or(0);
                }
                lines.push(remaining[..split].to_string());
                remaining = &remaining[split..];
                if remaining.is_empty() {
                    break;
                }
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
        lines
    }
    let mut c = Canvas::new();
    c.clear();
    header(&mut c, "AUG 16-22");
    const WV_COL: usize = 50;
    const WV_GAP: usize = 3;
    const WV_CARD_TOP: usize = 38;
    const WV_CARD_H: usize = 40;
    const WV_WD: usize = 43;
    const WV_DATE: usize = 56;
    const WV_LIST: usize = 88;
    const WV_LINE: usize = 8;
    const WV_ITEM_GAP: usize = 5;
    const WV_BOTTOM: usize = 296;
    let wdays = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
    let dates = [16, 17, 18, 19, 20, 21, 22];
    // A deliberately long todo on WED proves word-wrap keeps it inside the
    // column; the rest stay short so the overflow is easy to spot if it
    // ever regresses.
    let todos: [Vec<String>; 7] = [
        vec![],
        vec!["call mum".into()],
        vec![],
        vec!["remember to water the plants before the weekend trip".into()],
        vec!["buy oats".into(), "pay rent on time".into()],
        vec![],
        vec!["weekend".into(), "cleanup".into()],
    ];
    const MAX_TODO_LINES: usize = 3;
    const TEXT_INSET: usize = 8;
    let text_w = WV_COL.saturating_sub(TEXT_INSET + 2);
    for i in 0..7usize {
        let x = 16 + i * (WV_COL + WV_GAP);
        if i == 4 {
            c.stroke_rect(x, WV_CARD_TOP, WV_COL, WV_CARD_H, 2);
            c.fill_rect(x, WV_CARD_TOP + WV_CARD_H - 4, WV_COL, 4, true);
            c.fill_rect(x + WV_COL - 7, WV_CARD_TOP + 4, 3, 3, true);
        }
        let weekday_w = canvas::Canvas::text_small_width(wdays[i]);
        c.draw_text_small(x + (WV_COL - weekday_w) / 2, WV_WD, wdays[i]);
        let date = dates[i].to_string();
        let date_w = canvas::Canvas::text_prop_width(&date, 1);
        c.draw_text_prop(x + (WV_COL - date_w) / 2, WV_DATE, 1, &date);
        let mut yy = WV_LIST;
        'day: for text in &todos[i] {
            let mut lines = wrap_text_small(text, text_w);
            let truncated = lines.len() > MAX_TODO_LINES;
            lines.truncate(MAX_TODO_LINES);
            c.fill_rect(x + 1, yy + 2, 3, 3, true);
            for (line_index, line) in lines.iter().enumerate() {
                if yy + 7 > WV_BOTTOM {
                    break 'day;
                }
                let text_x = x + TEXT_INSET;
                if truncated && line_index + 1 == lines.len() {
                    let ellipsis_w = canvas::Canvas::text_small_width("...");
                    let mut end = line.len();
                    while end > 0
                        && canvas::Canvas::text_small_width(&line[..end]) + ellipsis_w > text_w
                    {
                        end -= 1;
                    }
                    c.draw_text_small(text_x, yy, &line[..end]);
                    c.draw_text_small(
                        text_x + canvas::Canvas::text_small_width(&line[..end]),
                        yy,
                        "...",
                    );
                } else {
                    c.draw_text_small(text_x, yy, line);
                }
                yy += WV_LINE;
            }
            yy += WV_ITEM_GAP;
            if yy <= WV_BOTTOM {
                c.fill_rect(x + TEXT_INSET, yy - 2, text_w, 1, true);
            }
        }
    }
    save("week-view.png", &c);

    // Alarms list (README's alarms.png, mirrors screens::render_alarm_page).
    let mut c = Canvas::new();
    c.clear();
    header(&mut c, "ALARMS");
    let alarm_rows = [
        "[X] 07:30 DAILY  Wake up",
        "[ ] 21:15 SU,MO,WE  Water plants",
        "[X] 06:00 DAY 1,15  Pay bills",
        "+ ADD ALARM",
    ];
    for (i, r) in alarm_rows.iter().enumerate() {
        let y = 39 + i * 37;
        if i == 0 {
            c.stroke_rect(16, y, 368, 35, 2);
            c.fill_rect(16, y, 5, 35, true);
        }
        c.draw_text_prop(50, y + 10, 1, r);
    }
    save("alarms.png", &c);

    // Todos list (README's todos.png, mirrors screens::render_todo_page).
    let mut c = Canvas::new();
    c.clear();
    header(&mut c, "TODOS");
    let todo_rows = [
        "[ ] !! Buy oats - DUE TODAY",
        "[X] Water the plant",
        "[ ] ! Call mum - 08/24",
        "[ ] Review notes",
    ];
    for (i, r) in todo_rows.iter().enumerate() {
        let y = 39 + i * 37;
        if i == 2 {
            c.stroke_rect(16, y, 368, 35, 2);
            c.fill_rect(16, y, 5, 35, true);
        }
        c.draw_text_prop(50, y + 10, 1, r);
    }
    save("todos.png", &c);

    // INBOX list page (mirrors screens::render_inbox_page + the home badge).
    let mut c = Canvas::new();
    c.clear();
    header(&mut c, "INBOX");
    let inbox_rows = [
        "○ Build failed",
        "○ Deploy complete",
        "○ Team Standup",
        "• Weekly digest",
    ];
    for (i, r) in inbox_rows.iter().enumerate() {
        let y = 39 + i * 37;
        if i == 0 {
            c.stroke_rect(16, y, 368, 35, 2);
            c.fill_rect(16, y, 5, 35, true);
        }
        c.draw_text_prop(50, y + 10, 1, r);
    }
    save("inbox.png", &c);

    // CJK rendering check: mixed Chinese + ASCII text (mirrors the
    // notification titles/bodies the pipeline sends).
    let mut c = Canvas::new();
    c.clear();
    header(&mut c, "通知");
    c.draw_text_prop(16, 40, 2, "完成 · inkwash-workspace");
    c.fill_rect(16, 74, 368, 1, true);
    let body = "opencode 已完成任务，详情：child_process 修复完成，中文渲染测试通过。This is an ASCII suffix.";
    let mut yy = 82usize;
    for line in wrap_text_prop(body, 368) {
        if yy + 16 > 282 {
            break;
        }
        c.draw_text_prop(16, yy, 1, &line);
        yy += 18;
    }
    c.draw_text_prop(16, 268, 1, "ENTER = 关闭");
    save("cjk-detail.png", &c);

    // URGENT full-screen reminder (mirrors main.rs remind_urgent_screen).
    let mut c = Canvas::new();
    c.clear();
    header(&mut c, "URGENT");
    let urgent_titles = ["MCP URGENT", "Build failed", "Deploy rollback"];
    for (i, t) in urgent_titles.iter().enumerate() {
        c.draw_text_prop(16, 48 + i * 24, 1, &format!("!! {t}"));
    }
    c.draw_text_prop(16, 284, 1, "ENTER = DISMISS");
    save("urgent.png", &c);

    // ALARM ring screen (mirrors main.rs ring_alarm_until_dismissed).
    let mut c = Canvas::new();
    c.clear();
    header(&mut c, "ALARM");
    let alarm_w = canvas::Canvas::text_prop_width("ALARM", 4);
    c.draw_text_prop(200usize.saturating_sub(alarm_w / 2), 92, 4, "ALARM");
    let hint = "ENTER = DISMISS";
    let hint_w = canvas::Canvas::text_prop_width(hint, 1);
    c.draw_text_prop(200usize.saturating_sub(hint_w / 2), 184, 1, hint);
    save("alarm-ring.png", &c);

    // INBOX item detail with wrapped body.
    let mut c = Canvas::new();
    c.clear();
    header(&mut c, "INBOX");
    c.draw_text_prop(16, 40, 2, "Build failed");
    c.fill_rect(16, 74, 368, 1, true);
    let body = "main / test-linux — the integration suite timed out after 45 minutes. Check the runner logs for the exact failing test.";
    let mut yy = 82usize;
    for line in wrap_text_prop(body, 368) {
        if yy + 16 > 282 {
            break;
        }
        c.draw_text_prop(16, yy, 1, &line);
        yy += 18;
    }
    save("inbox-detail.png", &c);

    // CJK long-title detail: wraps to two scale-1 lines instead of
    // overflowing (mirrors open_inbox_item's title fitting).
    let mut c = Canvas::new();
    c.clear();
    header(&mut c, "通知");
    let title = "完成 · inkwash-workspace 的超长中文标题测试";
    if canvas::Canvas::text_prop_width(title, 2) <= 368 {
        c.draw_text_prop(16, 40, 2, title);
    } else {
        let lines = wrap_text_prop(title, 368);
        for (i, line) in lines.iter().take(2).enumerate() {
            c.draw_text_prop(16, 38 + i * 22, 1, line);
        }
    }
    c.fill_rect(16, 92, 368, 1, true);
    let body = "长标题会自动换行到两行，分隔线保持在标题下方，正文正常排版。";
    let mut yy = 100usize;
    for line in wrap_text_prop(body, 368) {
        if yy + 16 > 282 {
            break;
        }
        c.draw_text_prop(16, yy, 1, &line);
        yy += 18;
    }
    save("cjk-long-title.png", &c);
}
