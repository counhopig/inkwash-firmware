//! Shared 3-button primitives: short UP/DOWN moves, short ENTER confirms,
//! long ENTER goes back, and long UP/DOWN switches a page at a time.
//! Shared by all on-device screens. Text entry is intentionally absent:
//! user-authored strings are supplied by Desktop/Server only.

use std::thread;
use std::time::Duration;

use crate::board::Note4Board;
use crate::button::{ButtonEvent, POLL_INTERVAL_MS};
use crate::canvas::Canvas;
use crate::ctx::DeviceContext;
use crate::display::Rect;
use crate::rtc::DateTime;
use crate::watchdog;

/// Outcome of one blocking poll wait: which control fired, if any.
pub enum Nav {
    None,
    Up,
    Down,
    Enter,
    /// Long ENTER: return to the previous screen.
    Cancel,
    /// Long UP/DOWN: switch a page at a time.
    PageUp,
    PageDown,
}

pub fn poll_nav(board: &mut Note4Board) -> Nav {
    if let Some(event) = board.key_enter.poll() {
        match event {
            ButtonEvent::Pressed => return Nav::Enter,
            ButtonEvent::LongPressed => return Nav::Cancel,
            ButtonEvent::Released => {}
        }
    }
    if let Some(event) = board.key_up.poll() {
        match event {
            ButtonEvent::Pressed => return Nav::Up,
            ButtonEvent::LongPressed => return Nav::PageUp,
            ButtonEvent::Released => {}
        }
    }
    if let Some(event) = board.key_down.poll() {
        match event {
            ButtonEvent::Pressed => return Nav::Down,
            ButtonEvent::LongPressed => return Nav::PageDown,
            ButtonEvent::Released => {}
        }
    }
    Nav::None
}

pub fn tick() {
    watchdog::feed();
    thread::sleep(Duration::from_millis(POLL_INTERVAL_MS as u64));
}

pub fn header(canvas: &mut Canvas, title: &str) {
    // Same brand block as the home screen: ink square mark + wordmark.
    canvas.stroke_rect(16, 9, 14, 14, 2);
    canvas.fill_rect(21, 14, 4, 4, true);
    canvas.draw_text_prop(38, 8, 1, "INKWASH");
    let width = Canvas::text_prop_width(title, 1);
    canvas.draw_text_prop(384usize.saturating_sub(width), 8, 1, title);
    canvas.fill_rect(16, 29, 368, 1, true);
}

/// Reserved for screen call sites that still describe their controls in
/// code. The device UI intentionally has no persistent button-hint footer.
pub fn footer(_canvas: &mut Canvas, _hint: &str) {}

pub fn show_message(board: &mut Note4Board, title: &str, lines: &[&str], pause: Duration) {
    let mut canvas = board.display.canvas_mut();
    canvas.clear();
    header(&mut canvas, title);
    // Centered horizontally: these are one-off status toasts (sync
    // result, sleep notice, BLE error), not scannable/left-reading
    // content, so they read better as a centered caption than pinned to
    // a fixed left margin.
    let mut y = 110usize;
    for line in lines {
        let width = Canvas::text_prop_width(line, 2);
        canvas.draw_text_prop((400usize.saturating_sub(width)) / 2, y, 2, line);
        y += 38;
    }
    drop(canvas);
    board.display.refresh_partial_best_effort(Rect {
        x: 0,
        y: 0,
        width: 400,
        height: 300,
    });
    thread::sleep(pause);
}

/// Rows visible at once. Longer lists scroll around the selection.
pub const MAX_LISTED_ITEMS: usize = 7;

const LIST_ROW_HEIGHT: usize = 37;
/// Fixed left edge for row text, selected or not - previously the selected
/// row's ">" chevron pushed its text 26px right of every other row's, so
/// the reading edge jumped as the selection moved. The stroke/accent-bar
/// chrome now lives entirely to the left of this column instead.
const LIST_TEXT_X: usize = 50;

/// Draws `items` under `title` as a scrolling row list, with the row at
/// `selected` highlighted by an outlined rect + left accent bar. Shared by
/// every screen that's "a list of things, pick or toggle one" - the
/// settings menu (via `pick_from_list` below) and the Alarms/Todos pages
/// (via `screens::render_alarm_page`/`render_todo_page`) - so they read as
/// one consistent visual language instead of two subtly different ones.
pub fn draw_rows(canvas: &mut Canvas, title: &str, items: &[String], selected: usize) {
    canvas.clear();
    header(canvas, title);
    let selected = selected.min(items.len().saturating_sub(1));
    let first = selected.saturating_sub(MAX_LISTED_ITEMS - 1);
    let mut y = 39usize;
    for (index, item) in items.iter().enumerate().skip(first).take(MAX_LISTED_ITEMS) {
        if index == selected {
            canvas.stroke_rect(16, y, 368, LIST_ROW_HEIGHT - 2, 2);
            canvas.fill_rect(16, y, 5, LIST_ROW_HEIGHT - 2, true);
        }
        canvas.draw_text_prop(LIST_TEXT_X, y + 10, 1, item);
        y += LIST_ROW_HEIGHT;
    }
}

