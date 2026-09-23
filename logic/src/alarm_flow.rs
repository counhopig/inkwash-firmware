use crate::app::RtcAlarmSnapshot;

pub trait AlarmHost {
    fn alarm_flag(&mut self) -> Result<bool, String>;

    fn read_snapshot(&mut self) -> Result<RtcAlarmSnapshot, String>;

    fn snapshot_ready(&mut self, snapshot: RtcAlarmSnapshot) -> bool;

    fn dismiss(&mut self);

    fn drain_kicks(&mut self);
}

#[derive(Debug, Default)]
pub struct AlarmPoll {
    edge_consumed: bool,

    alarm_exit: bool,

    last_error: Option<String>,
}

impl AlarmPoll {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe_alarm_flag(&mut self, af: bool) -> bool {
        if !af {
            self.edge_consumed = false;
            return false;
        }
        !self.edge_consumed
    }

    pub fn mark_snapshot_dispatched(&mut self) {
        self.edge_consumed = true;
    }

    pub fn poll<H: AlarmHost>(&mut self, host: &mut H) -> bool {
        let af = match host.alarm_flag() {
            Ok(af) => af,
            Err(err) => {
                self.last_error = Some(err);
                return false;
            }
        };
        if !af {
            self.edge_consumed = false;
            return false;
        }
        if self.edge_consumed {
            return false;
        }
        let snapshot = match host.read_snapshot() {
            Ok(s) => s,
            Err(err) => {
                self.last_error = Some(err);
                return false;
            }
        };
        let firing = host.snapshot_ready(snapshot);
        self.edge_consumed = true;
        host.drain_kicks();
        if firing {
            self.alarm_exit = true;
        }
        firing
    }

    pub fn ring_dismiss<H: AlarmHost>(&mut self, host: &mut H) {
        host.dismiss();
        host.drain_kicks();
    }

    pub fn alarm_exit(&self) -> bool {
        self.alarm_exit
    }

    pub fn take_alarm_exit(&mut self) -> bool {
        let v = self.alarm_exit;
        self.alarm_exit = false;
        v
    }

    pub fn take_error(&mut self) -> Option<String> {
        self.last_error.take()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlarmSource {
    Home,

    BlockingPage,
}

impl AlarmPoll {
    pub fn consume_for(&mut self, source: AlarmSource) -> bool {
        match source {
            AlarmSource::Home => self.take_alarm_exit(),
            AlarmSource::BlockingPage => self.alarm_exit(),
        }
    }
}

impl AlarmPoll {
    pub fn mark_exit(&mut self) {
        self.alarm_exit = true;
    }
}

#[cfg(test)]
mod async_tests {
    use super::AlarmPoll;

    #[test]
    fn async_alarm_edge_is_coalesced_until_clear() {
        let mut poll = AlarmPoll::new();
        assert!(poll.observe_alarm_flag(true));
        poll.mark_snapshot_dispatched();
        assert!(!poll.observe_alarm_flag(true));
        assert!(!poll.observe_alarm_flag(false));
        assert!(poll.observe_alarm_flag(true));
    }
}
