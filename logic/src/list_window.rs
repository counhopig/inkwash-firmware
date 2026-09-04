//! Shared visible-window arithmetic for list renderers.

/// Number of rows that fit in the list area of the NOTE4 display.
pub const MAX_LISTED_ITEMS: usize = 7;

/// The slice of a list that the renderer should draw, with `selected` always
/// inside the slice when the list is non-empty.
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

/// Computes a scrolling window for `item_count` rows. Out-of-range cursors
/// are conservatively clamped; button navigation is responsible for its
/// own wrap semantics before projecting a ViewModel.
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
    use super::{list_window, ListWindow, MAX_LISTED_ITEMS};

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
        // N alarms plus the trailing ADD row: selecting ADD puts it in the
        // last visible slot, even when the list is much longer than one page.
        assert_window(21, 20, 14, 21, 20);
    }

    #[test]
    fn invalid_cursor_clamps_to_last_drawable_row() {
        assert_window(20, usize::MAX, 13, 20, 19);
    }
}
