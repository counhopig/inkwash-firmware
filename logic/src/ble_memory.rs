pub const MIN_INTERNAL_FREE: usize = 60 * 1024;

pub const MIN_INTERNAL_LARGEST_BLOCK: usize = 24 * 1024;

pub const fn sufficient_internal_heap(free: usize, largest_block: usize) -> bool {
    free >= MIN_INTERNAL_FREE && largest_block >= MIN_INTERNAL_LARGEST_BLOCK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_admission_boundaries_are_accepted() {
        assert!(sufficient_internal_heap(
            MIN_INTERNAL_FREE,
            MIN_INTERNAL_LARGEST_BLOCK
        ));
    }

    #[test]
    fn one_byte_below_either_boundary_is_rejected() {
        assert!(!sufficient_internal_heap(
            MIN_INTERNAL_FREE - 1,
            MIN_INTERNAL_LARGEST_BLOCK
        ));
        assert!(!sufficient_internal_heap(
            MIN_INTERNAL_FREE,
            MIN_INTERNAL_LARGEST_BLOCK - 1
        ));
    }

    #[test]
    fn measured_note4_heap_is_admitted() {
        assert!(sufficient_internal_heap(64_831, 31_744));
    }
}
