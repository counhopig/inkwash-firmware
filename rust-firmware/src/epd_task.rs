//! Dedicated EPD refresh task.
//!
//! Owns the synchronous `zectrix_epd` FFI driver and serializes every
//! panel refresh on one thread, so the main loop never blocks on a
//! refresh.
//!
//! Request model: every refresh command carries an
//! **immutable pixel snapshot** of the whole frame, copied under the
//! canvas lock at request time. The task never touches the shared canvas,
//! so a pending request always paints exactly the picture its requester
//! drew - a later page/screen drawing over the canvas cannot corrupt a
//! queued refresh (the defect of the old "copy at execution time" design,
//! where a queued rect executed against whatever the canvas held by then).
//! The request path is a single latest-wins slot instead of an unbounded
//! queue: at most one command is pending, a `Full` request replaces any
//! pending partial, and partials merge into the union of their rects with
//! the newest frame. Memory is bounded (at most one 15 KB frame snapshot
//! pending).
//!
//! Completion: the task reports every refresh back to the
//! app through a completion channel - success, failure, and partial->full
//! recovery are all observable, so the app state machine can treat a frame
//! as displayed only once its completion arrives.

use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender};
use std::sync::Arc;

use parking_lot::Mutex;

use anyhow::{bail, Result};
use esp_idf_svc::sys::zectrix_epd::{
    zectrix_epd_config_t, zectrix_epd_del, zectrix_epd_get_default_config, zectrix_epd_handle_t,
    zectrix_epd_new, zectrix_epd_power_off, zectrix_epd_power_on, zectrix_epd_rect_t,
    zectrix_epd_refresh_full_1bpp, zectrix_epd_refresh_partial_1bpp,
};

use crate::canvas::{self, Rect};

/// What to refresh: the whole panel, or one rect of the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshKind {
    Full,
    Partial(Rect),
}

/// One refresh request: an immutable full-frame pixel snapshot plus the
/// region to paint from it. The snapshot is captured at request time under
/// the canvas lock, so the panel shows exactly the frame the requester
/// drew regardless of what the canvas holds later.
pub struct RenderCommand {
    pub kind: RefreshKind,
    pub frame: Box<[u8]>,
}

/// Completion report for one executed refresh.
#[derive(Debug, Clone, Copy)]
pub struct EpdCompletion {
    pub kind: RefreshKind,
    /// Whether the panel-level refresh succeeded. For a partial refresh
    /// that failed and recovered via a full refresh from the same
    /// snapshot, `ok` is true and `recovered` marks the recovery.
    pub ok: bool,
    /// A partial refresh failed and was re-executed as a full refresh
    /// from the same snapshot.
    pub recovered: bool,
}

/// The single pending-request slot shared by the app thread (writer) and
/// the EPD task (consumer).
struct RefreshSlot {
    pending: Mutex<Option<RenderCommand>>,
    /// Wakes the task; `sync_channel(1)` so at most one wakeup is ever
    /// queued. A full queue is not a lost wakeup - the task re-checks the
    /// slot on every loop iteration.
    notify_tx: SyncSender<()>,
}

impl RefreshSlot {
    /// Submits a partial refresh. Merges into any pending command:
    /// partials union their rects (the newest frame wins), and a pending
    /// full is superseded by the newer frame's full refresh (it covers
    /// everything anyway).
    fn submit_partial(&self, rect: Rect, frame: Box<[u8]>) {
        let mut pending = self.pending.lock();
        match pending.as_mut() {
            Some(cmd) => match &mut cmd.kind {
                RefreshKind::Full => {
                    cmd.frame = frame;
                }
                RefreshKind::Partial(prev) => {
                    *prev = union_rect(*prev, rect);
                    cmd.frame = frame;
                }
            },
            None => {
                *pending = Some(RenderCommand {
                    kind: RefreshKind::Partial(rect),
                    frame,
                });
            }
        }
        let _ = self.notify_tx.try_send(());
    }

    /// Submits a full refresh; replaces any pending command (a full of
    /// the newest frame supersedes a pending partial).
    fn submit_full(&self, frame: Box<[u8]>) {
        let mut pending = self.pending.lock();
        *pending = Some(RenderCommand {
            kind: RefreshKind::Full,
            frame,
        });
        let _ = self.notify_tx.try_send(());
    }
}

/// App-side handle: the request slot plus the completion receiver.
pub struct EpdHandle {
    slot: Arc<RefreshSlot>,
    completions: Receiver<EpdCompletion>,
}

