//! Menu/Calendar/Alarms/Todos surfaces. The state machine owns navigation and
//! the renderer receives already-collected AppState facts for each surface.

use crate::alarms::{self, Repeat, StoredAlarm};
use crate::board::Note4Board;
use crate::canvas::Canvas;
use crate::ctx::DeviceContext;
use crate::display::Rect;
use crate::inbox::InboxItem;
use crate::rtc::{is_leap, DateTime};
use crate::todos::Importance;
use crate::ui::{draw_rows, footer, header};

/// Text column width for list rows: rows start at `ui`'s `LIST_TEXT_X` (50)
/// and the selection box runs to x=384, so 334px of text fits. With CJK
/// glyphs now in the 8x16 path a fixed 34-char cap overflowed badly, so
/// row text is truncated by *measured width* instead of char count.
pub const LIST_TEXT_MAX_WIDTH: usize = 334;

/// Truncates `text` to fit `max_width` at scale 1 by measured glyph width
/// (CJK cells are 17px, ASCII is proportional), appending "…" when cut.
/// Never splits a UTF-8 codepoint (iterates chars).
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

/// GO TO navigation bar geometry: a left-hand panel overlaid on the live
/// page (which stays visible to its right) rather than a full screen.
/// `refresh_partial` updates only this rect, so the underlying page's
/// pixels stay put while the bar moves. Width 176 runs the bar's right edge
/// exactly to the home screen's left card border (x=192), so the covered
/// content ends at a clean boundary instead of leaving an orphaned sliver
/// of card outline.
pub(crate) const NAV_BAR_RECT: Rect = Rect {
    x: 16,
    y: 34,
    width: 176,
    height: 266,
};
const NAV_BAR_ROW_H: usize = 33;
/// Destinations in row order; `selected` indexes straight into this.
/// Five pages (HOME/CALENDAR/INBOX/ALARMS/TODOS) plus SETTINGS - six rows
/// at 33px each (33*6=198) stays inside the bar's 266px height.
const NAV_DESTINATIONS: [&str; 6] = ["HOME", "CALENDAR", "INBOX", "ALARMS", "TODOS", "SETTINGS"];

/// Draws the left navigation bar on top of whatever the canvas currently
/// holds - it deliberately does NOT clear the screen, so the current page
/// stays visible to the right of the bar and reads as the overlay's
/// background context. `selected` gets the same stroke + left-accent-bar
/// chrome every other row in the app uses; since the bar opens pre-selected
/// on the current page, that highlighted row doubles as the "you are here"
/// marker.
pub(crate) fn draw_navigation_bar(canvas: &mut Canvas, selected: usize) {
    // Solid white fill hides whatever page content sits underneath the bar
    // cleanly (no half-covered text), which is what makes it read as an
    // overlay instead of a chopped-up screen.
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
    // A thick vertical rule down the bar's right edge separates the drawer
    // from the underlying page content it overlays - the same 3px weight
    // the accent bars use, so the drawer reads as a deliberate panel edge
    // rather than a floating list.
    let rule_x = NAV_BAR_RECT.x as usize + NAV_BAR_RECT.width as usize - 3;
    canvas.fill_rect(
        rule_x,
        NAV_BAR_RECT.y as usize,
        3,
        NAV_BAR_RECT.height as usize,
        true,
    );
}

/// The Settings list rows, in the same order the state machine's
/// `SETTINGS_ROW_COUNT` / `Screen::Settings { selected }` indexes.
pub const SETTINGS_ROWS: [&str; 4] = ["SYNC NOW", "SYNC INTERVAL", "BLE PAIRING", "SLEEP"];

/// Draws the Settings list screen (Stage 4, slice 2): the state machine
/// owns the row selection and the executor draws the rows from
/// `RenderView::Settings { selected }`. Rows use the same chrome as every
/// other list in the app (`ui::draw_rows`), so the SM-drawn Settings screen
/// is pixel-consistent with the legacy list screens.
pub(crate) fn draw_settings(canvas: &mut Canvas, selected: usize) {
    let items: Vec<String> = SETTINGS_ROWS.iter().map(|s| s.to_string()).collect();
    draw_rows(canvas, "SETTINGS", &items, selected);
    footer(canvas, "UP/DOWN MOVE   ENTER OK   HOLD ENTER BACK");
}

/// The SYNC INTERVAL options in row order (mirrors logic
/// SYNC_INTERVAL_MINUTES).
const SYNC_INTERVAL_OPTIONS: [&str; 5] = ["1 MIN", "5 MIN", "10 MIN", "30 MIN", "60 MIN"];

