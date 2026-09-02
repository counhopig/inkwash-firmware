//! Effect-execution vocabulary shared by the application runtime and its
//! executors (host fakes and the firmware's real driver executor).
//!
//! `app::update` is pure; the single-consumer `Runtime` (`runtime` module)
//! owns `AppState`, the event queue, and batch execution. This module
//! keeps only what an executor needs: the per-category failure
//! classification, the outcome of running one effect, the async-kick
//! record, and the category-to-error mapping. The firmware crate provides
//! the real executor (`EffectRunner`) that runs each `Effect` against
//! `DeviceContext`'s drivers; a host harness provides a fake one that
//! records calls and scripts failures.

use crate::app::{
    Effect, EffectBatchId, EffectError, EffectId, EffectOutput, OperationId, RenderGeneration,
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
