//! Shared 3-button primitives: short UP/DOWN moves, short ENTER confirms,
//! long ENTER goes back, and long UP/DOWN switches a page at a time.
//! Shared by all on-device screens. Text entry is intentionally absent:
//! user-authored strings are supplied by Desktop/Server only.

use crate::canvas::Canvas;

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

/// Rows visible at once. Longer lists scroll around the selection.
pub use inkwash_logic::list_window::MAX_LISTED_ITEMS;

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
    let window = inkwash_logic::list_window::list_window(items.len(), selected);
    let mut y = 39usize;
    for (index, item) in items
        .iter()
        .enumerate()
        .skip(window.first)
        .take(window.len())
    {
        if index == window.selected {
            canvas.stroke_rect(16, y, 368, LIST_ROW_HEIGHT - 2, 2);
            canvas.fill_rect(16, y, 5, LIST_ROW_HEIGHT - 2, true);
        }
        canvas.draw_text_prop(LIST_TEXT_X, y + 10, 1, item);
        y += LIST_ROW_HEIGHT;
    }
}
