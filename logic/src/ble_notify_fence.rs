//! Matching BLE notify-tx callbacks to the reply that produced them.
//!
//! NimBLE reports a completed notification as a callback carrying only the
//! connection handle and a status — no reply identity. The firmware therefore
//! keeps one attempt armed at a time and labels the next callback for that
//! handle with it.
//!
//! With `CONFIG_BT_NIMBLE_MAX_CONNECTIONS=1` the controller reuses the same
//! connection handle for every client, so a callback belonging to the previous
//! connection is indistinguishable from one belonging to the current one. The
//! mailbox fences a handle whose attempt was still outstanding when the
//! connection went away; callbacks within `RETIRED_HANDLE_GRACE` of the fence
//! are then not attributed to a fresh attempt.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long a handle stays fenced after an attempt was abandoned.
pub const RETIRED_HANDLE_GRACE: Duration = Duration::from_secs(3);

/// Fenced handles retained at once; a controller reusing one handle needs one.
pub const RETIRED_HANDLE_CAPACITY: usize = 4;

/// One outstanding notify-tx attempt. `attempt_id` identifies it to the worker
/// that armed it, which ignores callbacks labelled with any other attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotifyAttempt {
    pub session_id: u64,
    pub generation: u64,
    pub conn_handle: u16,
    pub attempt_id: u64,
}

#[derive(Debug, Default)]
pub struct NotifyAttemptMailbox {
    armed: Option<NotifyAttempt>,
    retired_handles: VecDeque<(u16, Instant)>,
}

impl NotifyAttemptMailbox {
    fn discard_expired_handles(&mut self, now: Instant) {
        self.retired_handles.retain(|(_, retired_at)| {
            now.saturating_duration_since(*retired_at) < RETIRED_HANDLE_GRACE
        });
    }

    pub fn is_fenced(&mut self, conn_handle: u16, now: Instant) -> bool {
        self.discard_expired_handles(now);
        self.retired_handles
            .iter()
            .any(|(handle, _)| *handle == conn_handle)
    }

    fn fence_handle(&mut self, conn_handle: u16, now: Instant) {
        self.discard_expired_handles(now);
        self.retired_handles
            .retain(|(handle, _)| *handle != conn_handle);
        if self.retired_handles.len() == RETIRED_HANDLE_CAPACITY {
            self.retired_handles.pop_front();
        }
        self.retired_handles.push_back((conn_handle, now));
    }

    pub fn armed(&self) -> Option<NotifyAttempt> {
        self.armed
    }

    /// Arms `attempt` for the next callback on its handle. Fails when another
    /// attempt is outstanding or the handle is still fenced: in both cases the
    /// next callback for that handle cannot be attributed to this attempt.
    pub fn arm(&mut self, attempt: NotifyAttempt, now: Instant) -> bool {
        if self.armed.is_some() || self.is_fenced(attempt.conn_handle, now) {
            return false;
        }
        self.armed = Some(attempt);
        true
    }

    /// Abandons `attempt`: a callback for it may still be in flight, so its
    /// handle is fenced.
    pub fn quarantine(&mut self, attempt: NotifyAttempt, now: Instant) {
        if self.armed == Some(attempt) {
            self.armed = None;
        }
        self.fence_handle(attempt.conn_handle, now);
    }

    /// Releases the attempt the connection of `generation` left armed.
    ///
    /// Only a generation that still had an attempt outstanding can produce a
    /// late callback, so only that case fences the handle. Fencing every
    /// disconnect would mute the first replies of the next connection, which
    /// reuses the handle — and a reply that is never sent is never answered.
    pub fn release_generation(
        &mut self,
        session_id: u64,
        generation: u64,
        conn_handle: u16,
        now: Instant,
    ) {
        let outstanding = self.armed.is_some_and(|attempt| {
            attempt.session_id == session_id
                && attempt.generation == generation
                && attempt.conn_handle == conn_handle
        });
        if !outstanding {
            return;
        }
        self.armed = None;
        self.fence_handle(conn_handle, now);
    }

