use crate::app::{Effect, RenderGeneration, RenderView};
use crate::render_plan::{plan_render, RenderPlan, ViewModel};
use crate::runner::AsyncKick;

pub const RENDER_REGISTRY_CAPACITY: usize = 16;

pub fn retry_replacement_index(pending: &[AsyncKick], incoming: &AsyncKick) -> Option<usize> {
    let (incoming_generation, incoming_view) = render_key(incoming)?;
    let same_view = pending
        .iter()
        .enumerate()
        .filter_map(|(index, kick)| {
            let (generation, view) = render_key(kick)?;
            (view == incoming_view && generation.0 <= incoming_generation.0)
                .then_some((index, generation))
        })
        .min_by_key(|(_, generation)| generation.0);
    same_view.map(|(index, _)| index).or_else(|| {
        pending
            .iter()
            .enumerate()
            .filter_map(|(index, kick)| {
                let (generation, _) = render_key(kick)?;
                (generation.0 < incoming_generation.0).then_some((index, generation))
            })
            .min_by_key(|(_, generation)| generation.0)
            .map(|(index, _)| index)
    })
}

fn render_key(kick: &AsyncKick) -> Option<(RenderGeneration, &RenderView)> {
    let Effect::Render(request) = &kick.effect else {
        return None;
    };
    Some((
        kick.render_generation.unwrap_or(request.generation),
        &request.view,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderTerminal {
    Completed,
    Failed,
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum FeedOutcome {
    Matched(AsyncKick, RenderTerminal),

    Ignored,
}

#[derive(Debug)]
pub struct RenderRegistry {
    pub pending: Vec<AsyncKick>,

    pub last_shown: Option<ViewModel>,
}

impl Default for RenderRegistry {
    fn default() -> Self {
        Self {
            pending: Vec::with_capacity(RENDER_REGISTRY_CAPACITY),
            last_shown: None,
        }
    }
}

impl RenderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn plan_for(&self, vm: &ViewModel) -> RenderPlan {
        plan_render(self.last_shown.as_ref(), vm)
    }

    pub fn note_terminal(
        &mut self,
        vm: &ViewModel,
        plan: &RenderPlan,
        success: bool,
        generation_is_current: bool,
    ) {
        if !success {
            self.last_shown = None;
            return;
        }
        if !generation_is_current {
            return;
        }
        match plan {
            RenderPlan::Full { .. } => {
                self.last_shown = Some(vm.clone());
            }
            RenderPlan::Partial { .. } => {
                self.last_shown = Some(vm.clone());
            }
            RenderPlan::Noop => {}
        }
    }

    pub fn invalidate_cache(&mut self) {
        self.last_shown = None;
    }

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

    pub fn register(&mut self, kick: AsyncKick) -> Result<(), Box<AsyncKick>> {
        if self.pending.len() >= RENDER_REGISTRY_CAPACITY {
            return Err(Box::new(kick));
        }
        self.pending.push(kick);
        Ok(())
    }

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

    pub fn supersede_then_register(
        &mut self,
        replaced_id: u64,
        replacement: AsyncKick,
    ) -> Result<(), Box<AsyncKick>> {
        self.feed(replaced_id, false, true);
        self.register(replacement)
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn first_request_id(&self) -> Option<u64> {
        self.pending.iter().find_map(|k| k.request_id)
    }

    pub fn request_ids(&self) -> Vec<u64> {
        self.pending.iter().filter_map(|k| k.request_id).collect()
    }

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
        reg.register(kick(1)).unwrap();

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
        reg.register(kick(1)).unwrap();

        reg.supersede_then_register(1, kick(2)).unwrap();
        assert_eq!(reg.len(), 1);

        assert_eq!(reg.feed(1, true, false), FeedOutcome::Ignored);

        assert!(matches!(
            reg.feed(2, true, false),
            FeedOutcome::Matched(_, RenderTerminal::Completed)
        ));
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn failed_completion_is_terminal() {
        let mut reg = RenderRegistry::new();
        reg.register(kick(7)).unwrap();
        assert!(matches!(
            reg.feed(7, false, false),
            FeedOutcome::Matched(_, RenderTerminal::Failed)
        ));
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn full_registry_returns_the_owned_kick_for_retry() {
        let mut reg = RenderRegistry::new();
        for request_id in 0..RENDER_REGISTRY_CAPACITY as u64 {
            reg.register(kick(request_id)).unwrap();
        }
        let rejected = reg.register(kick(999)).unwrap_err();
        assert_eq!(rejected.request_id, Some(999));
        assert_eq!(reg.len(), RENDER_REGISTRY_CAPACITY);
        assert!(reg.feed(0, false, true) != FeedOutcome::Ignored);
        reg.register(*rejected).unwrap();
        assert_eq!(reg.len(), RENDER_REGISTRY_CAPACITY);
    }

    #[test]
    fn retry_replacement_keeps_newer_generation_and_surface() {
        let mut old = kick(1);
        old.render_generation = Some(RenderGeneration(3));
        if let Effect::Render(request) = &mut old.effect {
            request.generation = RenderGeneration(3);
        }
        let mut newer = kick(2);
        newer.render_generation = Some(RenderGeneration(4));
        if let Effect::Render(request) = &mut newer.effect {
            request.generation = RenderGeneration(4);
        }
        assert_eq!(retry_replacement_index(&[old.clone()], &newer), Some(0));

        let mut stale = kick(3);
        stale.render_generation = Some(RenderGeneration(2));
        if let Effect::Render(request) = &mut stale.effect {
            request.generation = RenderGeneration(2);
        }
        assert_eq!(retry_replacement_index(&[newer], &stale), None);

        let mut different_surface = kick(4);
        different_surface.render_generation = Some(RenderGeneration(5));
        if let Effect::Render(request) = &mut different_surface.effect {
            request.generation = RenderGeneration(5);
            request.view = RenderView::Settings { selected: 0 };
        }
        assert_eq!(retry_replacement_index(&[old], &different_surface), Some(0));
    }

    #[test]
    fn unknown_completion_is_ignored() {
        let mut reg = RenderRegistry::new();
        assert_eq!(reg.feed(999, true, false), FeedOutcome::Ignored);
        assert_eq!(reg.feed(999, false, true), FeedOutcome::Ignored);
    }

    fn home_vm(gen: u64, minute: Option<u32>) -> ViewModel {
        ViewModel {
            generation: RenderGeneration(gen),
            view: crate::app::RenderView::Home,
            clock_minute: minute,
            overlay: crate::render_plan::Overlay::None,
            data_fingerprint: 0,
        }
    }

    fn nav_vm(gen: u64, selected: usize, source: crate::app::RenderView) -> ViewModel {
        ViewModel {
            generation: RenderGeneration(gen),
            view: crate::app::RenderView::Navigation {
                selected,
                underlying: Box::new(source),
            },
            clock_minute: Some(8 * 60),
            overlay: crate::render_plan::Overlay::None,
            data_fingerprint: 0,
        }
    }

    #[test]
    fn cache_empty_first_render_is_full() {
        let mut reg = RenderRegistry::new();
        let plan = reg.plan_for(&home_vm(0, None));
        assert!(matches!(plan, RenderPlan::Full { .. }));

        reg.note_terminal(&home_vm(0, None), &plan, true, true);
        assert_eq!(reg.last_shown.as_ref(), Some(&home_vm(0, None)));
    }

    #[test]
    fn unknown_startup_cache_is_full_for_cold_and_deep_wake() {
        for minute in [Some(8 * 60), Some(8 * 60 + 1)] {
            let reg = RenderRegistry::new();
            assert!(matches!(
                reg.plan_for(&home_vm(0, minute)),
                RenderPlan::Full { .. }
            ));
        }
    }

    #[test]
    fn same_view_model_after_full_is_noop_and_cache_unchanged() {
        let mut reg = RenderRegistry::new();
        let first = home_vm(0, None);
        let p0 = reg.plan_for(&first);
        reg.note_terminal(&first, &p0, true, true);

        let plan = reg.plan_for(&first);
        assert_eq!(plan, RenderPlan::Noop);
    }

    #[test]
    fn home_minute_change_is_clock_partial() {
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
        assert_eq!(reg.last_shown.as_ref(), Some(&t1));
    }

    #[test]
    fn nav_bar_partial_completion_caches_current_view_for_next_diff() {
        let mut reg = RenderRegistry::new();
        let home = home_vm(0, Some(8 * 60));
        let first = reg.plan_for(&home);
        reg.note_terminal(&home, &first, true, true);

        let open = nav_vm(1, 0, crate::app::RenderView::Home);
        let open_plan = reg.plan_for(&open);
        assert_eq!(
            open_plan,
            RenderPlan::Partial {
                frame: crate::render_plan::Frame(1),
                region: crate::render_plan::PartialRegion::NavBar,
            }
        );
        reg.note_terminal(&open, &open_plan, true, true);
        assert_eq!(reg.last_shown.as_ref(), Some(&open));

        let moved = nav_vm(2, 3, crate::app::RenderView::Home);
        let moved_plan = reg.plan_for(&moved);
        assert!(matches!(
            moved_plan,
            RenderPlan::Partial {
                region: crate::render_plan::PartialRegion::NavBar,
                ..
            }
        ));
        reg.note_terminal(&moved, &moved_plan, true, true);
        assert_eq!(reg.last_shown.as_ref(), Some(&moved));

        let close = home_vm(3, Some(8 * 60));
        let close_plan = reg.plan_for(&close);
        assert!(matches!(
            close_plan,
            RenderPlan::Partial {
                region: crate::render_plan::PartialRegion::NavBar,
                ..
            }
        ));
    }

    #[test]
    fn failed_completion_invalidates_cache_forcing_full() {
        let mut reg = RenderRegistry::new();
        let t0 = home_vm(0, None);
        let p0 = reg.plan_for(&t0);
        reg.note_terminal(&t0, &p0, true, true);
        assert!(reg.last_shown.is_some());

        let t1 = home_vm(1, Some(9 * 60));
        let plan = reg.plan_for(&t1);
        reg.note_terminal(&t1, &plan, false, true);
        assert!(reg.last_shown.is_none());

        assert!(matches!(reg.plan_for(&t1), RenderPlan::Full { .. }));
    }

    #[test]
    fn stale_generation_completion_does_not_update_cache() {
        let mut reg = RenderRegistry::new();
        let t0 = home_vm(0, Some(8 * 60));
        let p0 = reg.plan_for(&t0);
        reg.note_terminal(&t0, &p0, true, true);

        let stale = home_vm(1, Some(10 * 60));
        let plan = RenderPlan::Full {
            frame: crate::render_plan::Frame(1),
        };
        reg.note_terminal(&stale, &plan, true, false);
        assert_eq!(reg.last_shown.as_ref(), Some(&t0));
    }

    #[test]
    fn home_minute_changes_stay_partial_without_maintenance_full() {
        let mut reg = RenderRegistry::new();
        let t0 = home_vm(0, Some(8 * 60));
        let p0 = reg.plan_for(&t0);
        reg.note_terminal(&t0, &p0, true, true);
        for gen in 1..=256 {
            let t = home_vm(u64::from(gen), Some(8 * 60 + gen));
            let plan = reg.plan_for(&t);
            assert!(matches!(plan, RenderPlan::Partial { .. }));
            reg.note_terminal(&t, &plan, true, true);
        }
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
