//! Button event vocabulary, moved out of `rust-firmware/src/button.rs`
//! (which re-exports it) so the host-testable application state machine can
//! reference button transitions without the GPIO debounce driver.

/// Which physical button produced an event. The NOTE4 has three buttons;
/// the state machine needs the identity to drive page navigation, not just
/// "some key was pressed".
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ButtonId {
    Enter,
    Up,
    Down,
}

/// A single debounced button transition, emitted by the `Button` driver,
/// tagged with the physical button that produced it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ButtonEvent {
    Pressed(ButtonId),
    Released(ButtonId),
    LongPressed(ButtonId),
}