/// Outcome of [`pick_from_list`] - not just which row was chosen, because
/// Settings needs to distinguish "picked a row", "cancelled" and "long
/// UP/DOWN pressed" (which every other page treats as "open the GO TO
/// drawer"). Making the long-press an explicit outcome lets the Settings
/// screens behave like the rest of the app instead of silently swallowing
/// it as a page scroll.
pub enum PickResult {
    Selected(usize),
    /// Hold ENTER: back out of the list.
    Cancelled,
    /// Long UP/DOWN: the caller should open the navigation drawer.
    OpenNav,
}

/// Blocking wheel-list picker: draws `items` (already formatted by the
/// caller, e.g. with a "[x] " done marker baked in) under `title` via
/// [`draw_rows`], and returns the chosen index on ENTER or `None` on
/// hold-to-cancel. Shared by every screen that's "a list of things, pick
/// one" - the menu, alarms list, todos list. `initial` is the row selected
/// on first draw (e.g. the day a week view was opened for, or the current
/// page in the GO TO drawer) - callers that don't care pass 0.
pub fn pick_from_list(
    ctx: &mut DeviceContext,
    now: Option<&DateTime>,
    title: &str,
    items: &[String],
    hint: &str,
    initial: usize,
) -> PickResult {
    if items.is_empty() {
        return PickResult::Cancelled;
    }
    let mut selected = initial.min(items.len() - 1);
    let mut needs_redraw = true;
    let mut first_draw = true;
    loop {
        if ctx.poll_background(now) {
            return PickResult::Cancelled;
        }
        if needs_redraw {
            let mut canvas = ctx.board.display.canvas_mut();
            draw_rows(&mut canvas, title, items, selected);
            footer(&mut canvas, hint);
            drop(canvas);
            if first_draw {
                ctx.board.display.refresh_full_best_effort();
                first_draw = false;
            } else {
                ctx.board.display.refresh_partial_best_effort(Rect {
                    x: 8,
                    y: 34,
                    width: 384,
                    height: 226,
                });
            }
            needs_redraw = false;
        }
        match poll_nav(ctx.board) {
            Nav::Up => {
                selected = if selected == 0 {
                    items.len() - 1
                } else {
                    selected - 1
                };
                needs_redraw = true;
            }
            Nav::Down => {
                selected = (selected + 1) % items.len();
                needs_redraw = true;
            }
            Nav::Enter => return PickResult::Selected(selected),
            Nav::Cancel => return PickResult::Cancelled,
            Nav::PageUp | Nav::PageDown => return PickResult::OpenNav,
            Nav::None => {}
        }
        tick();
    }
}

/// Blocking UP/DOWN-cycles-a-number, ENTER-confirms stepper, wrapping in
/// `[min, max]`. `None` on hold-to-cancel.
pub fn pick_number(
    ctx: &mut DeviceContext,
    now: Option<&DateTime>,
    title: &str,
    min: u8,
    max: u8,
) -> Option<u8> {
    let mut value = min;
    let mut needs_redraw = true;
    let mut first_draw = true;
    loop {
        if ctx.poll_background(now) {
            return None;
        }
        if needs_redraw {
            let mut canvas = ctx.board.display.canvas_mut();
            canvas.clear();
            header(&mut canvas, title);
            let label = format!("{value:02}");
            let number_width = Canvas::text_prop_width(&label, 5);
            let box_width = number_width + 64;
            let box_x = 200usize.saturating_sub(box_width / 2);
            // Label+box block vertically centered in the space below the
            // header rule instead of floating near the top with a large
            // empty void underneath; "CHOOSE VALUE" is centered on the
            // canvas (a caption for the whole screen) rather than pinned
            // to a hand-tuned x that only lined up by accident, and the
            // value digits are centered in the box's remaining space
            // (right of the accent bar) instead of a fixed offset that
            // only happened to fit one particular digit width.
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
            // Center the 80px-tall scale-5 digits in the box's leftover
            // space (right of the accent bar) - pinning them near the top
            // left dead air under them, unbalanced against the box.
            let value_x = box_x + 7 + (box_width - 7 - number_width) / 2;
            let value_y = BOX_TOP + (BOX_H.saturating_sub(80)) / 2;
            canvas.draw_text_prop(value_x, value_y, 5, &label);
            footer(&mut canvas, "UP/DOWN CHANGE   ENTER OK   HOLD ENTER BACK");
            drop(canvas);
            if first_draw {
                ctx.board.display.refresh_full_best_effort();
                first_draw = false;
            } else {
                ctx.board.display.refresh_partial_best_effort(Rect {
                    x: 96,
                    y: 78,
                    width: 208,
                    height: 172,
                });
            }
            needs_redraw = false;
        }
        match poll_nav(ctx.board) {
            Nav::Up => {
                value = if value == max { min } else { value + 1 };
                needs_redraw = true;
            }
            Nav::Down => {
                value = if value == min { max } else { value - 1 };
                needs_redraw = true;
            }
            Nav::Enter => return Some(value),
            Nav::Cancel => return None,
            Nav::PageUp => {
                value = value.saturating_add(10).min(max);
                needs_redraw = true;
            }
            Nav::PageDown => {
                value = value.saturating_sub(10).max(min);
                needs_redraw = true;
            }
            Nav::None => {}
        }
        tick();
    }
}
