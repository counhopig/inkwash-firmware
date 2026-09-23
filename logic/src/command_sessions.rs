use std::collections::VecDeque;

use crate::protocol::{Channel, Command, Reply};

pub const DEFAULT_CACHE_CAPACITY: usize = 8;

#[derive(Clone, PartialEq, Eq)]
struct CachedReply {
    request_id: String,
    command: Command,
    reply: Reply,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingRequest {
    pub request_id: Option<String>,
    pub command: Command,
}

#[derive(Clone, Default, PartialEq, Eq)]
struct TransportSession {
    session_id: Option<u64>,
    cached: VecDeque<CachedReply>,
    pending: Option<PendingRequest>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReserveError {
    StaleSession,

    AlreadyPending,
}

#[derive(Clone, PartialEq, Eq)]
pub struct CommandSessions {
    usb: TransportSession,
    ble: TransportSession,
    capacity: usize,
}

impl CommandSessions {
    pub fn new(capacity_per_transport: usize) -> Self {
        assert!(
            capacity_per_transport > 0,
            "command cache capacity must be positive"
        );
        Self {
            usb: TransportSession::default(),
            ble: TransportSession::default(),
            capacity: capacity_per_transport,
        }
    }

    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_CACHE_CAPACITY)
    }

    pub fn begin(&mut self, channel: Channel, session_id: u64) {
        let state = self.state_mut(channel);
        if state.session_id != Some(session_id) {
            state.session_id = Some(session_id);
            state.cached.clear();
            state.pending = None;
        }
    }

    pub fn begin_preserving_pending(&mut self, channel: Channel, session_id: u64) {
        let state = self.state_mut(channel);
        if state.session_id == Some(session_id) {
            return;
        }
        let pending = state.pending.take();
        state.session_id = Some(session_id);
        state.cached.clear();
        state.pending = pending;
    }

    pub fn end(&mut self, channel: Channel, session_id: u64) {
        let state = self.state_mut(channel);
        if state.session_id == Some(session_id) {
            *state = TransportSession::default();
        }
    }

    pub fn active_session(&self, channel: Channel) -> Option<u64> {
        self.state(channel).session_id
    }

    pub fn lookup(
        &self,
        channel: Channel,
        session_id: u64,
        request_id: &str,
        command: &Command,
    ) -> Option<Reply> {
        let state = self.state(channel);
        if state.session_id != Some(session_id) {
            return None;
        }
        state
            .cached
            .iter()
            .find(|entry| entry.request_id == request_id && &entry.command == command)
            .map(|entry| entry.reply.clone())
    }

    pub fn reserve_pending(
        &mut self,
        channel: Channel,
        session_id: u64,
        request_id: Option<String>,
        command: Command,
    ) -> Result<(), ReserveError> {
        let state = self.state_mut(channel);
        if state.session_id != Some(session_id) {
            return Err(ReserveError::StaleSession);
        }
        if state.pending.is_some() {
            return Err(ReserveError::AlreadyPending);
        }
        state.pending = Some(PendingRequest {
            request_id,
            command,
        });
        Ok(())
    }

    pub fn pending(&self, channel: Channel, session_id: u64) -> Option<&PendingRequest> {
        let state = self.state(channel);
        (state.session_id == Some(session_id))
            .then_some(state.pending.as_ref())
            .flatten()
    }

    pub fn cancel_pending(
        &mut self,
        channel: Channel,
        session_id: u64,
        request_id: Option<&str>,
        command: &Command,
    ) -> bool {
        let state = self.state_mut(channel);
        if state.session_id != Some(session_id) {
            return false;
        }
        if state.pending.as_ref().is_some_and(|pending| {
            pending.request_id.as_deref() == request_id && &pending.command == command
        }) {
            state.pending = None;
            return true;
        }
        false
    }

    pub fn pending_key(
        &self,
        channel: Channel,
        session_id: u64,
    ) -> Option<(Option<&str>, &Command)> {
        self.pending(channel, session_id)
            .map(|pending| (pending.request_id.as_deref(), &pending.command))
    }

    pub fn complete_terminal(
        &mut self,
        channel: Channel,
        session_id: u64,
        request_id: String,
        command: Command,
        reply: Reply,
    ) -> bool {
        let capacity = self.capacity;
        let state = self.state_mut(channel);
        if state.session_id != Some(session_id) {
            return false;
        }

        let matches_pending = state.pending.as_ref().is_some_and(|pending| {
            pending.request_id.as_deref() == Some(request_id.as_str()) && pending.command == command
        });
        if !is_terminal(&reply) {
            if matches_pending {
                state.pending = None;
                return true;
            }
            return false;
        }
        if matches_pending {
            state.pending = None;
        }

        if let Some(existing) = state
            .cached
            .iter_mut()
            .find(|entry| entry.request_id == request_id && entry.command == command)
        {
            existing.reply = reply;
            return true;
        }

        if state.cached.len() == capacity {
            state.cached.pop_front();
        }
        state.cached.push_back(CachedReply {
            request_id,
            command,
            reply,
        });
        true
    }

