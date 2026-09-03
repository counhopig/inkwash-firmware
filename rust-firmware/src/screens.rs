//! Menu/Calendar/Alarms/Todos screens, entered from the Home screen's
//! long UP/DOWN navigation drawer (see `main.rs`). Each screen is a
//! self-contained blocking function.

use crate::alarms::{self, AlarmStore, Repeat, StoredAlarm};
use crate::board::Note4Board;
use crate::canvas::Canvas;
use crate::ctx::DeviceContext;
use crate::display::Rect;
use crate::inbox::{InboxItem, InboxStore};
use crate::rtc::{is_leap, DateTime};
use crate::sync;
use crate::sync_task::OpSource;
use crate::todos::{Importance, TodoStore};
use crate::ui::{
    draw_rows, footer, header, pick_from_list, pick_number, poll_nav, show_message, tick, Nav,
    PickResult,
};

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

/// Entry point from Home's ENTER short-press: shows the top-level menu and
/// recurses into whichever screen the user picks, returning once they back
/// all the way out to Home. Always leaves the caller (Home) to redraw its
/// own full screen afterwards - none of these screens know how to render
/// the home screen themselves.
pub fn open_menu(ctx: &mut DeviceContext, now: Option<&DateTime>) {
    let items = [
        "SYNC NOW".to_string(),
        "SYNC INTERVAL".to_string(),
        "BLE PAIRING".to_string(),
        "SLEEP".to_string(),
    ];
    loop {
        // An alarm may have interrupted a nested screen; unwind Settings
        // to the unified router.
        if ctx.alarm_exit_pending() {
            return;
        }
        let want_nav = match pick_from_list(
            ctx,
            now,
            "SETTINGS",
            &items,
            "UP/DOWN MOVE   ENTER OK   HOLD ENTER BACK",
            0,
        ) {
            PickResult::Selected(0) => {
                sync_now_screen(ctx, now);
                if ctx.alarm_exit_pending() {
                    return;
                }
                false
            }
            PickResult::Selected(1) => {
                let nav = sync_interval_screen(ctx, now);
                if ctx.alarm_exit_pending() {
                    return;
                }
                nav
            }
            PickResult::Selected(2) => {
                ble_pairing_screen(ctx, now);
                if ctx.alarm_exit_pending() {
                    return;
                }
                false
            }
            PickResult::Selected(3) => {
                show_message(
                    ctx.board,
                    "SLEEP",
                    &["GOING TO SLEEP"],
                    std::time::Duration::from_millis(500),
                );
                let maintenance_wake = now.and_then(|dt| {
                    ctx.alarm_store
                        .load()
                        .ok()
                        .and_then(|alarms| alarms::maintenance_wakeup_delay(&alarms, dt))
                });
                crate::power::enter_deep_sleep_with_wakeups(maintenance_wake);
            }
            PickResult::Cancelled => return,
            PickResult::OpenNav => true,
            PickResult::AlarmInterrupted => {
                // Alarm rang over Settings: unwind to the unified router.
                ctx.alarm_poll.mark_exit();
                return;
            }
            _ => false,
        };
        // Long UP/DOWN inside Settings opens the GO TO drawer, matching
        // every other page (from here you can reach Todos/Calendar/etc.).
        if want_nav {
            open_navigation(ctx, now);
            return;
        }
    }
}

/// Pick the automatic sync interval from preset options (1/5/10/30/60
/// minutes). The device re-syncs with the configured server every this many
/// minutes through the shared scheduler in `DeviceContext`. Returns
/// `true` when the user long-pressed UP/DOWN, so the caller can open the
/// navigation drawer (this screen lacks the full context to do so itself).
fn sync_interval_screen(ctx: &mut DeviceContext, now: Option<&DateTime>) -> bool {
    const OPTIONS: [(&str, u16); 5] = [
        ("1 MIN", 1),
        ("5 MIN", 5),
        ("10 MIN", 10),
        ("30 MIN", 30),
        ("60 MIN", 60),
    ];
    let items: Vec<String> = OPTIONS.iter().map(|(label, _)| label.to_string()).collect();
    let outcome = pick_from_list(
        ctx,
        now,
        "SYNC INTERVAL",
        &items,
        "UP/DOWN MOVE   ENTER OK   HOLD ENTER BACK",
        0,
    );
    let PickResult::Selected(index) = outcome else {
        if outcome == PickResult::AlarmInterrupted {
            ctx.alarm_poll.mark_exit();
        }
        // Cancelled (hold ENTER) or OpenNav (long UP/DOWN) - surface the
        // nav request upward so Settings can open the GO TO drawer.
        return matches!(outcome, PickResult::OpenNav);
    };
    let (_, minutes) = OPTIONS[index];
    match ctx.counters.set_sync_interval_minutes(minutes) {
        Ok(()) => {
            log::info!("Sync interval set to {minutes} min");
            show_message(
                ctx.board,
                "SYNC INTERVAL",
                &[&format!("{minutes} MIN")],
                std::time::Duration::from_secs(1),
            );
        }
        Err(err) => log::warn!("Failed to save sync interval: {err}"),
    }
    false
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Home,
    Calendar,
    Alarms,
    Todos,
    Inbox,
}

