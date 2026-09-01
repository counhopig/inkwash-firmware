//! Client side of the EPD subsystem: the shared 1bpp
//! canvas and the request slot into the dedicated EPD task, which owns
//! the FFI driver. Every refresh is asynchronous - drawing into the canvas
//! is never blocked by a panel update, and the main loop never waits on a
//! refresh.
//!
//! The canvas lives on the app thread only. A refresh request snapshots
//! the whole frame under the canvas lock at request time, so the EPD task
//! never reads the canvas and a pending refresh is
//! immune to later drawing. Requests go through a single latest-wins slot,
//! and partial-vs-full promotion is decided here,
//! keeping the refresh policy in one place.

use anyhow::Result;
use parking_lot::Mutex;
use parking_lot::MutexGuard;

use crate::board::ChargeSnapshot;
use crate::canvas::Canvas;
use crate::epd_task::{self, EpdCompletion, EpdHandle};
use crate::home;
use crate::rtc::DateTime;

pub use crate::canvas::Rect;

/// Consecutive partial refreshes before the scheduler promotes the next
/// one to a full refresh (matching the reference demo's
/// ghosting guard: 8 UI partials then a full refresh).
const PARTIALS_BEFORE_FULL: u32 = 8;

/// Main-thread handle to the EPD subsystem: a lockable view of the frame
/// buffer plus the request slot into [`crate::epd_task`].
pub struct EpdClient {
    canvas: Mutex<Canvas>,
    handle: EpdHandle,
    /// Partial refreshes executed since the last full refresh; drives the
    /// full-refresh promotion.
    partials_since_full: u32,
}

impl EpdClient {
    /// Initializes the EPD driver (hardware faults propagate) and spawns
    /// the refresh task.
    pub fn new() -> Result<Self> {
        let canvas = Mutex::new(Canvas::new());
        let handle = epd_task::spawn()?;
        Ok(Self {
            canvas,
            handle,
            partials_since_full: 0,
        })
    }

    /// Direct canvas access for screens that don't fit the fixed
    /// `render_home` layout, e.g. the navigation drawer and `screens.rs`.
    /// The returned guard is the frame buffer; the refresh request issued
    /// after drawing snapshots it, so what was just drawn is what the
    /// panel shows.
    pub fn canvas_mut(&mut self) -> MutexGuard<'_, Canvas> {
        self.canvas.lock()
    }

    /// The idle/background screen: clock, Wi-Fi/battery status, next-alarm
    /// summary (time, countdown), and a todos summary (open count,
    /// due-today count). `main.rs` redraws this after returning from any
    /// modal screen (the navigation drawer, settings menu, alarm ring).
    /// Layout lives in `home::render` so the same pixels can be previewed
    /// on a PC; this only hands it the canvas.
    #[allow(clippy::too_many_arguments)]
    pub fn render_home(
        &mut self,
        clock: Option<&DateTime>,
        next_alarm_time: Option<&str>,
        next_alarm_date: Option<&str>,
        next_alarm_days_left: Option<i64>,
        todo_pending: usize,
        todo_due_today: usize,
        unread_inbox: usize,
        wifi_configured: bool,
        battery_percent: Option<u8>,
        charge: ChargeSnapshot,
    ) {
        let mut canvas = self.canvas.lock();
        home::render(
            &mut canvas,
            clock,
            next_alarm_time,
            next_alarm_date,
            next_alarm_days_left,
            todo_pending,
            todo_due_today,
            unread_inbox,
            wifi_configured,
            battery_percent,
            charge,
        );
    }

    /// Queues a full-screen refresh of the current canvas contents; the
    /// frame is snapshotted now, so the panel shows exactly this frame.
    /// Resets the partial-refresh promotion counter.
    pub fn refresh_full(&mut self) -> Result<u64> {
        let frame = self.canvas.lock().frame().to_vec().into_boxed_slice();
        let id = self.handle.request_full(frame);
        self.partials_since_full = 0;
        Ok(id)
    }

    /// Always refreshes only `rect`, never promotes to a full refresh.
    /// The RTC is sampled frequently but the visible home clock refreshes
    /// only when its displayed minute changes; boot and alarm-ring still use
    /// `refresh_full` explicitly. Callers re-render the whole canvas
    /// before refreshing, so the partial rect always shows fresh pixels.
    /// The scheduler merges consecutive partials into one pending request
    /// and promotes to a full refresh after [`PARTIALS_BEFORE_FULL`] of
    /// them; `refresh_full` is for the deliberate full
    /// refreshes (boot, alarm ring).
    pub fn refresh_partial(&mut self, rect: Rect) -> Result<u64> {
        if self.partials_since_full >= PARTIALS_BEFORE_FULL {
            self.partials_since_full = 0;
            log::info!("{PARTIALS_BEFORE_FULL} consecutive partial refreshes; promoting to full");
            return self.refresh_full();
        }
        let frame = self.canvas.lock().frame().to_vec().into_boxed_slice();
        let id = self.handle.request_partial(rect, frame);
        self.partials_since_full += 1;
        Ok(id)
    }

    /// UI screens are best-effort callers: a display fault must not silently
    /// disappear, but it also must not stop buttons, alarms, or the watchdog.
    /// The refresh itself (and any failure recovery) happens in the EPD
    /// task.
    pub fn refresh_full_best_effort(&mut self) {
        if let Err(err) = self.refresh_full() {
            log::error!("EPD full refresh request failed: {err}");
        }
    }

    /// Logs a request-send failure. The panel-level recovery for a failed
    /// partial refresh (one full refresh from the same snapshot) lives in
    /// the EPD task, where the FFI result is known.
    pub fn refresh_partial_best_effort(&mut self, rect: Rect) {
        if let Err(err) = self.refresh_partial(rect) {
            log::error!("EPD partial refresh request failed: {err}");
        }
    }

    /// Non-blocking drain of completed refreshes: the app
    /// state machine observes each refresh's success/failure/recovery here.
    pub fn poll_completion(&mut self) -> Option<EpdCompletion> {
        self.handle.poll_completion()
    }
}
