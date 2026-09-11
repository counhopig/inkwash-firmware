use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use parking_lot::MutexGuard;

use crate::board::ChargeSnapshot;
use crate::canvas::Canvas;
use crate::epd_task::{self, EpdCompletion, EpdHandle};
use crate::home;
use crate::rtc::DateTime;

pub use crate::canvas::Rect;

#[derive(Clone)]
pub struct EpdClient {
    canvas: Arc<Mutex<Canvas>>,
    handle: EpdHandle,
}

impl EpdClient {
    pub fn new() -> Result<Self> {
        let canvas = Arc::new(Mutex::new(Canvas::new()));
        let handle = epd_task::spawn()?;
        Ok(Self { canvas, handle })
    }

    pub fn canvas_mut(&self) -> MutexGuard<'_, Canvas> {
        self.canvas.lock()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render_home(
        &self,
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

    pub fn refresh_full(&self) -> Result<u64> {
        let frame = self.canvas.lock().frame().to_vec().into_boxed_slice();
        self.handle.request_full(frame)
    }

    pub fn refresh_partial(&self, rect: Rect) -> Result<u64> {
        let frame = self.canvas.lock().frame().to_vec().into_boxed_slice();
        self.handle.request_partial(rect, frame)
    }

    pub fn poll_completion(&self) -> Option<EpdCompletion> {
        self.handle.poll_completion()
    }
}
