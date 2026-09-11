#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepKind {
    Light,
    Deep,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepBlocker {
    InputPending,
    PageDisallowsSleep,
    FinalDisplayPending,
    FinalPersistPending,
    NetworkInFlight,
    ProtocolReplyPending,
    OperationInFlight,
    RtcPlanUnconfirmed,
    WakePlanUnconfirmed,
    UsbConnected,
    EventQueueNotEmpty,
    InputLatchSet,
    TokenInvalid,
    NoPreparedSleep,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SleepInputs {
    pub input_pending: bool,
    pub page_allows_sleep: bool,
    pub final_display_pending: bool,
    pub final_persist_pending: bool,
    pub network_in_flight: bool,

    pub network_resumable: bool,
    pub protocol_reply_pending: bool,
    pub operation_in_flight: bool,
    pub rtc_plan_confirmed: bool,
    pub wake_plan_confirmed: bool,
    pub usb_connected: bool,
    pub event_queue_empty: bool,
    pub input_latch_clear: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SleepToken {
    pub kind: SleepKind,
    pub activity_version: u64,
    pub request_id: u64,
    pub prepared_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SleepPhase {
    Prepared(SleepToken),
    Committed(SleepToken),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SleepState {
    activity_version: u64,
    next_request_id: u64,
    phase: Option<SleepPhase>,
}

impl SleepState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn activity_version(&self) -> u64 {
        self.activity_version
    }

    pub fn activity(&mut self) {
        self.activity_version = self.activity_version.wrapping_add(1);
        self.phase = None;
    }

    pub fn cancel(&mut self) {
        self.activity();
    }

    pub fn prepared_token(&self) -> Option<SleepToken> {
        match self.phase {
            Some(SleepPhase::Prepared(token)) => Some(token),
            _ => None,
        }
    }

    pub fn is_committed(&self, token: SleepToken) -> bool {
        matches!(self.phase, Some(SleepPhase::Committed(current)) if current == token)
    }

    pub fn committed_kind(&self) -> Option<SleepKind> {
        match self.phase {
            Some(SleepPhase::Committed(token)) => Some(token.kind),
            _ => None,
        }
    }

    pub fn final_check(&self, token: SleepToken, inputs: SleepInputs) -> bool {
        if !self.is_committed(token) {
            return false;
        }
        match token.kind {
            SleepKind::Light => light_blocker(inputs).is_none(),
            SleepKind::Deep => deep_blocker(inputs, true).is_none(),
        }
    }

    pub fn prepare(
        &mut self,
        kind: SleepKind,
        now: u64,
        inputs: SleepInputs,
    ) -> Result<SleepToken, SleepBlocker> {
        let blocker = match kind {
            SleepKind::Light => light_blocker(inputs),

            SleepKind::Deep => deep_blocker(inputs, false),
        };
        if let Some(blocker) = blocker {
            self.phase = None;
            return Err(blocker);
        }
        self.next_request_id = self.next_request_id.wrapping_add(1);
        let token = SleepToken {
            kind,
            activity_version: self.activity_version,
            request_id: self.next_request_id,
            prepared_at: now,
        };
        self.phase = Some(SleepPhase::Prepared(token));
        Ok(token)
    }

    pub fn commit(
        &mut self,
        token: SleepToken,
        inputs: SleepInputs,
    ) -> Result<SleepToken, SleepBlocker> {
        if token.activity_version != self.activity_version {
            self.phase = None;
            return Err(SleepBlocker::TokenInvalid);
        }
        if !matches!(self.phase, Some(SleepPhase::Prepared(current)) if current == token) {
            return Err(SleepBlocker::NoPreparedSleep);
        }
        let blocker = match token.kind {
            SleepKind::Light => light_blocker(inputs),
            SleepKind::Deep => deep_blocker(inputs, true),
        };
        if let Some(blocker) = blocker {
            self.phase = None;
            return Err(blocker);
        }
        self.phase = Some(SleepPhase::Committed(token));
        Ok(token)
    }
}

fn light_blocker(inputs: SleepInputs) -> Option<SleepBlocker> {
    if inputs.input_pending {
        Some(SleepBlocker::InputPending)
    } else if !inputs.page_allows_sleep {
        Some(SleepBlocker::PageDisallowsSleep)
    } else if inputs.final_display_pending {
        Some(SleepBlocker::FinalDisplayPending)
    } else if inputs.final_persist_pending {
        Some(SleepBlocker::FinalPersistPending)
    } else if inputs.network_in_flight && !inputs.network_resumable {
        Some(SleepBlocker::NetworkInFlight)
    } else {
        None
    }
}

fn deep_blocker(inputs: SleepInputs, require_wake_plan: bool) -> Option<SleepBlocker> {
    light_blocker(inputs)
        .or_else(|| {
            inputs
                .network_in_flight
                .then_some(SleepBlocker::NetworkInFlight)
        })
        .or_else(|| {
            inputs
                .protocol_reply_pending
                .then_some(SleepBlocker::ProtocolReplyPending)
        })
        .or_else(|| {
            inputs
                .operation_in_flight
                .then_some(SleepBlocker::OperationInFlight)
        })
        .or_else(|| (!inputs.rtc_plan_confirmed).then_some(SleepBlocker::RtcPlanUnconfirmed))
        .or_else(|| {
            (require_wake_plan && !inputs.wake_plan_confirmed)
                .then_some(SleepBlocker::WakePlanUnconfirmed)
        })
        .or_else(|| inputs.usb_connected.then_some(SleepBlocker::UsbConnected))
        .or_else(|| (!inputs.event_queue_empty).then_some(SleepBlocker::EventQueueNotEmpty))
        .or_else(|| (!inputs.input_latch_clear).then_some(SleepBlocker::InputLatchSet))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> SleepInputs {
        SleepInputs {
            page_allows_sleep: true,
            rtc_plan_confirmed: true,
            wake_plan_confirmed: true,
            event_queue_empty: true,
            input_latch_clear: true,
            ..SleepInputs::default()
        }
    }

    #[test]
    fn light_sleep_allows_only_recoverable_network() {
        let mut inputs = ready();
        inputs.network_in_flight = true;
        inputs.network_resumable = true;
        assert!(SleepState::new()
            .prepare(SleepKind::Light, 10, inputs)
            .is_ok());
        inputs.network_resumable = false;
        assert_eq!(
            SleepState::new().prepare(SleepKind::Light, 10, inputs),
            Err(SleepBlocker::NetworkInFlight)
        );
    }

    #[test]
    fn light_sleep_blocks_final_commit_and_input() {
        let mut state = SleepState::new();
        let mut inputs = ready();
        inputs.final_display_pending = true;
        assert_eq!(
            state.prepare(SleepKind::Light, 1, inputs),
            Err(SleepBlocker::FinalDisplayPending)
        );
        inputs.final_display_pending = false;
        inputs.input_pending = true;
        assert_eq!(
            state.prepare(SleepKind::Light, 1, inputs),
            Err(SleepBlocker::InputPending)
        );
    }

    #[test]
    fn deep_sleep_requires_all_final_gates() {
        let mut state = SleepState::new();
        let inputs = ready();
        type BlockerCase = (SleepBlocker, fn(&mut SleepInputs));
        let cases: &[BlockerCase] = &[
            (SleepBlocker::NetworkInFlight, |i: &mut SleepInputs| {
                i.network_in_flight = true
            }),
            (SleepBlocker::ProtocolReplyPending, |i: &mut SleepInputs| {
                i.protocol_reply_pending = true
            }),
            (SleepBlocker::OperationInFlight, |i: &mut SleepInputs| {
                i.operation_in_flight = true
            }),
            (SleepBlocker::RtcPlanUnconfirmed, |i: &mut SleepInputs| {
                i.rtc_plan_confirmed = false
            }),
            (SleepBlocker::WakePlanUnconfirmed, |i: &mut SleepInputs| {
                i.wake_plan_confirmed = false
            }),
            (SleepBlocker::UsbConnected, |i: &mut SleepInputs| {
                i.usb_connected = true
            }),
            (SleepBlocker::EventQueueNotEmpty, |i: &mut SleepInputs| {
                i.event_queue_empty = false
            }),
            (SleepBlocker::InputLatchSet, |i: &mut SleepInputs| {
                i.input_latch_clear = false
            }),
        ];
        for (expected, set_blocker) in cases {
            let mut candidate = inputs;
            set_blocker(&mut candidate);
            if *expected == SleepBlocker::WakePlanUnconfirmed {
                let token = state.prepare(SleepKind::Deep, 2, candidate).unwrap();
                assert_eq!(state.commit(token, candidate), Err(*expected));
            } else {
                assert_eq!(state.prepare(SleepKind::Deep, 2, candidate), Err(*expected));
            }
        }
        assert!(state.prepare(SleepKind::Deep, 2, inputs).is_ok());
    }

    #[test]
    fn activity_invalidates_token_before_commit() {
        let mut state = SleepState::new();
        let token = state.prepare(SleepKind::Deep, 3, ready()).unwrap();
        state.activity();
        assert_eq!(
            state.commit(token, ready()),
            Err(SleepBlocker::TokenInvalid)
        );
    }

    #[test]
    fn commit_rechecks_inputs() {
        let mut state = SleepState::new();
        let token = state.prepare(SleepKind::Deep, 4, ready()).unwrap();
        let mut inputs = ready();
        inputs.input_latch_clear = false;
        assert_eq!(
            state.commit(token, inputs),
            Err(SleepBlocker::InputLatchSet)
        );
    }

    #[test]
    fn repeated_prepare_invalidates_previous_request() {
        let mut state = SleepState::new();
        let first = state.prepare(SleepKind::Deep, 4, ready()).unwrap();
        let second = state.prepare(SleepKind::Deep, 4, ready()).unwrap();
        assert_ne!(first.request_id, second.request_id);
        assert_eq!(
            state.commit(first, ready()),
            Err(SleepBlocker::NoPreparedSleep)
        );
        assert!(state.commit(second, ready()).is_ok());
    }

    #[test]
    fn committed_sleep_is_cancelled_by_new_activity() {
        let mut state = SleepState::new();
        let token = state.prepare(SleepKind::Deep, 6, ready()).unwrap();
        state.commit(token, ready()).unwrap();
        state.activity();
        assert!(!state.is_committed(token));
        assert_eq!(
            state.commit(token, ready()),
            Err(SleepBlocker::TokenInvalid)
        );
    }

    #[test]
    fn manual_deep_sleep_uses_same_admission_path() {
        let mut state = SleepState::new();
        let mut inputs = ready();
        inputs.page_allows_sleep = false;
        assert_eq!(
            state.prepare(SleepKind::Deep, 5, inputs),
            Err(SleepBlocker::PageDisallowsSleep)
        );
    }
}
