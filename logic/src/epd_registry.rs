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

use crate::render_plan::{plan_render, RenderPlan, ViewModel};
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

/// The shared render-request registry, plus the renderer's private
/// ViewModel cache (Stage 5).
///
/// The cache holds the last *successfully shown* ViewModel and the number
/// of consecutive partial refreshes since the last Full. It is private to
/// the renderer - never part of `AppState`, never read or written by the
/// business layer. Rules enforced here (and host-tested):
///
/// - [`RenderRegistry::plan_for`] diff-s the incoming request's ViewModel
///   against the cached one to decide Noop / Partial / Full.
/// - The cache is updated only on a **successful** completion whose
///   render generation is still current.
/// - A failed completion invalidates the cache (`last_shown = None`) so
///   the next render is forced Full (partial-failure recovery).
/// - A superseded completion does not touch the cache: the replaced
///   request's pixels never reached the panel; the replacement request
///   updates the cache on its own completion.
/// - Each successful Full resets the partial counter; each successful
///   Partial increments it toward `PARTIAL_MAINTENANCE_LIMIT`.
#[derive(Debug, Default)]
pub struct RenderRegistry {
    pub pending: Vec<AsyncKick>,
    /// Last successfully-shown ViewModel (renderer-private cache).
    pub last_shown: Option<ViewModel>,
    /// Consecutive successful partial refreshes since the last Full.
    pub partials_since_maintenance: u32,
}

