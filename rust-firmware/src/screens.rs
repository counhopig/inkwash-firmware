use crate::alarms::{self, Repeat, StoredAlarm};
use crate::board::Note4Board;
use crate::canvas::Canvas;
use crate::ctx::DeviceContext;
use crate::display::Rect;
use crate::inbox::InboxItem;
use crate::rtc::{is_leap, DateTime};
use crate::todos::Importance;
use crate::ui::{draw_rows, footer, header};

pub const LIST_TEXT_MAX_WIDTH: usize = 334;

pub fn truncate_prop(text: &str, max_width: usize) -> String {
    let mut width = 0usize;
    let mut out = String::new();
    let ellipsis = "…";
    let ellipsis_w = Canvas::text_prop_width(ellipsis, 1);
    let mut truncated = false;
    for c in text.chars() {
        let w = if crate::font_cjk::is_cjk(c) {
            crate::font_cjk::WIDTH_16
        } else {
            (crate::font8x16::glyph(c).1 as usize) + 1
        };
        if width + w + ellipsis_w > max_width {
            truncated = true;
            break;
        }
        width += w;
        out.push(c);
    }
    if truncated {
        out.push_str(ellipsis);
    }
    out
}

pub(crate) const NAV_BAR_RECT: Rect = Rect {
    x: 16,
    y: 34,
    width: 176,
    height: 266,
};
const NAV_BAR_ROW_H: usize = 33;

const NAV_DESTINATIONS: [&str; 6] = ["HOME", "CALENDAR", "INBOX", "ALARMS", "TODOS", "SETTINGS"];

pub(crate) fn draw_navigation_bar(canvas: &mut Canvas, selected: usize) {
    canvas.fill_rect(
        NAV_BAR_RECT.x as usize,
        NAV_BAR_RECT.y as usize,
        NAV_BAR_RECT.width as usize,
        NAV_BAR_RECT.height as usize,
        false,
    );
    canvas.draw_text_prop(24, 42, 1, "GO TO");
    canvas.fill_rect(24, 58, NAV_BAR_RECT.width as usize - 16, 1, true);
    let mut y = 64usize;
    for (index, label) in NAV_DESTINATIONS.iter().enumerate() {
        if index == selected {
            canvas.stroke_rect(
                22,
                y,
                NAV_BAR_RECT.width as usize - 12,
                NAV_BAR_ROW_H - 2,
                2,
            );
            canvas.fill_rect(22, y, 5, NAV_BAR_ROW_H - 2, true);
        }
        canvas.draw_text_prop(30, y + 10, 1, label);
        y += NAV_BAR_ROW_H;
    }

    let rule_x = NAV_BAR_RECT.x as usize + NAV_BAR_RECT.width as usize - 3;
    canvas.fill_rect(
        rule_x,
        NAV_BAR_RECT.y as usize,
        3,
        NAV_BAR_RECT.height as usize,
        true,
    );
}

pub const SETTINGS_ROWS: [&str; 4] = ["SYNC NOW", "SYNC INTERVAL", "BLE PAIRING", "SLEEP"];

pub(crate) fn draw_settings(canvas: &mut Canvas, selected: usize) {
    let items: Vec<String> = SETTINGS_ROWS.iter().map(|s| s.to_string()).collect();
    draw_rows(canvas, "SETTINGS", &items, selected);
    footer(canvas, "UP/DOWN MOVE   ENTER OK   HOLD ENTER BACK");
}

const SYNC_INTERVAL_OPTIONS: [&str; 5] = ["1 MIN", "5 MIN", "10 MIN", "30 MIN", "60 MIN"];

pub(crate) fn draw_sync_interval(canvas: &mut Canvas, selected: usize) {
    let items: Vec<String> = SYNC_INTERVAL_OPTIONS
        .iter()
        .map(|s| s.to_string())
        .collect();
    draw_rows(canvas, "SYNC INTERVAL", &items, selected);
    footer(canvas, "UP/DOWN MOVE   ENTER OK   HOLD ENTER BACK");
}

