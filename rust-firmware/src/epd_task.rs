use std::collections::VecDeque;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;

use parking_lot::{Condvar, Mutex};

use anyhow::{bail, Result};
use esp_idf_svc::sys::zectrix_epd::{
    zectrix_epd_config_t, zectrix_epd_del, zectrix_epd_get_default_config, zectrix_epd_handle_t,
    zectrix_epd_new, zectrix_epd_power_off, zectrix_epd_power_on, zectrix_epd_rect_t,
    zectrix_epd_refresh_full_1bpp, zectrix_epd_refresh_partial_1bpp,
};

use crate::canvas::{self, Rect};

const COMPLETION_CAPACITY: usize = 16;

struct CompletionState {
    queue: VecDeque<EpdCompletion>,
    reserved: usize,
}

struct CompletionMailbox {
    state: Mutex<CompletionState>,
    available: Condvar,
}

impl CompletionMailbox {
    fn new() -> Self {
        Self {
            state: Mutex::new(CompletionState {
                queue: VecDeque::with_capacity(COMPLETION_CAPACITY),
                reserved: 0,
            }),
            available: Condvar::new(),
        }
    }

    fn reserve(&self) -> bool {
        let mut state = self.state.lock();
        if state.queue.len() + state.reserved >= COMPLETION_CAPACITY {
            return false;
        }
        state.reserved += 1;
        true
    }

    fn complete_reserved(&self, completion: EpdCompletion) {
        let mut state = self.state.lock();
        debug_assert!(state.reserved > 0);
        state.reserved = state.reserved.saturating_sub(1);
        state.queue.push_back(completion);
        self.available.notify_one();
    }

    fn send(&self, completion: EpdCompletion) {
        let mut state = self.state.lock();
        while state.queue.len() + state.reserved >= COMPLETION_CAPACITY {
            self.available.wait(&mut state);
        }
        state.queue.push_back(completion);
        self.available.notify_one();
    }