impl RenderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide the refresh plan for a request carrying `vm`, against the
    /// cached last-shown ViewModel. Does not mutate the cache.
    pub fn plan_for(&self, vm: &ViewModel) -> RenderPlan {
        plan_render(
            self.last_shown.as_ref(),
            vm,
            self.partials_since_maintenance,
        )
    }

    /// Apply the terminal outcome of a render to the cache. `success` is
    /// true for a Completed (pixels reached the panel); false for Failed.
    /// `generation_is_current` is false when the request's render
    /// generation is older than the current visible state.
    pub fn note_terminal(
        &mut self,
        vm: &ViewModel,
        plan: &RenderPlan,
        success: bool,
        generation_is_current: bool,
    ) {
        if !success {
            // Partial-failure recovery: forget what the panel shows so the
            // next render is a Full from a clean slate.
            self.last_shown = None;
            self.partials_since_maintenance = 0;
            return;
        }
        if !generation_is_current {
            // Stale completion: the visible state moved on; this render's
            // pixels are out of date and must not become the cache.
            return;
        }
        match plan {
            RenderPlan::Full { .. } => {
                self.last_shown = Some(vm.clone());
                self.partials_since_maintenance = 0;
            }
            RenderPlan::Partial { .. } => {
                self.last_shown = Some(vm.clone());
                self.partials_since_maintenance = self.partials_since_maintenance.saturating_add(1);
            }
            RenderPlan::Noop => {
                // Nothing changed on screen; the cache already equals vm.
            }
        }
    }

    /// Force-invalidate the cache (used when the renderer detects a state
    /// it can no longer trust, e.g. a boot-after-failure).
    pub fn invalidate_cache(&mut self) {
        self.last_shown = None;
        self.partials_since_maintenance = 0;
    }

    /// Seed the renderer's startup cache with the surface known to remain on
    /// the panel across a deep-sleep wake. This is intentionally explicit:
    /// normal cold boot starts with no previous frame, while a maintenance
    /// wake may use the RenderPlan clock partial for the preserved Home
    /// frame.
    pub fn seed_last_shown(&mut self, view_model: ViewModel) {
        self.last_shown = Some(view_model);
        self.partials_since_maintenance = 0;
    }

    /// Apply a completed render kick's terminal outcome to the cache.
    /// Called by the EPD completion path after `feed` matched a kick. The
    /// plan is recomputed from the kick's own ViewModel against the cache
    /// (with the single EPD slot, a matched non-superseded kick is the
    /// current submission, so the recomputed plan equals the one decided at
    /// submit time).
    ///
    /// `generation_is_current` is false when the kick's render generation is
    /// older than the current visible state - its pixels are out of date and
    /// must not become the cache.
    pub fn note_kick_terminal(
        &mut self,
        kick: &AsyncKick,
        success: bool,
        generation_is_current: bool,
    ) {
        let crate::app::Effect::Render(req) = &kick.effect else {
            return;
        };
        let plan = self.plan_for(&req.view_model);
        self.note_terminal(&req.view_model, &plan, success, generation_is_current);
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

    /// Request id of the first pending render kick (tests use it to
    /// drive completions).
    pub fn first_request_id(&self) -> Option<u64> {
        self.pending.iter().find_map(|k| k.request_id)
    }

    /// All pending request ids (tests drain completions by id).
    pub fn request_ids(&self) -> Vec<u64> {
        self.pending.iter().filter_map(|k| k.request_id).collect()
    }

    /// Take all pending kicks out (tests that need the raw kicks).
    pub fn drain_all(&mut self) -> Vec<AsyncKick> {
        std::mem::take(&mut self.pending)
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
                view: crate::app::RenderView::Home,
                view_model: crate::render_plan::ViewModel::home(RenderGeneration(1)),
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

    // ---- Stage-5 renderer cache rules --------------------------------------

    fn home_vm(gen: u64, minute: Option<u32>) -> ViewModel {
        ViewModel {
            generation: RenderGeneration(gen),
            view: crate::app::RenderView::Home,
            clock_minute: minute,
            overlay: crate::render_plan::Overlay::None,
            data_fingerprint: 0,
        }
    }

    #[test]
    fn cache_empty_first_render_is_full() {
        let mut reg = RenderRegistry::new();
        let plan = reg.plan_for(&home_vm(0, None));
        assert!(matches!(plan, RenderPlan::Full { .. }));
        // Completing it (current generation) caches the VM.
        reg.note_terminal(&home_vm(0, None), &plan, true, true);
        assert_eq!(reg.last_shown.as_ref(), Some(&home_vm(0, None)));
        assert_eq!(reg.partials_since_maintenance, 0);
    }

    #[test]
    fn same_view_model_after_full_is_noop_and_cache_unchanged() {
        let mut reg = RenderRegistry::new();
        let first = home_vm(0, None);
        let p0 = reg.plan_for(&first);
        reg.note_terminal(&first, &p0, true, true);
        // Identical visible state -> Noop; cache still holds the first VM.
        let plan = reg.plan_for(&first);
        assert_eq!(plan, RenderPlan::Noop);
    }

    #[test]
    fn home_minute_change_is_clock_partial_and_counts() {
        let mut reg = RenderRegistry::new();
        let t0 = home_vm(0, Some(8 * 60));
        let p0 = reg.plan_for(&t0);
        reg.note_terminal(&t0, &p0, true, true);
        let t1 = home_vm(1, Some(8 * 60 + 1));
        let plan = reg.plan_for(&t1);
        assert_eq!(
            plan,
            RenderPlan::Partial {
                frame: crate::render_plan::Frame(1),
                region: crate::render_plan::PartialRegion::Clock
            }
        );
        reg.note_terminal(&t1, &plan, true, true);
        assert_eq!(reg.partials_since_maintenance, 1);
        assert_eq!(reg.last_shown.as_ref(), Some(&t1));
    }

    #[test]
    fn seeded_deep_wake_home_frame_uses_clock_partial() {
        let mut reg = RenderRegistry::new();
        reg.seed_last_shown(home_vm(0, Some(8 * 60)));
        let current = home_vm(1, Some(8 * 60 + 1));
        assert_eq!(
            reg.plan_for(&current),
            RenderPlan::Partial {
                frame: crate::render_plan::Frame(1),
                region: crate::render_plan::PartialRegion::Clock,
            }
        );
    }

    #[test]
    fn failed_completion_invalidates_cache_forcing_full() {
        let mut reg = RenderRegistry::new();
        let t0 = home_vm(0, None);
        let p0 = reg.plan_for(&t0);
        reg.note_terminal(&t0, &p0, true, true);
        assert!(reg.last_shown.is_some());
        // A later Partial fails -> cache invalidated.
        let t1 = home_vm(1, Some(9 * 60));
        let plan = reg.plan_for(&t1);
        reg.note_terminal(&t1, &plan, false, true);
        assert!(reg.last_shown.is_none());
        assert_eq!(reg.partials_since_maintenance, 0);
        // Next render has no previous -> Full (recovery).
        assert!(matches!(reg.plan_for(&t1), RenderPlan::Full { .. }));
    }

    #[test]
    fn stale_generation_completion_does_not_update_cache() {
        let mut reg = RenderRegistry::new();
        let t0 = home_vm(0, Some(8 * 60));
        let p0 = reg.plan_for(&t0);
        reg.note_terminal(&t0, &p0, true, true);
        // A Full for generation 1 completes AFTER state moved to
        // generation 5: not current -> cache keeps gen-0 view.
        let stale = home_vm(1, Some(10 * 60));
        let plan = RenderPlan::Full {
            frame: crate::render_plan::Frame(1),
        };
        reg.note_terminal(&stale, &plan, true, false);
        assert_eq!(reg.last_shown.as_ref(), Some(&t0));
        assert_eq!(reg.partials_since_maintenance, 0);
    }

    #[test]
    fn maintenance_threshold_forces_full_and_resets_on_full() {
        let mut reg = RenderRegistry::new();
        let t0 = home_vm(0, Some(8 * 60));
        let p0 = reg.plan_for(&t0);
        reg.note_terminal(&t0, &p0, true, true);
        // Push LIMIT successful partials (each minute change).
        for gen in 1..=crate::render_plan::PARTIAL_MAINTENANCE_LIMIT {
            let t = home_vm(u64::from(gen), Some(8 * 60 + gen));
            let plan = reg.plan_for(&t);
            assert!(matches!(plan, RenderPlan::Partial { .. }));
            reg.note_terminal(&t, &plan, true, true);
        }
        assert_eq!(
            reg.partials_since_maintenance,
            crate::render_plan::PARTIAL_MAINTENANCE_LIMIT
        );
        // The next minute change is past the limit -> Full.
        let next = home_vm(
            u64::from(crate::render_plan::PARTIAL_MAINTENANCE_LIMIT + 1),
            Some(8 * 60 + crate::render_plan::PARTIAL_MAINTENANCE_LIMIT + 1),
        );
        let plan = reg.plan_for(&next);
        assert!(matches!(plan, RenderPlan::Full { .. }));
        reg.note_terminal(&next, &plan, true, true);
        assert_eq!(reg.partials_since_maintenance, 0);
    }

    #[test]
    fn page_type_change_is_full_and_updates_cache() {
        let mut reg = RenderRegistry::new();
        let home = home_vm(0, Some(8 * 60));
        let p0 = reg.plan_for(&home);
        reg.note_terminal(&home, &p0, true, true);
        let settings = ViewModel {
            generation: RenderGeneration(1),
            view: crate::app::RenderView::Settings { selected: 0 },
            clock_minute: Some(8 * 60),
            overlay: crate::render_plan::Overlay::None,
            data_fingerprint: 0,
        };
        let plan = reg.plan_for(&settings);
        assert!(matches!(plan, RenderPlan::Full { .. }));
        reg.note_terminal(&settings, &plan, true, true);
        assert_eq!(reg.last_shown.as_ref(), Some(&settings));
    }
}