pub(crate) fn draw_calendar_grid(
    ctx: &mut DeviceContext,
    now: Option<&DateTime>,
    selected_day: u8,
    todos: &[crate::todos::Todo],
) {
    let mut canvas = ctx.board.display.canvas_mut();
    canvas.clear();
    header(&mut canvas, "CALENDAR");
    if let Some(dt) = now {
        let mut marks = [DayMark::default(); 32];
        let dim = days_in_month(dt.year, dt.month);
        for todo in todos {
            for day in 1..=dim {
                let fires = match &todo.repeat {
                    Some(r) => {
                        r.fires_on(dt.year, dt.month, day, weekday_of(dt.year, dt.month, day))
                    }
                    None => todo
                        .due_date
                        .is_some_and(|d| d.year == dt.year && d.month == dt.month && d.day == day),
                };
                if fires {
                    marks[day as usize].todo = Some(todo.importance);
                }
            }
        }
        let selected = selected_day.min(dim).max(1);
        draw_month_grid(&mut canvas, dt.year, dt.month, now, selected, &marks);
    }
    footer(
        &mut canvas,
        "UP/DOWN MOVE   ENTER WEEK VIEW   HOLD UP/DOWN SWITCH PAGE",
    );
}

#[derive(Clone, Copy, Default)]
struct DayMark {
    todo: Option<Importance>,
}

fn draw_month_grid(
    canvas: &mut crate::canvas::Canvas,
    year: u16,
    month: u8,
    today: Option<&DateTime>,
    selected_day: u8,
    marks: &[DayMark; 32],
) {
    let title = format!("{:04} / {:02}", year, month);
    canvas.draw_text_prop(16, 38, 2, &title);

    const LABELS: [&str; 7] = ["SU", "MO", "TU", "WE", "TH", "FR", "SA"];
    const COL_WIDTH: usize = 53;

    const ROW_HEIGHT: usize = 32;
    const ORIGIN_X: usize = 18;
    const ORIGIN_Y: usize = 75;

    const MARKER_Y: usize = 18;

    for (i, label) in LABELS.iter().enumerate() {
        let x = ORIGIN_X + i * COL_WIDTH;
        canvas.draw_text_prop(x, ORIGIN_Y, 1, label);
    }
    canvas.fill_rect(16, 99, 368, 1, true);

    let days_in_month = days_in_month(year, month);
    let mut col = weekday_of(year, month, 1) as usize;
    let mut row = 1usize;
    for day in 1..=days_in_month {
        let x = ORIGIN_X + col * COL_WIDTH;
        let y = ORIGIN_Y + row * ROW_HEIGHT;
        let text = day.to_string();
        let is_today = today
            .map(|dt| dt.year == year && dt.month == month && dt.day == day)
            .unwrap_or(false);

        if day == selected_day {
            canvas.stroke_rect(x.saturating_sub(6), y.saturating_sub(4), 34, 30, 2);
            canvas.fill_rect(x.saturating_sub(6), y.saturating_sub(4), 4, 30, true);
        }
        canvas.draw_text_prop(x, y, 1, &text);
        if is_today {
            let w = Canvas::text_prop_width(&text, 1);
            canvas.fill_rect(x, y + 16, w, 1, true);
        }
        if let Some(importance) = marks[day as usize].todo {
            let size = if importance == Importance::High { 6 } else { 4 };
            canvas.fill_rect(x, y + MARKER_Y, size, size, true);
        }
        col += 1;
        if col > 6 {
            col = 0;
            row += 1;
        }
    }
}

