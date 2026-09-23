pub const MAX_LISTED_ITEMS: usize = 7;

pub const LIST_REGION_Y: usize = 34;
pub const LIST_REGION_HEIGHT: usize = 266;
pub const LIST_FIRST_ROW_Y: usize = 39;
pub const LIST_ROW_HEIGHT: usize = 37;
pub const LIST_DISPLAY_HEIGHT: usize = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListWindow {
    pub first: usize,
    pub last_exclusive: usize,
    pub selected: usize,
}

impl ListWindow {
    pub const fn len(self) -> usize {
        self.last_exclusive - self.first
    }

    pub const fn is_empty(self) -> bool {
        self.first == self.last_exclusive
    }
}

pub fn list_window(item_count: usize, selected: usize) -> ListWindow {
    if item_count == 0 {
        return ListWindow {
            first: 0,
            last_exclusive: 0,
            selected: 0,
        };
    }

    let selected = if selected >= item_count {
        item_count - 1
    } else {
        selected
    };
    let max_first = item_count.saturating_sub(MAX_LISTED_ITEMS);
    let candidate_first = selected.saturating_sub(MAX_LISTED_ITEMS - 1);
    let first = if candidate_first > max_first {
        max_first
    } else {
        candidate_first
    };
    let candidate_last = first + MAX_LISTED_ITEMS;
    let last_exclusive = if candidate_last > item_count {
        item_count
    } else {
        candidate_last
    };
    ListWindow {
        first,
        last_exclusive,
        selected,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        list_window, ListWindow, LIST_DISPLAY_HEIGHT, LIST_FIRST_ROW_Y, LIST_REGION_HEIGHT,
        LIST_REGION_Y, LIST_ROW_HEIGHT, MAX_LISTED_ITEMS,
    };

    fn assert_window(
        item_count: usize,
        selected: usize,
        first: usize,
        last_exclusive: usize,
        visible_selected: usize,
    ) {
        assert_eq!(
            list_window(item_count, selected),
            ListWindow {
                first,
                last_exclusive,
                selected: visible_selected,
            }
        );
    }

    #[test]
    fn empty_and_small_lists_start_at_zero() {
        assert_window(0, 0, 0, 0, 0);
        assert_window(3, 0, 0, 3, 0);
        assert_window(3, 2, 0, 3, 2);
    }

    #[test]
    fn first_page_and_first_row_after_overflow() {
        assert_window(MAX_LISTED_ITEMS, 0, 0, 7, 0);
        assert_window(MAX_LISTED_ITEMS, 6, 0, 7, 6);
        assert_window(MAX_LISTED_ITEMS + 1, 0, 0, 7, 0);
        assert_window(MAX_LISTED_ITEMS + 1, 7, 1, 8, 7);
    }

    #[test]
    fn large_list_keeps_first_last_and_add_row_visible() {
        assert_window(20, 0, 0, 7, 0);
        assert_window(20, 19, 13, 20, 19);

        assert_window(21, 20, 14, 21, 20);
    }

    #[test]
    fn invalid_cursor_clamps_to_last_drawable_row() {
        assert_window(20, usize::MAX, 13, 20, 19);
    }

    #[test]
    fn list_refresh_region_contains_the_last_visible_row() {
        assert_eq!(LIST_REGION_Y + LIST_REGION_HEIGHT, LIST_DISPLAY_HEIGHT);
        let last_row_end = LIST_FIRST_ROW_Y + MAX_LISTED_ITEMS * LIST_ROW_HEIGHT;
        assert!(last_row_end <= LIST_REGION_Y + LIST_REGION_HEIGHT);
    }
}
