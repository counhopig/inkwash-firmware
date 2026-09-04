//! Host-testable ordering contract for the shared Wi-Fi/BLE radio.
//!
//! The ESP-IDF side performs the same transitions around asynchronous
//! receipts.  Keeping the ordering model here makes the safety rules
//! executable without an ESP32 or NimBLE runtime.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BleRadioState {
    Idle,
    Suspending,
    Starting,
    Active,
    Stopping,
    Resuming,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BleRadioAction {
    SuspendWifi,
    StartBle,
    StopBle,
    ResumeWifi,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BleRadioError {
    Busy,
    StaleSession,
}

/// Minimal radio sequencing state machine.  It deliberately has no timing,
/// threads, or hardware calls; the firmware maps each action to its owner
/// task and advances it from the corresponding receipt.
pub struct BleRadioCoordinator {
    state: BleRadioState,
    session_id: Option<u64>,
}

impl Default for BleRadioCoordinator {
    fn default() -> Self {
        Self {
            state: BleRadioState::Idle,
            session_id: None,
        }
    }
}

impl BleRadioCoordinator {
    pub fn state(&self) -> BleRadioState {
        self.state
    }

    pub fn begin_start(
        &mut self,
        session_id: u64,
        wifi_busy: bool,
    ) -> Result<BleRadioAction, BleRadioError> {
        if wifi_busy || self.state != BleRadioState::Idle {
            return Err(BleRadioError::Busy);
        }
        self.session_id = Some(session_id);
        self.state = BleRadioState::Suspending;
        Ok(BleRadioAction::SuspendWifi)
    }

    pub fn wifi_suspended(&mut self, session_id: u64) -> Result<BleRadioAction, BleRadioError> {
        self.check_session(session_id, BleRadioState::Suspending)?;
        self.state = BleRadioState::Starting;
        Ok(BleRadioAction::StartBle)
    }

    pub fn ble_started(&mut self, session_id: u64) -> Result<(), BleRadioError> {
        self.check_session(session_id, BleRadioState::Starting)?;
        self.state = BleRadioState::Active;
        Ok(())
    }

    pub fn begin_stop(&mut self, session_id: u64) -> Result<BleRadioAction, BleRadioError> {
        self.check_session(session_id, self.state)?;
        if !matches!(self.state, BleRadioState::Starting | BleRadioState::Active) {
            return Err(BleRadioError::Busy);
        }
        self.state = BleRadioState::Stopping;
        Ok(BleRadioAction::StopBle)
    }

    pub fn ble_stopped(&mut self, session_id: u64) -> Result<BleRadioAction, BleRadioError> {
        self.check_session(session_id, BleRadioState::Stopping)?;
        self.state = BleRadioState::Resuming;
        Ok(BleRadioAction::ResumeWifi)
    }

    pub fn wifi_resumed(&mut self, session_id: u64) -> Result<(), BleRadioError> {
        self.check_session(session_id, BleRadioState::Resuming)?;
        self.state = BleRadioState::Idle;
        self.session_id = None;
        Ok(())
    }

    pub fn ble_start_failed(&mut self, session_id: u64) -> Result<BleRadioAction, BleRadioError> {
        self.check_session(session_id, BleRadioState::Starting)?;
        self.state = BleRadioState::Stopping;
        Ok(BleRadioAction::StopBle)
    }

    fn check_session(&self, session_id: u64, expected: BleRadioState) -> Result<(), BleRadioError> {
        if self.session_id != Some(session_id) || self.state != expected {
            return Err(BleRadioError::StaleSession);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_suspends_wifi_before_ble_and_stop_resumes_afterwards() {
        let mut radio = BleRadioCoordinator::default();
        assert_eq!(radio.begin_start(7, false), Ok(BleRadioAction::SuspendWifi));
        assert_eq!(radio.wifi_suspended(7), Ok(BleRadioAction::StartBle));
        radio.ble_started(7).unwrap();
        assert_eq!(radio.begin_stop(7), Ok(BleRadioAction::StopBle));
        assert_eq!(radio.ble_stopped(7), Ok(BleRadioAction::ResumeWifi));
        radio.wifi_resumed(7).unwrap();
        assert_eq!(radio.state(), BleRadioState::Idle);
    }

    #[test]
    fn start_failure_still_stops_before_resume() {
        let mut radio = BleRadioCoordinator::default();
        radio.begin_start(3, false).unwrap();
        radio.wifi_suspended(3).unwrap();
        assert_eq!(radio.ble_start_failed(3), Ok(BleRadioAction::StopBle));
        assert_eq!(radio.ble_stopped(3), Ok(BleRadioAction::ResumeWifi));
    }

    #[test]
    fn concurrent_wifi_operation_is_rejected_and_stale_results_are_ignored() {
        let mut radio = BleRadioCoordinator::default();
        assert_eq!(radio.begin_start(1, true), Err(BleRadioError::Busy));
        radio.begin_start(2, false).unwrap();
        assert_eq!(radio.wifi_suspended(1), Err(BleRadioError::StaleSession));
        assert_eq!(radio.state(), BleRadioState::Suspending);
    }
}
