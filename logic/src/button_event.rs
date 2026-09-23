#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ButtonId {
    Enter,
    Up,
    Down,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ButtonEvent {
    Pressed(ButtonId),
    Released(ButtonId),
    LongPressed(ButtonId),
}
