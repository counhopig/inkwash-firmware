//! Bounded command deduplication and pending-request ownership.
//!
//! USB and BLE use the same command/reply types, but their request IDs live in
//! separate transport sessions. This cache deliberately provides a bounded
//! replay window rather than permanent exactly-once semantics: a duplicate
//! older than the retained window may be executed again.

use std::collections::VecDeque;

use crate::protocol::{Channel, Command, Reply};

/// Number of terminal replies retained per transport session by default.
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

/// Result of reserving the one deferred command slot for a transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReserveError {
    /// The caller has not opened this transport session, or the result came
    /// from a previous session.
    StaleSession,
    /// A deferred command is already associated with this transport session.
    AlreadyPending,
}

/// Per-transport, per-session bounded deduplication state.
///
/// A session transition clears both the replay cache and the pending command
/// association. This prevents an ID reused after USB reconnect or BLE
/// reconnect from matching an old reply, while allowing USB and BLE to reuse
/// the same request ID independently.
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

    /// Starts or resumes a transport session. Repeating the same session ID
    /// is idempotent; changing it invalidates all old cache and pending state.
    pub fn begin(&mut self, channel: Channel, session_id: u64) {
        let state = self.state_mut(channel);
        if state.session_id != Some(session_id) {
            state.session_id = Some(session_id);
            state.cached.clear();
            state.pending = None;
        }
    }

    /// Starts a new transport generation while preserving the one command
    /// correlation that is already being completed across a BLE handoff.
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

    /// Ends only the currently active session. A late completion from an old
    /// session cannot clear or populate a newer session.
    pub fn end(&mut self, channel: Channel, session_id: u64) {
        let state = self.state_mut(channel);
        if state.session_id == Some(session_id) {
            *state = TransportSession::default();
        }
    }

    pub fn active_session(&self, channel: Channel) -> Option<u64> {
        self.state(channel).session_id
    }

    /// Returns a reply only for an exact `(session, request_id, command)` key.
    /// A same-ID/different-command request is a miss and remains governed by
    /// the caller's existing command validation contract.
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

    /// Reserves the one deferred-reply association for a transport. `Busy`
    /// is returned by the caller when this fails; it is never cached here.
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

    /// Releases a reservation when admitting the command event failed before
    /// the state machine could produce a reply.
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

    /// Returns the correlation key reserved for the current deferred reply.
    /// The key remains owned by the session until the transport confirms the
    /// terminal frame, so a failed write can retry without re-executing the
    /// command.
    pub fn pending_key(
        &self,
        channel: Channel,
        session_id: u64,
    ) -> Option<(Option<&str>, &Command)> {
        self.pending(channel, session_id)
            .map(|pending| (pending.request_id.as_deref(), &pending.command))
    }

    /// Stores only a final reply and clears its matching pending association.
    /// `Busy` and `Pending` are intentionally not cached.
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
            // Busy/Pending is a transport response, not a replayable result,
            // but it still completes the exact reservation that produced it.
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

    /// Completes an untagged pending command after its reply was accepted by
    /// the transport. Untagged commands never enter the replay cache.
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