/// GO TO navigation bar geometry: a left-hand panel overlaid on the live
/// page (which stays visible to its right) rather than a full screen.
/// `refresh_partial` updates only this rect, so the underlying page's
/// pixels stay put while the bar moves. Width 176 runs the bar's right edge
/// exactly to the home screen's left card border (x=192), so the covered
/// content ends at a clean boundary instead of leaving an orphaned sliver
/// of card outline.
const NAV_BAR_RECT: Rect = Rect {
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
fn draw_navigation_bar(canvas: &mut Canvas, selected: usize) {
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

/// Opens the GO TO left-hand bar, pre-selected on wherever you already are
/// (`current`) instead of always resetting to HOME - so pressing ENTER with
/// no further input just closes the bar back onto the same page, and the
/// highlighted row doubles as the "you are here" indicator. Unlike a
/// full-screen list, the bar is drawn over the live page (see
/// `draw_navigation_bar`) so you keep your place on screen while browsing
/// destinations. Returns the chosen index, or `None` on hold-to-cancel.
fn pick_navigation(
    ctx: &mut DeviceContext,
    now: Option<&DateTime>,
    current: Page,
) -> Option<usize> {
    let current_index = match current {
        Page::Home => 0,
        Page::Calendar => 1,
        Page::Inbox => 2,
        Page::Alarms => 3,
        Page::Todos => 4,
    };
    let mut selected = current_index;
    let mut needs_redraw = true;
    loop {
        match ctx.poll_background(now) {
            crate::ctx::BackgroundOutcome::AlarmHandled => {
                ctx.alarm_poll.mark_exit();
                return None;
            }
            crate::ctx::BackgroundOutcome::VisibleChanged => needs_redraw = true,
            crate::ctx::BackgroundOutcome::NoChange => {}
        }
        if needs_redraw {
            let mut canvas = ctx.board.display.canvas_mut();
            draw_navigation_bar(&mut canvas, selected);
            drop(canvas);
            ctx.board.display.refresh_partial_best_effort(NAV_BAR_RECT);
            needs_redraw = false;
        }
        match poll_nav(ctx.board) {
            Nav::Up => {
                selected = if selected == 0 {
                    NAV_DESTINATIONS.len() - 1
                } else {
                    selected - 1
                };
                needs_redraw = true;
            }
            Nav::Down => {
                selected = (selected + 1) % NAV_DESTINATIONS.len();
                needs_redraw = true;
            }
            Nav::Enter => return Some(selected),
            Nav::Cancel => return None,
            Nav::PageUp => {
                selected = 0;
                needs_redraw = true;
            }
            Nav::PageDown => {
                selected = NAV_DESTINATIONS.len() - 1;
                needs_redraw = true;
            }
            Nav::None => {}
        }
        tick();
    }
}

/// Opens the global navigation directory. Both long UP and long DOWN enter
/// this directory; short UP/DOWN selects a destination and ENTER opens it.
pub fn open_navigation(ctx: &mut DeviceContext, now: Option<&DateTime>) {
    loop {
        let Some(selected) = pick_navigation(ctx, now, Page::Home) else {
            // Cancel or alarm - either way unwind; the alarm flag tells
            // main why.
            return;
        };
        match selected {
            0 => return,
            1..=4 => {
                let page = match selected {
                    1 => Page::Calendar,
                    2 => Page::Inbox,
                    3 => Page::Alarms,
                    _ => Page::Todos,
                };
                browse_page(ctx, page, now);
                return;
            }
            5 => {
                open_menu(ctx, now);
                // An alarm inside Settings (or a nested screen) must
                // unwind open_navigation too.
                if ctx.alarm_exit_pending() {
                    return;
                }
            }
            _ => {}
        }
    }
}

/// Runs one peer content page. Long UP/DOWN opens the navigation overlay;
/// cancelling that overlay restores this page.
fn browse_page(ctx: &mut DeviceContext, mut page: Page, now: Option<&DateTime>) {
    let mut live_now = now.copied();
    let mut rtc_poll_ticks = 0u8;
    let mut alarm_selected = 0usize;
    let mut todo_selected = 0usize;
    let mut inbox_selected = 0usize;
    // Calendar day cursor - starts on today, moves with UP/DOWN, ENTER
    // opens that day's week view. Only meaningful while `now` is known.
    let mut cal_selected_day: u8 = live_now.map(|dt| dt.day).unwrap_or(1);
    let mut needs_redraw = true;
    let mut first_draw = true;
    loop {
        if ctx.poll_alarm_snapshot() {
            // Alarm fired over the page: exit to the main loop, which
            // re-renders per the AppRunner's current Screen.
            return;
        }
        match ctx.poll_background(live_now.as_ref()) {
            crate::ctx::BackgroundOutcome::AlarmHandled => return,
            crate::ctx::BackgroundOutcome::VisibleChanged => {
                if let Ok(fresh) = ctx.rtc.read_time() {
                    live_now = Some(fresh);
                }
                needs_redraw = true;
            }
            crate::ctx::BackgroundOutcome::NoChange => {}
        }
        if needs_redraw {
            match page {
                Page::Home => {
                    let now = live_now.as_ref();
                    let next_alarm = now.and_then(|dt| next_alarm_label(ctx.alarm_store, dt));
                    let todo_summary = todo_summary(ctx.todo_store, now);
                    let unread_inbox = ctx.inbox_store.unread_count().unwrap_or(0);
                    let wifi_configured = ctx
                        .counters
                        .wifi_creds()
                        .map(|creds| creds.is_some())
                        .unwrap_or(false);
                    let battery_percent = ctx.board.battery_percent();
                    let charge = ctx.board.charge_snapshot();
                    ctx.board.display.render_home(
                        now,
                        next_alarm.as_ref().map(|label| label.time.as_str()),
                        next_alarm.as_ref().and_then(|label| label.date.as_deref()),
                        next_alarm.as_ref().map(|label| label.days_left),
                        todo_summary.pending,
                        todo_summary.due_today,
                        unread_inbox,
                        wifi_configured,
                        battery_percent,
                        charge,
                    );
                }
                Page::Calendar => {
                    let now = live_now.as_ref();
                    let mut canvas = ctx.board.display.canvas_mut();
                    canvas.clear();
                    header(&mut canvas, "CALENDAR");
                    if let Some(dt) = now {
                        let todos = ctx.todo_store.load().unwrap_or_default();
                        // Day markers for the visible month: whether a todo
                        // is due that day (repeat schedule, or its single
                        // due date), carrying importance so the marker can
                        // be sized by it. Alarms don't get a mark here - the
                        // month grid is a todo-due overview; ENTER on a day
                        // opens the week view for the specifics, and alarms
                        // already have their own page.
                        let mut marks = [DayMark::default(); 32];
                        let days_in_month = days_in_month(dt.year, dt.month);
                        for todo in todos.iter() {
                            for day in 1..=days_in_month {
                                let fires = match &todo.repeat {
                                    Some(r) => r.fires_on(
                                        dt.year,
                                        dt.month,
                                        day,
                                        weekday_of(dt.year, dt.month, day),
                                    ),
                                    None => todo.due_date.is_some_and(|d| {
                                        d.year == dt.year && d.month == dt.month && d.day == day
                                    }),
                                };
                                if fires {
                                    marks[day as usize].todo = Some(todo.importance);
                                }
                            }
                        }
                        cal_selected_day = cal_selected_day.min(days_in_month).max(1);
                        draw_month_grid(
                            &mut canvas,
                            dt.year,
                            dt.month,
                            now,
                            cal_selected_day,
                            &marks,
                        );
                    }
                    footer(
                        &mut canvas,
                        "UP/DOWN MOVE   ENTER WEEK VIEW   HOLD UP/DOWN SWITCH PAGE",
                    );
                    drop(canvas);
                }
                Page::Alarms => render_alarm_page(ctx.board, ctx.alarm_store, alarm_selected),
                Page::Todos => {
                    render_todo_page(ctx.board, ctx.todo_store, todo_selected, live_now.as_ref())
                }
                Page::Inbox => render_inbox_page(ctx.board, ctx.inbox_store, inbox_selected),
            }
            if first_draw {
                ctx.board.display.refresh_full_best_effort();
                first_draw = false;
            } else {
                ctx.board.display.refresh_partial_best_effort(Rect {
                    x: 0,
                    y: 0,
                    width: 400,
                    height: 300,
                });
            }
            needs_redraw = false;
        }

        match poll_nav(ctx.board) {
            Nav::PageUp | Nav::PageDown => {
                match pick_navigation(ctx, live_now.as_ref(), page) {
                    Some(0) => return,
                    Some(1) => page = Page::Calendar,
                    Some(2) => page = Page::Inbox,
                    Some(3) => page = Page::Alarms,
                    Some(4) => page = Page::Todos,
                    Some(5) => {
                        open_menu(ctx, live_now.as_ref());
                        // An alarm inside Settings (or a nested screen
                        // reached from it) must unwind the whole stack.
                        if ctx.alarm_exit_pending() {
                            return;
                        }
                    }
                    // Alarm (or cancel): pick_navigation sets the flag on
                    // alarm and returns None; exit instead of resuming the
                    // page.
                    Some(_) | None => {
                        if ctx.alarm_exit_pending() {
                            return;
                        }
                    }
                }
                needs_redraw = true;
            }
            Nav::Cancel => {
                if page == Page::Todos {
                    // Long ENTER cycles a todo's importance (Low -> Medium
                    // -> High) instead of leaving the page - long UP/DOWN
                    // opens the navigation drawer, which is the way out.
                    cycle_todo_importance(ctx.todo_store, todo_selected);
                    needs_redraw = true;
                } else if page == Page::Home {
                    return;
                } else {
                    page = Page::Home;
                    needs_redraw = true;
                }
            }
            Nav::Up => match page {
                Page::Alarms => {
                    alarm_selected = alarm_selected.saturating_sub(1);
                    needs_redraw = true;
                }
                Page::Todos => {
                    todo_selected = todo_selected.saturating_sub(1);
                    needs_redraw = true;
                }
                Page::Inbox => {
                    inbox_selected = inbox_selected.saturating_sub(1);
                    needs_redraw = true;
                }
                Page::Calendar => {
                    cal_selected_day = cal_selected_day.saturating_sub(1).max(1);
                    needs_redraw = true;
                }
                _ => {}
            },
            Nav::Down => match page {
                Page::Alarms => {
                    let len = ctx.alarm_store.load().map(|v| v.len()).unwrap_or(0) + 1;
                    alarm_selected = (alarm_selected + 1).min(len - 1);
                    needs_redraw = true;
                }
                Page::Todos => {
                    let len = ctx.todo_store.load().map(|v| v.len()).unwrap_or(0);
                    if len > 0 {
                        todo_selected = (todo_selected + 1).min(len - 1);
                        needs_redraw = true;
                    }
                }
                Page::Inbox => {
                    let len = ctx.inbox_store.load().map(|v| v.len()).unwrap_or(0);
                    if len > 0 {
                        inbox_selected = (inbox_selected + 1).min(len - 1);
                        needs_redraw = true;
                    }
                }
                Page::Calendar => {
                    if let Some(dt) = live_now.as_ref() {
                        let dim = days_in_month(dt.year, dt.month);
                        cal_selected_day = (cal_selected_day + 1).min(dim);
                        needs_redraw = true;
                    }
                }
                _ => {}
            },
            Nav::Enter => {
                match page {
                    Page::Home => {}
                    Page::Alarms => {
                        activate_alarm_row(ctx, live_now.as_ref(), alarm_selected);
                        if ctx.alarm_exit_pending() {
                            return;
                        }
                    }
                    Page::Todos => activate_todo_row(ctx.todo_store, todo_selected),
                    Page::Inbox => {
                        open_inbox_item(ctx, live_now.as_ref(), inbox_selected);
                        if ctx.alarm_exit_pending() {
                            return;
                        }
                    }
                    Page::Calendar => {
                        if let Some(dt) = live_now.as_ref() {
                            week_view(ctx, dt.year, dt.month, cal_selected_day, live_now.as_ref());
                            if ctx.alarm_exit_pending() {
                                return;
                            }
                        }
                    }
                }
                needs_redraw = true;
            }
            Nav::None => {}
        }
        rtc_poll_ticks = rtc_poll_ticks.saturating_add(1);
        if rtc_poll_ticks >= 50 {
            rtc_poll_ticks = 0;
            match ctx.rtc.read_time() {
                Ok(fresh) => {
                    let visible_time_changed = live_now.as_ref().is_none_or(|old| {
                        old.minute != fresh.minute
                            || old.hour != fresh.hour
                            || old.day != fresh.day
                            || old.month != fresh.month
                            || old.year != fresh.year
                    });
                    live_now = Some(fresh);
                    if visible_time_changed
                        && matches!(page, Page::Home | Page::Calendar | Page::Todos)
                    {
                        needs_redraw = true;
                    }
                }
                Err(err) => log::warn!("RTC read failed while browsing: {err}"),
            }
        }
        tick();
    }
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

/// Opens a read-only week view for the week (Sun-Sat) containing
/// `year`/`month`/`day`: one column per day, matching the month grid's own
/// column rhythm - a week is 7 days side by side, not a 7-row list. Each
/// column carries its weekday/date and a word-wrapped stack of that day's
/// open todo text - the detail the month grid's single dot can't show.
/// `now` gets the same thin underline the month grid uses for today,
/// independent of which day (`day`, the cursor position when ENTER was
/// pressed) the view was opened for. Any button closes it - read-only,
/// there's nothing further to drill into.
fn week_view(ctx: &mut DeviceContext, year: u16, month: u8, day: u8, now: Option<&DateTime>) {
    let todos = ctx.todo_store.load().unwrap_or_default();
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

    ctx.board.display.refresh_full_best_effort();
    loop {
        // An alarm unwinds to the unified router; a visible background
        // change (e.g. a full-screen reminder just covered this view)
        // returns to the caller, which redraws the page.
        match ctx.poll_background(now) {
            crate::ctx::BackgroundOutcome::AlarmHandled => {
                ctx.alarm_poll.mark_exit();
                return;
            }
            crate::ctx::BackgroundOutcome::VisibleChanged => return,
            crate::ctx::BackgroundOutcome::NoChange => {}
        }
        match poll_nav(ctx.board) {
            Nav::None => {}
            _ => return,
        }
        tick();
    }
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

fn render_alarm_page(board: &mut Note4Board, store: &AlarmStore, selected: usize) {
    let mut items: Vec<String> = store
        .load()
        .unwrap_or_default()
        .iter()
        .map(format_alarm_row)
        .collect();
    items.push("+ ADD ALARM".to_string());
    let mut canvas = board.display.canvas_mut();
    draw_rows(&mut canvas, "ALARMS", &items, selected);
    footer(&mut canvas, "UP/DOWN MOVE   ENTER OK   HOLD UP/DOWN PAGE");
}

fn activate_alarm_row(ctx: &mut DeviceContext, now: Option<&DateTime>, selected: usize) {
    let mut list = ctx.alarm_store.load().unwrap_or_default();
    let mut toggled_id = None;
    if selected >= list.len() {
        match add_alarm_screen(ctx, now, &list) {
            AlarmEditOutcome::Added(alarm) => list.push(alarm),
            AlarmEditOutcome::Cancelled => return,
            AlarmEditOutcome::AlarmInterrupted => {
                // Alarm rang over the editor: unwind through the alarm
                // page to the unified router / main loop.
                ctx.alarm_poll.mark_exit();
                return;
            }
        }
    } else {
        list[selected].enabled = !list[selected].enabled;
        toggled_id = Some(list[selected].id);
    }
    if let Err(err) = ctx.alarm_store.save(&list) {
        log::warn!("Failed to save alarms: {err}");
        return;
    }
    if let Some(id) = toggled_id {
        if let Err(err) = ctx.alarm_store.mark_dirty(id) {
            log::warn!("Alarm saved but failed to mark it dirty: {err}");
        }
    }
    if let Some(dt) = now {
        if let Err(err) = alarms::program_hardware_alarm_via(ctx.rtc, &list, dt) {
            log::warn!("Failed to reprogram hardware alarm: {err}");
        }
    }
}

fn render_todo_page(
    board: &mut Note4Board,
    store: &TodoStore,
    selected: usize,
    now: Option<&DateTime>,
) {
    let items: Vec<String> = store
        .load()
        .unwrap_or_default()
        .iter()
        .map(|t| format_todo_row(t, now))
        .collect();
    let mut canvas = board.display.canvas_mut();
    draw_rows(&mut canvas, "TODOS", &items, selected);
    footer(&mut canvas, "ENTER DONE   HOLD ENTER IMPORTANCE");
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

fn activate_todo_row(store: &TodoStore, selected: usize) {
    let mut list = store.load().unwrap_or_default();
    let Some(todo) = list.get_mut(selected) else {
        return;
    };
    todo.done = !todo.done;
    let id = todo.id;
    if let Err(err) = store.save(&list) {
        log::warn!("Failed to save todos: {err}");
        return;
    }
    if let Err(err) = store.mark_dirty(id) {
        log::warn!("Todo saved but failed to mark it dirty: {err}");
    }
}

/// Long-ENTER action: cycle the selected todo's importance
/// Low -> Medium -> High -> Low and persist. The device can't edit text,
/// but importance is cheap to express with the three-button set.
fn cycle_todo_importance(store: &TodoStore, selected: usize) {
    let mut list = store.load().unwrap_or_default();
    let Some(todo) = list.get_mut(selected) else {
        return;
    };
    todo.importance = match todo.importance {
        crate::todos::Importance::Low => crate::todos::Importance::Medium,
        crate::todos::Importance::Medium => crate::todos::Importance::High,
        crate::todos::Importance::High => crate::todos::Importance::Low,
    };
    log::info!("Todo id={} importance now {:?}", todo.id, todo.importance);
    let id = todo.id;
    if let Err(err) = store.save(&list) {
        log::warn!("Failed to save todos: {err}");
        return;
    }
    if let Err(err) = store.mark_dirty(id) {
        log::warn!("Todo saved but failed to mark it dirty: {err}");
    }
}

fn format_inbox_row(item: &InboxItem) -> String {
    let mark = if item.read { "• " } else { "○ " };
    let row = format!("{mark}{}", item.title);
    truncate_prop(&row, LIST_TEXT_MAX_WIDTH)
}

fn render_inbox_page(board: &mut Note4Board, store: &InboxStore, selected: usize) {
    let items: Vec<String> = store
        .load()
        .unwrap_or_default()
        .iter()
        .map(format_inbox_row)
        .collect();
    let mut canvas = board.display.canvas_mut();
    if items.is_empty() {
        draw_rows(&mut canvas, "INBOX", &["NO MESSAGES".to_string()], selected);
    } else {
        draw_rows(&mut canvas, "INBOX", &items, selected);
    }
    footer(&mut canvas, "ENTER OPEN   HOLD UP/DOWN PAGE");
}

/// Opens an inbox item's detail (title + body) and marks it read locally.
fn open_inbox_item(ctx: &mut DeviceContext, now: Option<&DateTime>, selected: usize) {
    let list = ctx.inbox_store.load().unwrap_or_default();
    let Some(item) = list.get(selected).cloned() else {
        return;
    };
    if let Err(err) = ctx.inbox_store.mark_read(item.id) {
        log::warn!("Failed to mark inbox item read: {err}");
    }

    let mut canvas = ctx.board.display.canvas_mut();
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
    ctx.board.display.refresh_full_best_effort();
    loop {
        match ctx.poll_background(now) {
            crate::ctx::BackgroundOutcome::AlarmHandled => {
                ctx.alarm_poll.mark_exit();
                return;
            }
            crate::ctx::BackgroundOutcome::VisibleChanged => return,
            crate::ctx::BackgroundOutcome::NoChange => {}
        }
        match poll_nav(ctx.board) {
            Nav::None => {}
            _ => return,
        }
        tick();
    }
}

/// Two-stage hour/minute stepper for a new daily alarm - editing repeat
/// mode or a specific one-shot date is left to the PC tool/server sync
/// path, not this on-device screen.
/// Outcome of the new-alarm editor so the caller can distinguish a real
/// alarm add from a user cancel from an RTC alarm interruption (which
/// must unwind to the unified router, not silently resume the old page).
enum AlarmEditOutcome {
    Added(StoredAlarm),
    Cancelled,
    AlarmInterrupted,
}

fn add_alarm_screen(
    ctx: &mut DeviceContext,
    now: Option<&DateTime>,
    existing: &[StoredAlarm],
) -> AlarmEditOutcome {
    let hour = match pick_number(ctx, now, "NEW ALARM - HOUR", 0, 23) {
        crate::ui::NumberPick::Value(v) => v,
        crate::ui::NumberPick::Cancelled => return AlarmEditOutcome::Cancelled,
        crate::ui::NumberPick::AlarmInterrupted => return AlarmEditOutcome::AlarmInterrupted,
    };
    let minute = match pick_number(ctx, now, "NEW ALARM - MINUTE", 0, 59) {
        crate::ui::NumberPick::Value(v) => v,
        crate::ui::NumberPick::Cancelled => return AlarmEditOutcome::Cancelled,
        crate::ui::NumberPick::AlarmInterrupted => return AlarmEditOutcome::AlarmInterrupted,
    };
    let id = match alarms::next_id(existing) {
        Some(id) => id,
        None => {
            log::warn!("Cannot add alarm: all 256 alarm ids are in use");
            return AlarmEditOutcome::Cancelled;
        }
    };
    AlarmEditOutcome::Added(StoredAlarm {
        id,
        hour,
        minute,
        repeat: Repeat::Daily,
        enabled: true,
        label: String::new(),
    })
}

/// Sync screen: connects Wi-Fi, fetches alarms and todos from the
/// configured server, applies them to the local stores, and displays the
/// result. All the actual work is `sync::sync_now` - shared with the
/// USB/BLE `SyncNow` command so the two paths can't drift (in particular,
/// both must connect Wi-Fi first: `main.rs` drops the boot-time connection
/// once NTP sync is done, so by the time a user reaches this screen there
/// usually isn't an active Wi-Fi connection to piggyback on).
fn sync_now_screen(ctx: &mut DeviceContext, now: Option<&DateTime>) {
    let Some(now_dt) = now else {
        show_message(
            ctx.board,
            "NO CLOCK",
            &["Clock not available"],
            std::time::Duration::from_secs(2),
        );
        return;
    };

    if ctx.pending_wifi_op.is_some() {
        show_message(
            ctx.board,
            "SYNC BUSY",
            &["SYNC ALREADY IN PROGRESS"],
            std::time::Duration::from_secs(2),
        );
        return;
    }

    // Render the syncing status once; the wait loop below polls keys and
    // USB so the screen never freezes while the sync task works.
    {
        let mut canvas = ctx.board.display.canvas_mut();
        canvas.clear();
        header(&mut canvas, "SYNC NOW");
        let label = "SYNCING...";
        let label_width = Canvas::text_prop_width(label, 2);
        canvas.draw_text_prop((400usize.saturating_sub(label_width)) / 2, 110, 2, label);
        footer(&mut canvas, "HOLD ENTER BACK");
        drop(canvas);
        ctx.board.display.refresh_full_best_effort();
    }

    let result = match ctx.start_sync(
        OpSource::Internal,
        *now_dt,
        crate::control::Command::SyncNow,
    ) {
        Ok(true) => {
            let mut result = None;
            while result.is_none() {
                // An RTC alarm must preempt the sync wait: AppRunner rings
                // over the page, and we unwind to the unified router so
                // main re-renders per the current Screen.
                if ctx.poll_alarm_snapshot() {
                    ctx.alarm_poll.mark_exit();
                    return;
                }
                let _ = ctx.poll_usb_control(now);
                if matches!(poll_nav(ctx.board), Nav::Enter | Nav::Cancel) {
                    // User backed out; the sync continues on the sync task
                    // and its receipt is applied by the main loop.
                    break;
                }
                if let Some(crate::sync_task::WifiOpEvent::SyncDone(outcome)) = ctx.poll_wifi_ops()
                {
                    result = Some(outcome);
                }
                tick();
            }
            result
        }
        Ok(false) => {
            show_message(
                ctx.board,
                "SYNC BUSY",
                &["SYNC ALREADY IN PROGRESS"],
                std::time::Duration::from_secs(2),
            );
            None
        }
        Err(err) => {
            log::warn!("Failed to dispatch sync: {err}");
            let err_msg = err.to_string();
            let truncated = if err_msg.chars().count() > 40 {
                format!("{}...", err_msg.chars().take(37).collect::<String>())
            } else {
                err_msg
            };
            show_message(
                ctx.board,
                "SYNC FAILED",
                &[&truncated],
                std::time::Duration::from_secs(2),
            );
            None
        }
    };

    match result {
        Some(Ok(sync::SyncOutcome::Applied {
            alarms,
            todos,
            inbox,
            inbox_truncated,
            ..
        })) => {
            let alarm_count = alarms.len();
            let todo_count = todos.len();
            let inbox_count = inbox.len();
            let msg = if inbox_truncated {
                format!("A:{alarm_count} T:{todo_count} IN:{inbox_count}+")
            } else {
                format!("A:{alarm_count} T:{todo_count} IN:{inbox_count}")
            };
            show_message(
                ctx.board,
                "SYNC OK",
                &[&msg],
                std::time::Duration::from_secs(2),
            );
        }
        Some(Err(err)) => {
            let err_msg = err.to_string();
            // Truncate long error messages for display - by chars, not
            // bytes, so this can't panic mid-UTF-8-codepoint.
            let truncated = if err_msg.chars().count() > 40 {
                format!("{}...", err_msg.chars().take(37).collect::<String>())
            } else {
                err_msg
            };
            show_message(
                ctx.board,
                "SYNC FAILED",
                &[&truncated],
                std::time::Duration::from_secs(2),
            );
            log::warn!("Sync failed: {err}");
        }
        None => {}
    }
}

fn ble_pairing_screen(ctx: &mut DeviceContext, now: Option<&DateTime>) {
    // Start BLE advertising on entry to the pairing screen.
    match crate::ble_control::BleControl::start() {
        Ok(ble) => {
            *ctx.ble_control = Some(ble);
            log::info!("BLE pairing screen: started advertising");
            // The S3 BT controller registers no IDF skip-light-sleep
            // callback (unlike Wi-Fi), so an idle moment in this screen's
            // loop could drop the radio into light sleep and break the
            // BLE connection. Hold an explicit pm lock for the whole
            // pairing session; it releases when this screen exits.
            let _light_sleep_block = match crate::power::LightSleepBlock::acquire("ble_pairing") {
                Ok(block) => Some(block),
                Err(err) => {
                    log::warn!("Failed to block light sleep for BLE pairing: {err}");
                    None
                }
            };

            // Display the pairing screen with instructions.
            let mut canvas = ctx.board.display.canvas_mut();
            canvas.clear();
            header(&mut canvas, "BLE PAIRING");
            canvas.draw_text_prop(8, 40, 1, "CONNECTING...");
            canvas.draw_text_prop(8, 60, 1, "Service UUID:");
            canvas.draw_text_prop(8, 72, 1, "d2c25e50-");
            canvas.draw_text_prop(8, 84, 1, "5e22-48d8...");
            footer(&mut canvas, "HOLD ENTER BACK");
            drop(canvas);
            ctx.board.display.refresh_full_best_effort();

            // This screen owns the main thread while pairing is active, so it
            // must drain BLE commands here. Waiting for the outer Home loop
            // would deadlock the control session because leaving this screen
            // tears BLE down.
            let started_at = std::time::Instant::now();
            let mut last_alarm_poll = std::time::Instant::now();
            loop {
                let _ = ctx.poll_usb_control(now);
                // Deferred replies for long Wi-Fi ops (SyncNow/SetWifi)
                // dispatched over BLE while pairing: delivered when the
                // sync task finishes.
                let _ = ctx.poll_wifi_ops();
                if let Some((id, cmd)) = ctx.ble_control.as_ref().and_then(|ble| ble.poll_command())
                {
                    let reply = crate::control::dispatch(
                        ctx,
                        crate::control::Channel::Ble,
                        id.as_deref(),
                        cmd,
                        now,
                    );
                    if let Some(ble) = ctx.ble_control.as_ref() {
                        ble.write_reply(&reply, id.as_deref());
                    }
                }
                // This is a passive status page with no confirm action, so
                // either ENTER gesture exits. The timeout is a final escape
                // hatch if a button event is ever lost while the radio stack
                // is active.
                if matches!(poll_nav(ctx.board), Nav::Enter | Nav::Cancel)
                    || started_at.elapsed() >= std::time::Duration::from_secs(120)
                {
                    break;
                }
                if last_alarm_poll.elapsed() >= std::time::Duration::from_secs(1) {
                    last_alarm_poll = std::time::Instant::now();
                    if let Ok(fresh) = ctx.rtc.read_time() {
                        match ctx.poll_local_alerts(&fresh) {
                            crate::ctx::BackgroundOutcome::AlarmHandled => {
                                ctx.alarm_poll.mark_exit();
                                break;
                            }
                            crate::ctx::BackgroundOutcome::VisibleChanged => {
                                // A reminder covered the screen: redraw the
                                // pairing page so input context and display
                                // match again.
                                let mut canvas = ctx.board.display.canvas_mut();
                                canvas.clear();
                                header(&mut canvas, "BLE PAIRING");
                                canvas.draw_text_prop(8, 40, 1, "CONNECTING...");
                                canvas.draw_text_prop(8, 60, 1, "Service UUID:");
                                canvas.draw_text_prop(8, 72, 1, "d2c25e50-");
                                canvas.draw_text_prop(8, 84, 1, "5e22-48d8...");
                                footer(&mut canvas, "HOLD ENTER BACK");
                                drop(canvas);
                                ctx.board.display.refresh_full_best_effort();
                            }
                            crate::ctx::BackgroundOutcome::NoChange => {}
                        }
                    }
                }
                tick();
            }
        }
        Err(err) => {
            log::warn!("Failed to start BLE: {err}");
            show_message(
                ctx.board,
                "BLE ERROR",
                &[&err.to_string()],
                std::time::Duration::from_secs(2),
            );
        }
    }

    // Stop BLE advertising on exit from the pairing screen.
    *ctx.ble_control = None;
    log::info!("BLE pairing screen: stopped advertising, reclaimed RAM");
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

pub fn next_alarm_label(store: &AlarmStore, now: &DateTime) -> Option<NextAlarmLabel> {
    let list = match store.load() {
        Ok(list) => list,
        Err(err) => {
            log::warn!("Failed to load alarms for next-alarm label: {err}");
            return None;
        }
    };
    alarms::next_due(&list, now).map(|alarm| {
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

pub fn todo_summary(store: &TodoStore, now: Option<&DateTime>) -> TodoSummary {
    let Ok(list) = store.load() else {
        return TodoSummary {
            pending: 0,
            due_today: 0,
        };
    };
    let mut summary = TodoSummary {
        pending: 0,
        due_today: 0,
    };
    for todo in &list {
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