    /// Labels the callback that just arrived with the armed attempt, if it can
    /// belong to it. A callback arriving while nothing is armed proves no
    /// attempt of the fenced generation is outstanding any more, so the fence
    /// on that handle is lifted early.
    pub fn take_for_callback(&mut self, conn_handle: u16, now: Instant) -> Option<NotifyAttempt> {
        let Some(attempt) = self.armed else {
            self.discard_expired_handles(now);
            self.retired_handles
                .retain(|(handle, _)| *handle != conn_handle);
            return None;
        };
        if attempt.conn_handle != conn_handle {
            return None;
        }
        self.armed.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(attempt_id: u64) -> NotifyAttempt {
        NotifyAttempt {
            session_id: 7,
            generation: 2,
            conn_handle: 0,
            attempt_id,
        }
    }

    #[test]
    fn first_attempt_arms_and_its_callback_is_labelled() {
        let now = Instant::now();
        let mut mailbox = NotifyAttemptMailbox::default();
        assert!(mailbox.arm(attempt(1), now));
        assert_eq!(mailbox.take_for_callback(0, now), Some(attempt(1)));
        assert_eq!(mailbox.armed(), None);
    }

    #[test]
    fn a_second_attempt_waits_for_the_first_callback() {
        let now = Instant::now();
        let mut mailbox = NotifyAttemptMailbox::default();
        assert!(mailbox.arm(attempt(1), now));
        assert!(!mailbox.arm(attempt(2), now));
        assert_eq!(mailbox.take_for_callback(0, now), Some(attempt(1)));
    }

    #[test]
    fn a_disconnect_without_an_outstanding_attempt_leaves_the_handle_open() {
        let now = Instant::now();
        let mut mailbox = NotifyAttemptMailbox::default();
        mailbox.release_generation(7, 2, 0, now);
        assert!(
            !mailbox.is_fenced(0, now),
            "replies sent right after a clean disconnect must go out"
        );
        assert!(mailbox.arm(attempt(1), now));
    }

    #[test]
    fn a_disconnect_with_an_outstanding_attempt_fences_the_handle() {
        let now = Instant::now();
        let mut mailbox = NotifyAttemptMailbox::default();
        assert!(mailbox.arm(attempt(1), now));
        mailbox.release_generation(7, 2, 0, now);
        assert!(mailbox.is_fenced(0, now));
        assert!(
            !mailbox.arm(attempt(2), now),
            "a late callback for the abandoned attempt must not be labelled with the new one"
        );
        assert!(
            mailbox.arm(
                attempt(2),
                now + RETIRED_HANDLE_GRACE + Duration::from_millis(1)
            ),
            "the fence must expire so the reply can still be sent"
        );
    }

    #[test]
    fn a_disconnect_for_another_generation_does_not_fence() {
        let now = Instant::now();
        let mut mailbox = NotifyAttemptMailbox::default();
        assert!(mailbox.arm(attempt(1), now));
        mailbox.release_generation(7, 1, 0, now);
        assert!(!mailbox.is_fenced(0, now));
        assert_eq!(mailbox.armed(), Some(attempt(1)));
    }

    #[test]
    fn an_unarmed_callback_lifts_the_fence_early() {
        let now = Instant::now();
        let mut mailbox = NotifyAttemptMailbox::default();
        mailbox.quarantine(attempt(1), now);
        assert!(mailbox.is_fenced(0, now));
        assert_eq!(mailbox.take_for_callback(0, now), None);
        assert!(
            !mailbox.is_fenced(0, now),
            "observing a stray callback proves no abandoned attempt is pending"
        );
    }

    #[test]
    fn quarantine_abandons_the_armed_attempt() {
        let now = Instant::now();
        let mut mailbox = NotifyAttemptMailbox::default();
        assert!(mailbox.arm(attempt(1), now));
        mailbox.quarantine(attempt(1), now);
        assert_eq!(mailbox.armed(), None);
        assert!(mailbox.is_fenced(0, now));
    }

    #[test]
    fn callbacks_for_another_handle_are_not_labelled() {
        let now = Instant::now();
        let mut mailbox = NotifyAttemptMailbox::default();
        assert!(mailbox.arm(attempt(1), now));
        assert_eq!(mailbox.take_for_callback(3, now), None);
        assert_eq!(mailbox.armed(), Some(attempt(1)));
    }
}