    pub fn complete_untagged_terminal(
        &mut self,
        channel: Channel,
        session_id: u64,
        command: Command,
        reply: Reply,
    ) -> bool {
        let state = self.state_mut(channel);
        if state.session_id != Some(session_id) {
            return false;
        }
        let matches_pending = state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.request_id.is_none() && pending.command == command);
        if !is_terminal(&reply) {
            if matches_pending {
                state.pending = None;
                return true;
            }
            return false;
        }
        if matches_pending {
            state.pending = None;
            return true;
        }
        false
    }

    fn state(&self, channel: Channel) -> &TransportSession {
        match channel {
            Channel::Usb => &self.usb,
            Channel::Ble => &self.ble,
        }
    }

    fn state_mut(&mut self, channel: Channel) -> &mut TransportSession {
        match channel {
            Channel::Usb => &mut self.usb,
            Channel::Ble => &mut self.ble,
        }
    }
}

impl Default for CommandSessions {
    fn default() -> Self {
        Self::with_default_capacity()
    }
}

fn is_terminal(reply: &Reply) -> bool {
    matches!(
        reply,
        Reply::Ok | Reply::Status { .. } | Reply::Error { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command() -> Command {
        Command::GetStatus
    }

    fn other_command() -> Command {
        Command::ClearAlarms
    }

    #[test]
    fn same_id_isolated_between_channels() {
        let mut sessions = CommandSessions::new(8);
        sessions.begin(Channel::Usb, 1);
        sessions.begin(Channel::Ble, 1);
        sessions.complete_terminal(Channel::Usb, 1, "same".into(), command(), Reply::Ok);
        assert_eq!(
            sessions.lookup(Channel::Usb, 1, "same", &command()),
            Some(Reply::Ok)
        );
        assert_eq!(sessions.lookup(Channel::Ble, 1, "same", &command()), None);
    }

    #[test]
    fn reconnect_id_reuse_does_not_replay_old_session() {
        let mut sessions = CommandSessions::default();
        sessions.begin(Channel::Usb, 10);
        sessions.complete_terminal(Channel::Usb, 10, "1".into(), command(), Reply::Ok);
        sessions.end(Channel::Usb, 10);
        sessions.begin(Channel::Usb, 11);
        assert_eq!(sessions.lookup(Channel::Usb, 11, "1", &command()), None);
        sessions.end(Channel::Usb, 11);
        sessions.begin(Channel::Usb, 10);
        assert_eq!(sessions.lookup(Channel::Usb, 10, "1", &command()), None);
    }

    #[test]
    fn late_old_session_completion_cannot_pollute_new_session() {
        let mut sessions = CommandSessions::default();
        sessions.begin(Channel::Ble, 1);
        sessions
            .reserve_pending(Channel::Ble, 1, Some("x".into()), command())
            .unwrap();
        sessions.begin(Channel::Ble, 2);
        assert!(!sessions.complete_terminal(Channel::Ble, 1, "x".into(), command(), Reply::Ok));
        assert_eq!(sessions.lookup(Channel::Ble, 2, "x", &command()), None);
    }

    #[test]
    fn busy_is_not_cached_and_retry_can_complete() {
        let mut sessions = CommandSessions::default();
        sessions.begin(Channel::Usb, 1);
        assert!(!sessions.complete_terminal(Channel::Usb, 1, "x".into(), command(), Reply::Busy));
        assert_eq!(sessions.lookup(Channel::Usb, 1, "x", &command()), None);
        sessions
            .reserve_pending(Channel::Usb, 1, Some("x".into()), command())
            .unwrap();
        assert_eq!(
            sessions.reserve_pending(Channel::Usb, 1, Some("y".into()), other_command()),
            Err(ReserveError::AlreadyPending)
        );
        assert!(sessions.complete_terminal(Channel::Usb, 1, "x".into(), command(), Reply::Ok));
        assert!(sessions.pending(Channel::Usb, 1).is_none());
    }

    #[test]
    fn busy_completion_releases_matching_pending_without_caching() {
        let mut sessions = CommandSessions::default();
        sessions.begin(Channel::Ble, 4);
        sessions
            .reserve_pending(Channel::Ble, 4, Some("busy".into()), command())
            .unwrap();
        assert!(sessions.complete_terminal(Channel::Ble, 4, "busy".into(), command(), Reply::Busy));
        assert!(sessions.pending(Channel::Ble, 4).is_none());
        assert_eq!(sessions.lookup(Channel::Ble, 4, "busy", &command()), None);
    }

    #[test]
    fn pending_key_is_available_until_terminal_completion() {
        let mut sessions = CommandSessions::default();
        sessions.begin(Channel::Usb, 7);
        sessions
            .reserve_pending(Channel::Usb, 7, Some("rtc-1".into()), command())
            .unwrap();
        let (id, pending_command) = sessions.pending_key(Channel::Usb, 7).unwrap();
        assert_eq!(id, Some("rtc-1"));
        assert_eq!(pending_command, &command());
        assert!(sessions.complete_terminal(Channel::Usb, 7, "rtc-1".into(), command(), Reply::Ok,));
        assert!(sessions.pending_key(Channel::Usb, 7).is_none());
    }

    #[test]
    fn pending_key_is_invalidated_by_session_change() {
        let mut sessions = CommandSessions::default();
        sessions.begin(Channel::Ble, 3);
        sessions
            .reserve_pending(Channel::Ble, 3, Some("wifi-1".into()), other_command())
            .unwrap();
        sessions.begin(Channel::Ble, 4);
        assert!(sessions.pending_key(Channel::Ble, 3).is_none());
        assert!(sessions.pending_key(Channel::Ble, 4).is_none());
    }

    #[test]
    fn handoff_generation_change_preserves_pending_correlation() {
        let mut sessions = CommandSessions::default();
        sessions.begin(Channel::Ble, 3);
        sessions
            .reserve_pending(Channel::Ble, 3, Some("wifi-1".into()), other_command())
            .unwrap();
        sessions.begin_preserving_pending(Channel::Ble, 4);
        let (id, pending_command) = sessions.pending_key(Channel::Ble, 4).unwrap();
        assert_eq!(id, Some("wifi-1"));
        assert_eq!(pending_command, &other_command());
        assert!(sessions.complete_terminal(
            Channel::Ble,
            4,
            "wifi-1".into(),
            other_command(),
            Reply::Ok,
        ));
        assert!(sessions.pending_key(Channel::Ble, 4).is_none());
    }

    #[test]
    fn untagged_pending_is_not_cached_and_clears_on_terminal_delivery() {
        let mut sessions = CommandSessions::with_default_capacity();
        sessions.begin(Channel::Usb, 9);
        sessions
            .reserve_pending(Channel::Usb, 9, None, command())
            .unwrap();
        let (id, pending_command) = sessions.pending_key(Channel::Usb, 9).unwrap();
        assert_eq!(id, None);
        assert_eq!(pending_command, &command());
        assert!(sessions.complete_untagged_terminal(Channel::Usb, 9, command(), Reply::Ok));
        assert!(sessions.pending(Channel::Usb, 9).is_none());
        assert!(sessions.lookup(Channel::Usb, 9, "", &command()).is_none());
    }

    #[test]
    fn untagged_ble_completion_cannot_cross_connection_generation() {
        let mut sessions = CommandSessions::default();
        sessions.begin(Channel::Ble, 41);
        sessions
            .reserve_pending(Channel::Ble, 41, None, command())
            .unwrap();
        sessions.begin(Channel::Ble, 42);
        assert!(!sessions.complete_untagged_terminal(Channel::Ble, 41, command(), Reply::Ok));
        sessions
            .reserve_pending(Channel::Ble, 42, None, command())
            .unwrap();
        assert!(sessions.complete_untagged_terminal(Channel::Ble, 42, command(), Reply::Ok));
    }

    #[test]
    fn cache_evicts_oldest_terminal_reply_per_transport() {
        let mut sessions = CommandSessions::new(2);
        sessions.begin(Channel::Usb, 1);
        for id in ["a", "b", "c"] {
            assert!(sessions.complete_terminal(Channel::Usb, 1, id.into(), command(), Reply::Ok,));
        }
        assert_eq!(sessions.lookup(Channel::Usb, 1, "a", &command()), None);
        assert_eq!(
            sessions.lookup(Channel::Usb, 1, "b", &command()),
            Some(Reply::Ok)
        );
        assert_eq!(
            sessions.lookup(Channel::Usb, 1, "c", &command()),
            Some(Reply::Ok)
        );
    }

    #[test]
    fn same_id_different_command_is_a_miss() {
        let mut sessions = CommandSessions::default();
        sessions.begin(Channel::Usb, 1);
        sessions.complete_terminal(Channel::Usb, 1, "x".into(), command(), Reply::Ok);
        assert_eq!(
            sessions.lookup(Channel::Usb, 1, "x", &other_command()),
            None
        );
    }
}
