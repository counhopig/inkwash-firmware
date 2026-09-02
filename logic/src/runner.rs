//! Generic application-state runner: drives `app::update` and executes
//! each returned `EffectBatch` through a caller-provided executor trait.
//!
//! This is the host-testable core of the firmware's `AppRunner`. The
//! state machine (`app::update`) is pure; the batch-execution engine
//! (id allocation, AbortBatch semantics, synchronous completion feedback,
//! async-kick collection, render-completion routing) is also pure, so it
//! lives here where a host test can drive it with a fake executor. The
//! firmware crate provides a real executor (`EffectRunner`) that runs
//! each `Effect` against `DeviceContext`'s drivers; a host harness
//! provides a fake one that records calls and scripts failures.

use crate::app::{
    self, AppState, Effect, EffectBatch, EffectBatchId, EffectCompletion, EffectError,
    EffectFailure, EffectId, EffectOutput, Event, FailurePolicy, OperationId, RenderGeneration,
};

/// Per-category failure classification, mirroring the state machine's
/// `EffectError` variants. The executor reports which category an
/// `anyhow`-style failure belongs to so the runner can map it to the
/// exact `EffectError` (ACK vs Persist vs Rtc vs Render vs Sync vs Tone
/// vs Sleep) and the state machine schedules the right retry path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EffectCategory {
    Ack,
    Persist,
    Rtc,
    Render,
    Sync,
    Tone,
    Sleep,
}

/// Outcome of running one `Effect` in the executor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectOutcome {
    /// The effect completed synchronously; feed `EffectCompleted` back.
    Completed(EffectOutput),
    /// The effect was kicked off asynchronously (no request id yet).
    Async,
    /// The effect started an async operation carrying a request id (e.g.
    /// an EPD refresh) that a later completion will echo back.
    AsyncWithId(u64),
}

/// A side-effect request the dispatch could not complete synchronously.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsyncKick {
    pub batch_id: EffectBatchId,
    pub effect_id: EffectId,
    pub operation_id: OperationId,
    pub render_generation: Option<RenderGeneration>,
    pub effect: Effect,
    /// EPD request id echoed by the matching completion, when the kick is
    /// a Render whose panel refresh already started.
    pub request_id: Option<u64>,
}

impl AsyncKick {
    pub fn is_render(&self) -> bool {
        matches!(self.effect, Effect::Render(_))
    }
}

/// Executes one `Effect` against whatever the caller supplies. The
/// firmware implementation drives `DeviceContext`; a host harness drives
/// a recording fake. The error is returned as a category plus message so
/// the runner can build the exact `EffectError`.
pub trait EffectExecutor {
    fn run(&mut self, effect: &Effect) -> Result<EffectOutcome, (EffectCategory, String)>;
}

/// Owns the `AppState` and drives every event through `app::update`,
/// executing batches via a caller-provided `EffectExecutor`.
///
/// `dispatch` takes `&mut E` so the firmware can pass a fresh
/// `EffectRunner` borrowing `DeviceContext` (constructed per batch with
/// the current clock) while the host harness passes a `FakeExecutor`.
#[derive(Debug)]
pub struct AppRunner {
    state: AppState,
    /// Most recent RTC read - threaded into `Effect::Render` so the home
    /// screen does not need a fresh I2C transaction for each refresh. The
    /// firmware executor reads this when constructing its per-batch
    /// runner.
    last_clock: Option<crate::datetime::DateTime>,
    /// In-flight kicks accumulated by `dispatch` / `on_effect_completed` /
    /// `on_effect_failed`; drained via `take_pending_kicks`.
    pending_kicks: Vec<AsyncKick>,
}

impl AppRunner {
    pub fn new() -> Self {
        Self {
            state: AppState::default(),
            last_clock: None,
            pending_kicks: Vec::new(),
        }
    }