/// Draws the state-machine SYNC INTERVAL picker (Stage 4, slice 11): the
/// five fixed interval options with the SM row cursor. Same layout the
/// legacy sync_interval_screen list used.
pub(crate) fn draw_sync_interval(canvas: &mut Canvas, selected: usize) {
    let items: Vec<String> = SYNC_INTERVAL_OPTIONS
        .iter()
        .map(|s| s.to_string())
        .collect();
    draw_rows(canvas, "SYNC INTERVAL", &items, selected);
    footer(canvas, "UP/DOWN MOVE   ENTER OK   HOLD ENTER BACK");
}

/// Draws the state-machine Settings screen (Stage 4, slice 2).
/// Draws the state-machine Calendar grid screen (Stage 4, slice 6): the
/// read-only current-month grid with day marks read from the todo store and
/// the day cursor drawn from state. The SM owns the cursor (UP/DOWN), ENTER
/// opens a day's week view through a deferred render. The SM-drawn grid is
/// pixel-consistent. `selected_day` is clamped to the month here too (belt
/// and braces; the SM clamps on transitions).
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
        // Day markers for the visible month: whether a todo is due that day
        // (repeat schedule, or its single due date), carrying importance so
        // the marker can be sized by it. Alarms don't get a mark here - the
        // month grid is a todo-due overview; ENTER on a day opens the week
        // view for the specifics, and alarms already have their own page.
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

/// Per-day cell marker for the month grid: the importance of a todo due
/// that day, if any. Alarms don't get a month-grid mark - see the ENTER ->
/// week view flow below for schedule detail.
#[derive(Clone, Copy, Default)]
struct DayMark {
    todo: Option<Importance>,
}

/// Read-only current-month grid. No month navigation in v1 - the device
/// always shows "now". UP/DOWN moves `selected_day` (a linear cursor over
/// 1..=days_in_month, wrapping row/col); ENTER on it opens that day's week
/// view (`week_view`) - the grid itself only has room for a due/not-due
/// dot, so that's where "what exactly is due Wednesday" gets answered.
/// `today` gets a thin underline so it stays visible even when the cursor
/// (the box + accent bar) has moved off it.
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
    // Rows must fit all 6 possible week rows, each with clearance below
    // the day number for its todo dot, within the 300px display.
    const ROW_HEIGHT: usize = 32;
    const ORIGIN_X: usize = 18;
    const ORIGIN_Y: usize = 75;
    // The todo dot sits below the day number's 16px glyph height, not
    // overlapping it.
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
        // The cursor gets the same box + left accent bar every selection
        // in the app uses (nav drawer, list rows) - "you are here, ENTER
        // acts on it".
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

