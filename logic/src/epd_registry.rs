//! Shared EPD render-request registry with a per-request terminal-state
//! rule, host-testable.
//!
//! The firmware's `pending_renders` registry pairs each in-flight
//! AppRunner render kick with its eventual EPD completion by request id.
//! The EPD task has a single latest-wins pending slot, so a submitted
//! request may end in exactly one of three ways:
//!
//! - `completed`: its panel refresh ran and reported success;
//! - `failed`: its panel refresh ran and reported failure;
//! - `superseded`: a newer request replaced it before it ran.
//!
//! The invariant the firmware relies on: **every request id receives
//! exactly one terminal outcome**; a completion with no matching kick
//! (already-terminated or never-registered) is observed and ignored.
//! This module implements that registry rule as pure orchestration so a
//! host harness drives the same logic the firmware's main loop runs.

use crate::runner::AsyncKick;

/// Terminal outcome of a render request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderTerminal {
    Completed,
    Failed,
    Superseded,
}

/// Outcome of feeding one EPD completion into the registry: the matched
/// kick (so the caller can `feed_completion_back` it) plus its terminal
/// state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedOutcome {
    /// A matching kick was found, consumed, and terminated.
    Matched(AsyncKick, RenderTerminal),
    /// No matching kick (stale / duplicate / never registered); ignored.
    Ignored,
}

/// The shared render-request registry.
#[derive(Debug, Default)]
pub struct RenderRegistry {
    pub pending: Vec<AsyncKick>,
}

impl RenderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an in-flight render kick.
    pub fn register(&mut self, kick: AsyncKick) {
        self.pending.push(kick);
    }

    /// Feed an EPD completion (identified by `request_id`) into the
    /// registry. `ok` selects Completed vs Failed; `superseded` marks a
    /// request replaced before it ran. Returns the terminal outcome
    /// applied to the matching kick, or `Ignored` when no kick matches.
    pub fn feed(&mut self, request_id: u64, ok: bool, superseded: bool) -> FeedOutcome {
        let Some(idx) = self
            .pending
            .iter()
            .position(|k| k.request_id == Some(request_id))
        else {
            return FeedOutcome::Ignored;
        };
        let kick = self.pending.remove(idx);
        let terminal = if superseded {
            RenderTerminal::Superseded
        } else if ok {
            RenderTerminal::Completed
        } else {
            RenderTerminal::Failed
        };
        FeedOutcome::Matched(kick, terminal)
    }

    /// A newer request replaced an older pending one in the EPD's single
    /// slot: the older kick is terminally `Superseded` (removed from the
    /// registry) and the newer one is registered. Mirrors the firmware's
    /// EPD task emitting a superseded completion for the replaced id.
    pub fn supersede_then_register(&mut self, replaced_id: u64, replacement: AsyncKick) {
        self.feed(replaced_id, false, true);
        self.register(replacement);
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Effect, RenderGeneration, RenderRequest};
    use crate::runner::AsyncKick;

    fn kick(id: u64) -> AsyncKick {
        AsyncKick {
            batch_id: crate::app::EffectBatchId(0),
            effect_id: crate::app::EffectId(1),
            operation_id: crate::app::OperationId(1),
            render_generation: Some(RenderGeneration(1)),
            effect: Effect::Render(RenderRequest {
                generation: RenderGeneration(1),
            }),
            request_id: Some(id),
        }
    }

    #[test]
    fn each_request_id_gets_exactly_one_terminal() {
        let mut reg = RenderRegistry::new();
        reg.register(kick(1));
        // First completion matches -> terminal; second (duplicate) ignored.
        assert!(matches!(
            reg.feed(1, true, false),
            FeedOutcome::Matched(_, RenderTerminal::Completed)
        ));
        assert_eq!(reg.feed(1, true, false), FeedOutcome::Ignored);
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn superseded_terminates_replaced_request() {
        let mut reg = RenderRegistry::new();
        reg.register(kick(1));
        // A newer request replaces 1: 1 is superseded, 2 is registered.
        reg.supersede_then_register(1, kick(2));
        assert_eq!(reg.len(), 1);
        // 1 has its terminal; a late completion for it is ignored.
        assert_eq!(reg.feed(1, true, false), FeedOutcome::Ignored);
        // 2 completes normally.
        assert!(matches!(
            reg.feed(2, true, false),
            FeedOutcome::Matched(_, RenderTerminal::Completed)
        ));
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn failed_completion_is_terminal() {
        let mut reg = RenderRegistry::new();
        reg.register(kick(7));
        assert!(matches!(
            reg.feed(7, false, false),
            FeedOutcome::Matched(_, RenderTerminal::Failed)
        ));
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn unknown_completion_is_ignored() {
        let mut reg = RenderRegistry::new();
        assert_eq!(reg.feed(999, true, false), FeedOutcome::Ignored);
        assert_eq!(reg.feed(999, false, true), FeedOutcome::Ignored);
    }
}
