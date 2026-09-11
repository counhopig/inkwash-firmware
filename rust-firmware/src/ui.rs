use crate::canvas::Canvas;

pub fn header(canvas: &mut Canvas, title: &str) {
    canvas.stroke_rect(16, 9, 14, 14, 2);
    canvas.fill_rect(21, 14, 4, 4, true);
    canvas.draw_text_prop(38, 8, 1, "INKWASH");
    let width = Canvas::text_prop_width(title, 1);
    canvas.draw_text_prop(384usize.saturating_sub(width), 8, 1, title);
    canvas.fill_rect(16, 29, 368, 1, true);
}

pub fn footer(_canvas: &mut Canvas, _hint: &str) {}

const LIST_FIRST_ROW_Y: usize = inkwash_logic::list_window::LIST_FIRST_ROW_Y;
const LIST_ROW_HEIGHT: usize = inkwash_logic::list_window::LIST_ROW_HEIGHT;

const LIST_TEXT_X: usize = 50;

pub fn draw_rows(canvas: &mut Canvas, title: &str, items: &[String], selected: usize) {
    canvas.clear();
    header(canvas, title);
    let window = inkwash_logic::list_window::list_window(items.len(), selected);
    let mut y = LIST_FIRST_ROW_Y;
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
