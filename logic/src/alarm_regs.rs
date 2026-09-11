#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlarmRegs {
    pub minute: u8,
    pub hour: u8,
    pub day: Option<u8>,
    pub weekday: Option<u8>,
}
