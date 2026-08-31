//! Button event vocabulary, moved out of `rust-firmware/src/button.rs`
//! (which re-exports it) so the host-testable application state machine can
//! reference button transitions without the GPIO debounce driver.

/// A single debounced button transition, emitted by the `Button` driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ButtonEvent {
    Pressed,
    Released,
    LongPressed,
}