const MONTH_NAMES: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];

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
            if Canvas::text_small_width(&candidate) <= max_width {
                current = candidate;
                break;
            }
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                continue;
            }
            let mut split = remaining.len();
            while split > 0 && Canvas::text_small_width(&remaining[..split]) > max_width {
                split = remaining[..split]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
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
            if Canvas::text_prop_width(&candidate, 1) <= max_width {
                current = candidate;
                break;
            }
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                continue;
            }
            let mut split = remaining.len();
            while split > 0 && Canvas::text_prop_width(&remaining[..split], 1) > max_width {
                split = remaining[..split]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
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

pub(crate) fn draw_week_view(
    ctx: &mut DeviceContext,
    todos: &[crate::todos::Todo],
    year: u16,
    month: u8,
    day: u8,
    now: Option<&DateTime>,
) {
    let start = alarms::days_since_epoch(year, month, day) - weekday_of(year, month, day) as i64;
    let (_, sm, sd) = alarms::date_from_days(start);
    let (_, em, ed) = alarms::date_from_days(start + 6);
    let title = if sm == em {
        format!("{} {}-{}", MONTH_NAMES[(sm - 1) as usize], sd, ed)
    } else {
        format!(
            "{} {} - {} {}",
            MONTH_NAMES[(sm - 1) as usize],
            sd,
            MONTH_NAMES[(em - 1) as usize],
            ed
        )
    };

    let mut canvas = ctx.board.display.canvas_mut();
    canvas.clear();
    header(&mut canvas, &title);

    const ORIGIN_X: usize = 16;
    const COL_WIDTH: usize = 50;
    const COL_GAP: usize = 3;
    const CARD_TOP: usize = 38;
    const CARD_HEIGHT: usize = 40;
    const WEEKDAY_Y: usize = 43;
    const DATE_Y: usize = 56;
    const LIST_TOP: usize = 88;
    const LINE_H: usize = 8;
    const ITEM_GAP: usize = 5;
    const BOTTOM: usize = 296;
    let opened_index = (alarms::days_since_epoch(year, month, day) - start) as usize;

    for i in 0..7usize {
        let (y, m, d) = alarms::date_from_days(start + i as i64);
        let weekday = weekday_of(y, m, d);
        let x = ORIGIN_X + i * (COL_WIDTH + COL_GAP);
        let is_today = now.is_some_and(|dt| dt.year == y && dt.month == m && dt.day == d);

        if i == opened_index {
            canvas.stroke_rect(x, CARD_TOP, COL_WIDTH, CARD_HEIGHT, 2);
            canvas.fill_rect(x, CARD_TOP + CARD_HEIGHT - 4, COL_WIDTH, 4, true);
        }
        if is_today {
            canvas.fill_rect(x + COL_WIDTH - 7, CARD_TOP + 4, 3, 3, true);
        }
        let weekday_text = WEEKDAY_SHORT[weekday as usize];
        let weekday_w = Canvas::text_small_width(weekday_text);
        canvas.draw_text_small(x + (COL_WIDTH - weekday_w) / 2, WEEKDAY_Y, weekday_text);
        let date_text = d.to_string();
        let date_w = Canvas::text_prop_width(&date_text, 1);
        canvas.draw_text_prop(x + (COL_WIDTH - date_w) / 2, DATE_Y, 1, &date_text);

        let due: Vec<&str> = todos
            .iter()
            .filter(|t| !t.done)
            .filter(|t| match &t.repeat {
                Some(r) => r.fires_on(y, m, d, weekday),
                None => t
                    .due_date
                    .is_some_and(|dd| dd.year == y && dd.month == m && dd.day == d),
            })
            .map(|t| t.text.as_str())
            .collect();

        const MAX_TODO_LINES: usize = 3;
        const TEXT_INSET: usize = 8;
        let text_w = COL_WIDTH.saturating_sub(TEXT_INSET + 2);
        let mut y_cursor = LIST_TOP;
        'day: for text in due {
            let mut lines = wrap_text_small(text, text_w);
            let truncated = lines.len() > MAX_TODO_LINES;
            lines.truncate(MAX_TODO_LINES);
            canvas.fill_rect(x + 1, y_cursor + 2, 3, 3, true);
            for (line_index, line) in lines.iter().enumerate() {
                if y_cursor + 7 > BOTTOM {
                    break 'day;
                }
                let text_x = x + TEXT_INSET;
                if truncated && line_index + 1 == lines.len() {
                    let ellipsis_w = Canvas::text_small_width("...");
                    let mut end = line.len();
                    while end > 0 && Canvas::text_small_width(&line[..end]) + ellipsis_w > text_w {
                        end -= 1;
                    }
                    canvas.draw_text_small(text_x, y_cursor, &line[..end]);
                    canvas.draw_text_small(
                        text_x + Canvas::text_small_width(&line[..end]),
                        y_cursor,
                        "...",
                    );
                } else {
                    canvas.draw_text_small(text_x, y_cursor, line);
                }
                y_cursor += LINE_H;
            }
            y_cursor += ITEM_GAP;
            if y_cursor <= BOTTOM {
                canvas.fill_rect(x + TEXT_INSET, y_cursor - 2, text_w, 1, true);
            }
        }
    }
    drop(canvas);
}