impl EpdHandle {
    /// Queues a partial refresh of `rect`, merging into any pending
    /// request (latest-wins).
    pub fn request_partial(&self, rect: Rect, frame: Box<[u8]>) {
        self.slot.submit_partial(rect, frame);
    }

    /// Queues a full refresh of the newest frame, replacing any pending
    /// request.
    pub fn request_full(&self, frame: Box<[u8]>) {
        self.slot.submit_full(frame);
    }

    /// Non-blocking drain of completed refreshes.
    pub fn poll_completion(&self) -> Option<EpdCompletion> {
        self.completions.try_recv().ok()
    }
}

/// 12 KiB stack: the FFI driver (SPI + waveform) is the deepest caller on
/// this thread; the default 4 KiB pthread stack would be tight, and 16 KiB
/// plus the sync task's stack overran internal RAM (pthread stacks are
/// forced to MALLOC_CAP_INTERNAL - the device reboot-looped at boot).
const EPD_TASK_STACK: usize = 12 * 1024;

/// Initializes the EPD driver and spawns the refresh task. An init fault
/// propagates to the caller (board bring-up fails loudly) rather than
/// silently killing a background task.
pub fn spawn() -> Result<EpdHandle> {
    let driver = EpdDriver(init_driver()?);
    // One notify channel: the sender wakes the task through `RefreshSlot`,
    // the receiver is the run-loop's wake source. The previous code created
    // two independent `sync_channel(1)`s - one whose sender was kept and one
    // whose receiver was handed to `run` - so `submit_*` notified a channel
    // with a dropped receiver (`try_send` always failed) and `run` blocked
    // forever on a channel nobody sent to. The task executed at most the
    // commands already pending at startup, then never drained again: every
    // later refresh (clock minute changes, deep-sleep wake region) was lost,
    // which froze the on-screen clock.
    let (notify_tx, notify_rx) = sync_channel(1);
    let slot = Arc::new(RefreshSlot {
        pending: Mutex::new(None),
        notify_tx,
    });
    let (completions_tx, completions_rx) = channel();
    let task_slot = Arc::clone(&slot);
    std::thread::Builder::new()
        .name("epd".to_string())
        .stack_size(EPD_TASK_STACK)
        .spawn(move || run(driver, task_slot, notify_rx, completions_tx))?;
    Ok(EpdHandle {
        slot,
        completions: completions_rx,
    })
}

/// FFI driver handle moved into the EPD task. Raw pointers are `!Send` on
/// this toolchain; the handle is only ever passed back to the C driver
/// (never dereferenced here), so marking it `Send` to cross the thread
/// boundary is sound.
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
    completions: Sender<EpdCompletion>,
) {
    // Scratch buffer for the pixels handed to the FFI, reused across
    // refreshes so a request never allocates.
    let mut scratch: Vec<u8> = Vec::with_capacity(canvas::WIDTH * canvas::HEIGHT / 8);
    loop {
        // Drain every pending command before blocking again; the slot is
        // re-checked after each one, so requests submitted during a
        // refresh are not lost even when the wakeup queue was full.
        while let Some(cmd) = slot.pending.lock().take() {
            let completion = execute(driver.0, &cmd, &mut scratch);
            let _ = completions.send(completion);
        }
        if notify_rx.recv().is_err() {
            break; // client is gone
        }
    }
    // Channel closed: the client is gone. Release the driver.
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
                ok: result.is_ok(),
                recovered: false,
            }
        }
        RefreshKind::Partial(rect) => {
            canvas::pack_rect_from_frame(&cmd.frame, rect, scratch);
            match refresh_partial(handle, rect, scratch) {
                Ok(()) => EpdCompletion {
                    kind: RefreshKind::Partial(rect),
                    ok: true,
                    recovered: false,
                },
                Err(err) => {
                    // Recovery path, same as the old synchronous
                    // `refresh_partial_best_effort`, but deterministic: the
                    // full refresh reuses the *same* snapshot the failed
                    // partial came from, not whatever the canvas holds now.
                    log::warn!("EPD partial refresh failed; trying full refresh: {err}");
                    let result = refresh_full(handle, &cmd.frame);
                    EpdCompletion {
                        kind: RefreshKind::Partial(rect),
                        ok: result.is_ok(),
                        recovered: result.is_ok(),
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

/// Smallest rect containing both inputs, clamped to the frame.
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