    pub fn from_state(state: AppState) -> Self {
        Self {
            state,
            last_clock: None,
            pending_kicks: Vec::new(),
        }
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut AppState {
        &mut self.state
    }

    pub fn set_last_clock(&mut self, clock: Option<crate::datetime::DateTime>) {
        self.last_clock = clock;
    }

    pub fn last_clock(&self) -> Option<crate::datetime::DateTime> {
        self.last_clock
    }

    /// Dispatch a single event through the state machine and run every
    /// returned effect batch against `executor`. Async kicks are stashed
    /// in `self.pending_kicks`; callers drain them via
    /// `take_pending_kicks`.
    pub fn dispatch<E: EffectExecutor>(
        &mut self,
        event: Event,
        executor: &mut E,
    ) -> Result<(), String> {
        let batches = app::update(&mut self.state, event);
        for batch in batches {
            self.run_batch(batch, executor)?;
        }
        Ok(())
    }

    /// Drain the queue of in-flight asynchronous kicks.
    pub fn take_pending_kicks(&mut self) -> Vec<AsyncKick> {
        std::mem::take(&mut self.pending_kicks)
    }

    fn run_batch<E: EffectExecutor>(
        &mut self,
        batch: EffectBatch,
        executor: &mut E,
    ) -> Result<(), String> {
        // EffectId allocation: stable, position-based within the batch so
        // completion/failure events correlate precisely. The
        // `batch.failure_policy` decides whether to keep going after a
        // failure: `AbortBatch` stops the rest of the batch on the first
        // failure; `Continue` runs every effect regardless.
        for (idx, effect) in batch.effects.iter().cloned().enumerate() {
            let effect_id = EffectId(idx as u64 + 1);
            let outcome = executor.run(&effect);
            match outcome {
                Ok(EffectOutcome::Completed(output)) => {
                    let completion = EffectCompletion {
                        batch_id: batch.id,
                        effect_id,
                        operation_id: batch.operation_id,
                        render_generation: batch.render_generation,
                        output,
                    };
                    let chained = app::update(&mut self.state, Event::EffectCompleted(completion));
                    for next in chained {
                        self.run_batch(next, executor)?;
                    }
                }
                Ok(EffectOutcome::AsyncWithId(request_id)) => self.pending_kicks.push(AsyncKick {
                    batch_id: batch.id,
                    effect_id,
                    operation_id: batch.operation_id,
                    render_generation: batch.render_generation,
                    effect,
                    request_id: Some(request_id),
                }),
                Ok(EffectOutcome::Async) => self.pending_kicks.push(AsyncKick {
                    batch_id: batch.id,
                    effect_id,
                    operation_id: batch.operation_id,
                    render_generation: batch.render_generation,
                    effect,
                    request_id: None,
                }),
                Err((category, msg)) => {
                    let failure = EffectFailure {
                        batch_id: batch.id,
                        effect_id,
                        operation_id: batch.operation_id,
                        render_generation: batch.render_generation,
                        error: err_for_category(category, &msg),
                    };
                    let chained = app::update(&mut self.state, Event::EffectFailed(failure));
                    for next in chained {
                        self.run_batch(next, executor)?;
                    }
                    if batch.failure_policy == FailurePolicy::AbortBatch {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// Feed an externally-observed completion back into the state machine.
    pub fn on_effect_completed<E: EffectExecutor>(
        &mut self,
        completion: EffectCompletion,
        executor: &mut E,
    ) -> Result<(), String> {
        let batches = app::update(&mut self.state, Event::EffectCompleted(completion));
        for batch in batches {
            self.run_batch(batch, executor)?;
        }
        Ok(())
    }

    /// Feed an externally-observed failure back into the state machine.
    pub fn on_effect_failed<E: EffectExecutor>(
        &mut self,
        failure: EffectFailure,
        executor: &mut E,
    ) -> Result<(), String> {
        let batches = app::update(&mut self.state, Event::EffectFailed(failure));
        for batch in batches {
            self.run_batch(batch, executor)?;
        }
        Ok(())
    }

    /// Feed a render completion (success or failure) back into the state
    /// machine via the matching in-flight `AsyncKick`. Returns
    /// immediately if the kick carries a non-render effect.
    pub fn feed_render_completion<E: EffectExecutor>(
        &mut self,
        kick: AsyncKick,
        output: EffectOutput,
        failure: Option<EffectError>,
        executor: &mut E,
    ) -> Result<(), String> {
        if !kick.is_render() {
            return Ok(());
        }
        let chained = match failure {
            Some(err) => {
                let failure = EffectFailure {
                    batch_id: kick.batch_id,
                    effect_id: kick.effect_id,
                    operation_id: kick.operation_id,
                    render_generation: kick.render_generation,
                    error: err,
                };
                app::update(&mut self.state, Event::EffectFailed(failure))
            }
            None => {
                let completion = EffectCompletion {
                    batch_id: kick.batch_id,
                    effect_id: kick.effect_id,
                    operation_id: kick.operation_id,
                    render_generation: kick.render_generation,
                    output,
                };
                app::update(&mut self.state, Event::EffectCompleted(completion))
            }
        };
        for batch in chained {
            self.run_batch(batch, executor)?;
        }
        Ok(())
    }
}

impl Default for AppRunner {
    fn default() -> Self {
        Self::new()
    }
}

/// Maps a per-category failure into the state machine's `EffectError`.
pub fn err_for_category(category: EffectCategory, msg: &str) -> EffectError {
    match category {
        EffectCategory::Ack => EffectError::Ack(msg.to_string()),
        EffectCategory::Persist => EffectError::Persist(msg.to_string()),
        EffectCategory::Rtc => EffectError::Rtc(msg.to_string()),
        EffectCategory::Render => EffectError::Render(msg.to_string()),
        EffectCategory::Sync => EffectError::Sync(msg.to_string()),
        EffectCategory::Tone => EffectError::Tone(msg.to_string()),
        EffectCategory::Sleep => EffectError::Sleep(msg.to_string()),
    }
}