fn days_in_month(year: u16, month: u8) -> u8 {
    const DAYS: [u8; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if month == 2 && is_leap(year as i64) {
        29
    } else {
        DAYS[(month - 1) as usize]
    }
}

fn weekday_of(year: u16, month: u8, day: u8) -> u8 {
    const T: [i64; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let mut y = year as i64;
    if month < 3 {
        y -= 1;
    }
    ((y + y / 4 - y / 100 + y / 400 + T[(month - 1) as usize] + day as i64) % 7) as u8
}

fn format_alarm_row(alarm: &StoredAlarm) -> String {
    let mark = if alarm.enabled { "[X]" } else { "[ ]" };
    let when = match &alarm.repeat {
        Repeat::Daily => format!("{:02}:{:02} DAILY", alarm.hour, alarm.minute),
        Repeat::Weekly { days } => {
            let weekdays: Vec<&str> = days.iter().map(|d| WEEKDAY_SHORT[*d as usize]).collect();
            format!(
                "{:02}:{:02} {}",
                alarm.hour,
                alarm.minute,
                weekdays.join(",")
            )
        }
        Repeat::Monthly { days } => {
            let list: Vec<String> = days.iter().map(|d| d.to_string()).collect();
            format!(
                "{:02}:{:02} DAY {}",
                alarm.hour,
                alarm.minute,
                list.join(",")
            )
        }
        Repeat::Once { month, day, .. } => {
            format!(
                "{:02}:{:02} {:02}/{:02}",
                alarm.hour, alarm.minute, month, day
            )
        }
    };
    let mut row = format!("{mark} {when}");
    if !alarm.label.is_empty() {
        row.push(' ');
        row.push_str(&alarm.label);
    }
    truncate_prop(&row, LIST_TEXT_MAX_WIDTH)
}

const WEEKDAY_SHORT: [&str; 7] = ["SU", "MO", "TU", "WE", "TH", "FR", "SA"];

fn render_alarm_page(board: &mut Note4Board, alarms: &[StoredAlarm], selected: usize) {
    let mut items: Vec<String> = alarms.iter().map(format_alarm_row).collect();
    items.push("+ ADD ALARM".to_string());
    let mut canvas = board.display.canvas_mut();
    draw_rows(&mut canvas, "ALARMS", &items, selected);
    footer(&mut canvas, "UP/DOWN MOVE   ENTER OK   HOLD UP/DOWN PAGE");
}

pub(crate) fn draw_alarm_list(board: &mut Note4Board, alarms: &[StoredAlarm], selected: usize) {
    render_alarm_page(board, alarms, selected);
}

pub(crate) fn draw_alarm_ringing(board: &mut Note4Board) {
    let mut canvas = board.display.canvas_mut();
    canvas.clear();
    header(&mut canvas, "ALARM");
    let alarm_w = Canvas::text_prop_width("ALARM", 4);
    canvas.draw_text_prop(200usize.saturating_sub(alarm_w / 2), 92, 4, "ALARM");
    let hint = "ENTER = DISMISS";
    let hint_w = Canvas::text_prop_width(hint, 1);
    canvas.draw_text_prop(200usize.saturating_sub(hint_w / 2), 184, 1, hint);
    footer(&mut canvas, "ENTER = DISMISS");
    drop(canvas);
}

pub(crate) fn draw_reminder(
    board: &mut Note4Board,
    kind: inkwash_logic::app::ReminderKind,
    lines: &[String],
) {
    let mut canvas = board.display.canvas_mut();
    canvas.clear();
    let title = match kind {
        inkwash_logic::app::ReminderKind::Urgent => "URGENT",
        inkwash_logic::app::ReminderKind::Todo => "TODOS DUE",
    };
    header(&mut canvas, title);

    let (max_rows, overflow_label) = match kind {
        inkwash_logic::app::ReminderKind::Urgent => (4, "MORE IN INBOX..."),
        inkwash_logic::app::ReminderKind::Todo => (7, "MORE..."),
    };
    for (index, line) in lines.iter().take(max_rows).enumerate() {
        let text = truncate_prop(line, 300);
        canvas.draw_text_prop(16, 48 + index * 24, 1, &format!("!! {text}"));
    }
    if lines.len() > max_rows {
        canvas.draw_text_prop(16, 48 + max_rows * 24, 1, overflow_label);
    }
    footer(&mut canvas, "ENTER = DISMISS");
    drop(canvas);
}

pub(crate) fn draw_ble_pairing(board: &mut Note4Board) {
    let mut canvas = board.display.canvas_mut();
    canvas.clear();
    header(&mut canvas, "BLE PAIRING");
    canvas.draw_text_prop(8, 40, 1, "CONNECTING...");
    canvas.draw_text_prop(8, 60, 1, "Service UUID:");
    canvas.draw_text_prop(8, 72, 1, "d2c25e50-");
    canvas.draw_text_prop(8, 84, 1, "5e22-48d8...");
    footer(&mut canvas, "HOLD ENTER BACK");
}

pub(crate) fn draw_number_pick(
    board: &mut Note4Board,
    stage: inkwash_logic::app::AddStage,
    value: u8,
) {
    let title = match stage {
        inkwash_logic::app::AddStage::Hour => "NEW ALARM - HOUR",
        inkwash_logic::app::AddStage::Minute => "NEW ALARM - MINUTE",
    };
    let mut canvas = board.display.canvas_mut();
    canvas.clear();
    header(&mut canvas, title);
    let label = format!("{value:02}");
    let number_width = Canvas::text_prop_width(&label, 5);
    let box_width = number_width + 64;
    let box_x = 200usize.saturating_sub(box_width / 2);
    const BOX_TOP: usize = 123;
    const BOX_H: usize = 120;
    let caption_width = Canvas::text_prop_width("CHOOSE VALUE", 1);
    canvas.draw_text_prop(
        (400usize.saturating_sub(caption_width)) / 2,
        87,
        1,
        "CHOOSE VALUE",
    );
    canvas.stroke_rect(box_x, BOX_TOP, box_width, BOX_H, 3);
    canvas.fill_rect(box_x, BOX_TOP, 7, BOX_H, true);
    let value_x = box_x + 7 + (box_width - 7 - number_width) / 2;
    let value_y = BOX_TOP + (BOX_H.saturating_sub(80)) / 2;
    canvas.draw_text_prop(value_x, value_y, 5, &label);
    footer(&mut canvas, "UP/DOWN CHANGE   ENTER OK   HOLD ENTER BACK");
}

pub(crate) fn draw_todo_list(
    board: &mut Note4Board,
    todos: &[crate::todos::Todo],
    selected: usize,
    now: Option<&DateTime>,
) {
    render_todo_page(board, todos, selected, now);
}

fn render_todo_page(
    board: &mut Note4Board,
    todos: &[crate::todos::Todo],
    selected: usize,
    now: Option<&DateTime>,
) {
    let items: Vec<String> = todos.iter().map(|t| format_todo_row(t, now)).collect();
    let mut canvas = board.display.canvas_mut();
    draw_rows(&mut canvas, "TODOS", &items, selected);
    footer(&mut canvas, "ENTER DONE   HOLD ENTER HOME");
}

fn todo_due_today(todo: &crate::todos::Todo, now: Option<&DateTime>) -> bool {
    now.is_some_and(|dt| match &todo.repeat {
        Some(r) => r.fires_on(dt.year, dt.month, dt.day, dt.weekday),
        None => todo
            .due_date
            .is_some_and(|d| d.year == dt.year && d.month == dt.month && d.day == dt.day),
    })
}

fn format_todo_row(todo: &crate::todos::Todo, now: Option<&DateTime>) -> String {
    let mark = if todo.done { "[X]" } else { "[ ]" };
    let imp = match todo.importance {
        crate::todos::Importance::Low => "",
        crate::todos::Importance::Medium => "! ",
        crate::todos::Importance::High => "!! ",
    };
    let mut row = format!("{mark} {imp}{}", todo.text);
    if !todo.done {
        if todo_due_today(todo, now) {
            row.push_str(" - DUE TODAY");
        } else if let Some(due) = todo.due_date {
            row.push_str(&format!(" - {:02}/{:02}", due.month, due.day));
        } else if let Some(Repeat::Weekly { days }) = &todo.repeat {
            let weekdays: Vec<&str> = days.iter().map(|d| WEEKDAY_SHORT[*d as usize]).collect();
            row.push_str(" - ");
            row.push_str(&weekdays.join(","));
        }
    }
    truncate_prop(&row, LIST_TEXT_MAX_WIDTH)
}

fn format_inbox_row(item: &InboxItem) -> String {
    let mark = if item.read { "• " } else { "○ " };
    let row = format!("{mark}{}", item.title);
    truncate_prop(&row, LIST_TEXT_MAX_WIDTH)
}

pub(crate) fn draw_inbox_list(board: &mut Note4Board, items: &[InboxItem], selected: usize) {
    render_inbox_page(board, items, selected);
}

fn render_inbox_page(board: &mut Note4Board, inbox: &[InboxItem], selected: usize) {
    let items: Vec<String> = inbox.iter().map(format_inbox_row).collect();
    let mut canvas = board.display.canvas_mut();
    if items.is_empty() {
        draw_rows(&mut canvas, "INBOX", &["NO MESSAGES".to_string()], selected);
    } else {
        draw_rows(&mut canvas, "INBOX", &items, selected);
    }
    footer(&mut canvas, "ENTER OPEN   HOLD UP/DOWN PAGE");
}

pub(crate) fn draw_inbox_item_detail(board: &mut Note4Board, items: &[InboxItem], selected: usize) {
    let Some(item) = items.get(selected).cloned() else {
        return;
    };
    let mut canvas = board.display.canvas_mut();
    canvas.clear();
    header(&mut canvas, "INBOX");

    let title = truncate_prop(&item.title, 340);
    let mut title_y = 40usize;
    let mut rule_y = 74usize;
    let mut body_y = 82usize;
    if Canvas::text_prop_width(&title, 2) <= 368 {
        canvas.draw_text_prop(16, title_y, 2, &title);
    } else {
        let lines = wrap_text_prop(&title, 368);
        title_y = 38;
        for (i, line) in lines.iter().take(2).enumerate() {
            canvas.draw_text_prop(16, title_y + i * 22, 1, line);
        }
        rule_y = 92;
        body_y = 100;
    }

    canvas.fill_rect(16, rule_y, 368, 1, true);
    let mut y = body_y;
    for line in wrap_text_prop(&item.body, 368) {
        if y + 16 > 282 {
            break;
        }
        canvas.draw_text_prop(16, y, 1, &line);
        y += 18;
    }
    footer(&mut canvas, "ENTER / HOLD ENTER CLOSE");
    drop(canvas);
}

pub struct NextAlarmLabel {
    pub time: String,

    pub date: Option<String>,

    pub days_left: i64,
}

pub fn next_alarm_label_from(list: &[StoredAlarm], now: &DateTime) -> Option<NextAlarmLabel> {
    alarms::next_due(list, now).map(|alarm| {
        let time = format!("{:02}:{:02}", alarm.hour, alarm.minute);
        let (date, days_left) = match &alarm.repeat {
            Repeat::Daily => (None, 0),
            Repeat::Weekly { .. } => {
                let (year, month, day, _) =
                    alarms::next_occurrence_date(&alarm.repeat, alarm.hour, alarm.minute, now);
                (
                    Some(format!("{:02}/{:02}", month, day)),
                    alarms::days_until(year, month, day, now),
                )
            }
            Repeat::Monthly { .. } => {
                let (year, month, day, _) =
                    alarms::next_occurrence_date(&alarm.repeat, alarm.hour, alarm.minute, now);
                (
                    Some(format!("{:02}/{:02}", month, day)),
                    alarms::days_until(year, month, day, now),
                )
            }
            Repeat::Once { year, month, day } => (
                Some(format!("{:02}/{:02}", month, day)),
                alarms::days_until(*year, *month, *day, now),
            ),
        };
        NextAlarmLabel {
            time,
            date,
            days_left,
        }
    })
}

pub struct TodoSummary {
    pub pending: usize,
    pub due_today: usize,
}

pub fn todo_summary_from(list: &[crate::todos::Todo], now: Option<&DateTime>) -> TodoSummary {
    let mut summary = TodoSummary {
        pending: 0,
        due_today: 0,
    };
    for todo in list {
        if todo.done {
            continue;
        }
        summary.pending += 1;
        if todo_due_today(todo, now) {
            summary.due_today += 1;
        }
    }
    summary
}