/// Greedy word-wrap measured with the tiny 5x7 font
/// (`Canvas::text_small_width`) - the week-view columns are too
/// narrow to waste 16px-tall type on. A single word longer than
/// `max_width` on its own gets hard character-split across lines
/// instead of overflowing into the next column.
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
                // Step back to the previous char boundary (CJK is multi-byte).
                split = remaining[..split]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
            }
            if split == 0 {
                // Even one char alone is too wide; emit a single full char
                // so the loop always makes progress.
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

/// Greedy word-wrap measured with the regular 8x16 proportional font
/// (`Canvas::text_prop_width`) - for detail pages that render body text
/// at scale 1. Same shape as [`wrap_text_small`]; a single word longer
/// than `max_width` is hard character-split instead of overflowing.
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
                // Step back to the previous char boundary (CJK is multi-byte).
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

/// Draws the state-machine WeekView screen (Stage 4, slice 6): the Sun-Sat
/// week containing `day` - one column per day with each day's open todo
/// text read from the todo store. The SM owns open/close (ENTER on a
/// Calendar day opens it; any button closes back to the grid); the executor
/// draws from state. Same layout the legacy week_view used, so the two
/// paths are pixel-consistent.
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

    // Seven compact day cards keep the dense week scannable without the
    // spreadsheet-like full-height grid. The opened day gets the same
    // outline/accent treatment as selected rows elsewhere in the UI; today
    // remains a separate, small marker in the card's top-right corner.
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

        // A square bullet creates a stable reading edge. Short inset rules
        // separate items while leaving white gutters between day columns.
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

/// Sakamoto's algorithm; 0=Sunday, matching the `LABELS` order above. This
/// is independent of `DateTime::weekday`'s own (unrelated) convention -
/// nothing here reads that field.
fn weekday_of(year: u16, month: u8, day: u8) -> u8 {
    const T: [i64; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let mut y = year as i64;
    if month < 3 {
        y -= 1;
    }
    ((y + y / 4 - y / 100 + y / 400 + T[(month - 1) as usize] + day as i64) % 7) as u8
}

/// `[X]`/`[ ]` + time + compact repeat summary, then the label (when set)
/// appended and truncated to a single row's width - the alarm page now
/// shows *when it repeats* and *what it's called*, not just the time.
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

/// Draws the state-machine AlarmList screen (Stage 4, slice 3): stored
/// alarm rows (from the store; the SM owns toggling, the executor renders)
/// plus the trailing "+ ADD ALARM" row. Same layout the legacy alarm page
/// used, so the SM-drawn list is pixel-consistent.
pub(crate) fn draw_alarm_list(board: &mut Note4Board, alarms: &[StoredAlarm], selected: usize) {
    render_alarm_page(board, alarms, selected);
}

/// Draws the state-machine ADD-ALARM number picker (Stage 4, slice 9): the
/// big stepped digit for the active stage (hour or minute). The SM owns the
/// value + stage; the executor draws from state using the same visual
/// language the legacy pick_number used (title header, centered box + large
/// scale-5 digits), so the two paths look identical.
/// Draws the state-machine BLE pairing session screen (Stage 4): the
/// pairing instructions canvas. The SM owns the lifecycle (events drive
/// the phase; any button exits back to Settings); this draws the same
/// instructions the legacy ble_pairing_screen showed while its radio
/// wedge was on screen.
/// Draws the non-blocking alarm ring frame (Stage 5/6). The SM owns the
/// ring lifecycle (Firing state, ENTER dismiss, ring-deadline timeout); this
/// only paints the ALARM canvas the legacy ring_screen used to draw over the
/// page. Replaces the direct `refresh_full_best_effort` call the legacy
/// blocking ring made - the RenderPlan-driven executor submits this frame.
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

/// Draws the full-screen reminder overlay (urgent or todo) from the SM's
/// final visible lines. The RenderPlan-driven executor submits this frame;
/// the function never refreshes the panel itself (the legacy reminder
/// loops' direct refresh_full_best_effort is gone).
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
    // At most 7 rows (todo) / 4 rows (urgent) fit before the hint; the fact
    // layer already truncated to the visible set, so draw what we got with a
    // "MORE..." tail when there are more lines than fit.
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

/// Draws the state-machine TodoList screen (Stage 4, slice 4): stored todo
/// rows rendered from the store (with the due-today / repeat markers). The
/// SM owns the done-toggle and navigation; importance is server-authored and
/// displayed as part of each row. The executor renders the screen.
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

/// Whether `todo` (repeating or one-off) is due on `now`'s date.
fn todo_due_today(todo: &crate::todos::Todo, now: Option<&DateTime>) -> bool {
    now.is_some_and(|dt| match &todo.repeat {
        Some(r) => r.fires_on(dt.year, dt.month, dt.day, dt.weekday),
        None => todo
            .due_date
            .is_some_and(|d| d.year == dt.year && d.month == dt.month && d.day == dt.day),
    })
}

/// `[X]`/`[ ]` plus a `!!`/`!` importance suffix (low gets none), then the
/// text; a trailing `- MM/DD`/`- DUE TODAY` marks the due date / repeat so
/// the open-items page also shows *when* something needs doing, not just
/// what.
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

/// Draws the state-machine InboxList screen (Stage 4, slice 5): stored
/// inbox rows (read/unread markers) rendered from the store; ENTER opens an
/// item's detail through a deferred legacy wedge. Same layout the legacy
/// inbox page used.
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

/// Draws the state-machine InboxItem screen (Stage 4, slice 7): the opened
/// item's title + body from the store (carries only the index). The SM owns
/// open/close and the optimistic read-mark; the executor draws from state.
pub(crate) fn draw_inbox_item_detail(board: &mut Note4Board, items: &[InboxItem], selected: usize) {
    let Some(item) = items.get(selected).cloned() else {
        return;
    };
    let mut canvas = board.display.canvas_mut();
    canvas.clear();
    header(&mut canvas, "INBOX");
    // Title: scale 2 when it fits, otherwise wrap to up to two scale-1
    // lines (CJK cells are full-width, so a char-count cap overflowed
    // badly). The hairline rule below always clears the title's ink.
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
    // Hairline rule under the title separates the headline from the body,
    // matching the header/card title-band language used everywhere else.
    // Kept clear of the title's full glyph box (incl. CJK cells) so the
    // line never cuts through descenders.
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

/// Next enabled alarm's summary for the Home screen card: the time plus
/// enough schedule detail to say *when* without opening the alarm page -
/// next firing date, and how many days out it is.
pub struct NextAlarmLabel {
    pub time: String,
    /// Next firing date as `MM/DD` (only when the schedule has a specific
    /// date to name - Weekly/Monthly/Once).
    pub date: Option<String>,
    /// Whole days from today until the next firing (0 = today).
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

/// Aggregated todo stats for the Home screen's OPEN TODOS card: how many
/// are still open, and how many of those are due today.
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