    fn try_recv(&self) -> Option<EpdCompletion> {
        let mut state = self.state.lock();
        let completion = state.queue.pop_front();
        if completion.is_some() {
            self.available.notify_one();
        }
        completion
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshKind {
    Full,
    Partial(Rect),
}

pub struct RenderCommand {
    pub kind: RefreshKind,
    pub frame: Box<[u8]>,

    pub request_id: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct EpdCompletion {
    pub kind: RefreshKind,
    pub request_id: u64,

    pub ok: bool,

    pub recovered: bool,

    pub superseded: bool,
}

struct RefreshSlot {
    pending: Mutex<Option<RenderCommand>>,

    notify_tx: SyncSender<()>,

    completions: Arc<CompletionMailbox>,
}

impl RefreshSlot {
    fn submit_partial(&self, rect: Rect, frame: Box<[u8]>, request_id: u64) -> Result<()> {
        let mut pending = self.pending.lock();
        let mut superseded = None;
        match pending.as_mut() {
            Some(cmd) => match &mut cmd.kind {
                RefreshKind::Full => {
                    if !self.completions.reserve() {
                        bail!("EPD completion mailbox full; retry partial refresh");
                    }
                    superseded =
                        Some(self.superseded_completion(cmd.request_id, RefreshKind::Full));
                    cmd.request_id = request_id;
                    cmd.frame = frame;
                }
                RefreshKind::Partial(prev) => {
                    if !self.completions.reserve() {
                        bail!("EPD completion mailbox full; retry partial refresh");
                    }
                    superseded = Some(
                        self.superseded_completion(cmd.request_id, RefreshKind::Partial(*prev)),
                    );
                    cmd.request_id = request_id;
                    *prev = union_rect(*prev, rect);
                    cmd.frame = frame;
                }
            },
            None => {
                *pending = Some(RenderCommand {
                    kind: RefreshKind::Partial(rect),
                    frame,
                    request_id,
                });
            }
        }
        let _ = self.notify_tx.try_send(());
        drop(pending);
        if let Some(completion) = superseded {
            self.completions.complete_reserved(completion);
        }
        Ok(())
    }

    fn submit_full(&self, frame: Box<[u8]>, request_id: u64) -> Result<()> {
        let mut pending = self.pending.lock();
        let superseded = if let Some(cmd) = pending.as_ref() {
            if !self.completions.reserve() {
                bail!("EPD completion mailbox full; retry full refresh");
            }
            Some(self.superseded_completion(cmd.request_id, cmd.kind))
        } else {
            None
        };
        *pending = Some(RenderCommand {
            kind: RefreshKind::Full,
            frame,
            request_id,
        });
        let _ = self.notify_tx.try_send(());
        drop(pending);
        if let Some(completion) = superseded {
            self.completions.complete_reserved(completion);
        }
        Ok(())
    }

    fn superseded_completion(&self, request_id: u64, kind: RefreshKind) -> EpdCompletion {
        EpdCompletion {
            kind,
            request_id,
            ok: true,
            recovered: false,
            superseded: true,
        }
    }
}

#[derive(Clone)]
pub struct EpdHandle {
    slot: Arc<RefreshSlot>,
    completions: Arc<CompletionMailbox>,

    next_id: Arc<std::sync::atomic::AtomicU32>,
}

impl EpdHandle {
    pub fn request_partial(&self, rect: Rect, frame: Box<[u8]>) -> Result<u64> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u64;
        self.slot.submit_partial(rect, frame, id)?;
        Ok(id)
    }

    pub fn request_full(&self, frame: Box<[u8]>) -> Result<u64> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u64;
        self.slot.submit_full(frame, id)?;
        Ok(id)
    }

    pub fn poll_completion(&self) -> Option<EpdCompletion> {
        self.completions.try_recv()
    }
}

const EPD_TASK_STACK: usize = 12 * 1024;

pub fn spawn() -> Result<EpdHandle> {
    let driver = EpdDriver(init_driver()?);

    let (notify_tx, notify_rx) = sync_channel(1);
    let completions = Arc::new(CompletionMailbox::new());
    let slot = Arc::new(RefreshSlot {
        pending: Mutex::new(None),
        notify_tx,
        completions: Arc::clone(&completions),
    });
    let task_slot = Arc::clone(&slot);
    let task_completions = Arc::clone(&completions);
    std::thread::Builder::new()
        .name("epd".to_string())
        .stack_size(EPD_TASK_STACK)
        .spawn(move || run(driver, task_slot, notify_rx, task_completions))?;
    Ok(EpdHandle {
        slot,
        completions,
        next_id: Arc::new(std::sync::atomic::AtomicU32::new(1)),
    })
}

struct EpdDriver(zectrix_epd_handle_t);

unsafe impl Send for EpdDriver {}

fn init_driver() -> Result<zectrix_epd_handle_t> {
    let mut config = unsafe { std::mem::zeroed::<zectrix_epd_config_t>() };
    unsafe { zectrix_epd_get_default_config(&mut config) };

    let mut handle: zectrix_epd_handle_t = std::ptr::null_mut();
    check_epd("initialize official Zectrix EPD driver", unsafe {
        zectrix_epd_new(&config, &mut handle)
    })?;
    Ok(handle)
}

fn run(
    driver: EpdDriver,
    slot: Arc<RefreshSlot>,
    notify_rx: Receiver<()>,
    completions: Arc<CompletionMailbox>,
) {
    let mut scratch: Vec<u8> = Vec::with_capacity(canvas::WIDTH * canvas::HEIGHT / 8);
    loop {
        while let Some(cmd) = slot.pending.lock().take() {
            let completion = execute(driver.0, &cmd, &mut scratch);

            completions.send(completion);
        }
        if notify_rx.recv().is_err() {
            break;
        }
    }

    if !driver.0.is_null() {
        unsafe { zectrix_epd_del(driver.0) };
    }
}

fn execute(
    handle: zectrix_epd_handle_t,
    cmd: &RenderCommand,
    scratch: &mut Vec<u8>,
) -> EpdCompletion {
    match cmd.kind {
        RefreshKind::Full => {
            let result = refresh_full(handle, &cmd.frame);
            EpdCompletion {
                kind: RefreshKind::Full,
                request_id: cmd.request_id,
                ok: result.is_ok(),
                recovered: false,
                superseded: false,
            }
        }
        RefreshKind::Partial(rect) => {
            canvas::pack_rect_from_frame(&cmd.frame, rect, scratch);
            match refresh_partial(handle, rect, scratch) {
                Ok(()) => EpdCompletion {
                    kind: RefreshKind::Partial(rect),
                    request_id: cmd.request_id,
                    ok: true,
                    recovered: false,
                    superseded: false,
                },
                Err(err) => {
                    log::warn!("EPD partial refresh failed; trying full refresh: {err}");
                    let result = refresh_full(handle, &cmd.frame);
                    EpdCompletion {
                        kind: RefreshKind::Partial(rect),
                        request_id: cmd.request_id,
                        ok: result.is_ok(),
                        recovered: result.is_ok(),
                        superseded: false,
                    }
                }
            }
        }
    }
}

fn refresh_full(handle: zectrix_epd_handle_t, frame: &[u8]) -> Result<()> {
    check_epd("power on EPD", unsafe { zectrix_epd_power_on(handle) })?;
    let refresh = check_epd("refresh EPD", unsafe {
        zectrix_epd_refresh_full_1bpp(handle, frame.as_ptr(), frame.len())
    });
    let power_off = check_epd("power off EPD", unsafe { zectrix_epd_power_off(handle) });
    refresh.and(power_off)
}

fn refresh_partial(handle: zectrix_epd_handle_t, rect: Rect, pixels: &[u8]) -> Result<()> {
    check_epd("power on EPD", unsafe { zectrix_epd_power_on(handle) })?;
    let c_rect = zectrix_epd_rect_t {
        x: rect.x as i32,
        y: rect.y as i32,
        width: rect.width as i32,
        height: rect.height as i32,
    };
    let refresh = check_epd("refresh EPD partial", unsafe {
        zectrix_epd_refresh_partial_1bpp(handle, &c_rect, pixels.as_ptr(), pixels.len())
    });
    let power_off = check_epd("power off EPD", unsafe { zectrix_epd_power_off(handle) });
    refresh.and(power_off)
}

fn union_rect(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = (a.x + a.width).max(b.x + b.width).min(canvas::WIDTH as u16);
    let bottom = (a.y + a.height)
        .max(b.y + b.height)
        .min(canvas::HEIGHT as u16);
    Rect {
        x,
        y,
        width: right.saturating_sub(x),
        height: bottom.saturating_sub(y),
    }
}

fn check_epd(operation: &str, result: i32) -> Result<()> {
    if result != 0 {
        bail!("{operation} failed with ESP-IDF error 0x{result:04x}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion(request_id: u64) -> EpdCompletion {
        EpdCompletion {
            kind: RefreshKind::Full,
            request_id,
            ok: true,
            recovered: false,
            superseded: false,
        }
    }

    #[test]
    fn completion_mailbox_is_bounded_and_reservation_is_lossless() {
        let mailbox = CompletionMailbox::new();
        for request_id in 0..COMPLETION_CAPACITY as u64 {
            assert!(mailbox.reserve());
            mailbox.complete_reserved(completion(request_id));
        }
        assert!(!mailbox.reserve());
        assert_eq!(mailbox.try_recv().unwrap().request_id, 0);
        assert!(mailbox.reserve());
        mailbox.complete_reserved(completion(99));
        assert_eq!(mailbox.try_recv().unwrap().request_id, 1);
    }
}
