//! Pure BLE startup memory admission policy.
//!
//! The ESP32-S3 controller allocates from internal DMA-capable RAM and the
//! NimBLE port asserts on allocation failure. Keep a conservative admission
//! check independent of the ESP-IDF crate so its boundary behavior is tested
//! on the host.

/// Minimum internal heap required before attempting controller startup.
///
/// The 60 KiB floor leaves a bounded margin below the NOTE4 measurement while
/// avoiding the known failure mode where controller startup cannot allocate a
/// 4 KiB block and asserts in C.
pub const MIN_INTERNAL_FREE: usize = 60 * 1024;

/// Minimum largest contiguous internal block required before startup.
///
/// This is six times the 0x1000-byte allocation observed immediately before
/// the previous controller assertion, protecting against fragmentation while
/// admitting the post-configuration NOTE4 measurement (~31 KiB).
pub const MIN_INTERNAL_LARGEST_BLOCK: usize = 24 * 1024;

/// Returns whether the measured heap can safely enter BLE controller init.
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
