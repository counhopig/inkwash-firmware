//! Multi-alarm store, backed by one NVS blob. The PCF8563 only has a single
//! live hardware alarm slot, so `program_hardware_alarm_via` always figures
//! out which stored alarm is chronologically nearest and reprograms the RTC
//! to just that one through the executor - see `rtc::Pcf8563::set_alarm`.

use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition};
use esp_idf_svc::systime::EspSystemTime;
use std::thread;
use std::time::Duration;

use crate::ble_control::BleControl;
use crate::board::Note4Board;
use crate::button::POLL_INTERVAL_MS;
use crate::canvas::Canvas;
use crate::nvs_blob::{read_blob, write_blob, DirtySet};
use crate::rtc::DateTime;
use crate::usb_console::{reject_pending_command, UsbConsole};
use crate::{ui, watchdog};

const NAMESPACE: &str = "inkwash_alrm";
const KEY_ALARMS: &str = "alarms";
/// Locally-changed `local_id`s pending upload (two-way sync dirty set).
const KEY_DIRTY: &str = "dirty";
/// Generous headroom over what a few dozen short JSON alarm records need;
/// NVS blob entries top out around ~4000 bytes on this partition anyway.
const BLOB_BUF_LEN: usize = 1024;
/// Safety bound so an unattended/stuck-button alarm cannot ring forever.
const MAX_RING_SECS: u64 = 300;

/// `Repeat`/`StoredAlarm` and every pure ordering/recurrence/ID-allocation
/// function live in `inkwash-logic` so they can be unit-tested on the host
/// — this crate is the single source of truth, re-exported here so every
/// existing `alarms::Repeat` / `alarms::StoredAlarm` / `alarms::next_due`
/// call site keeps working unchanged.
pub use inkwash_logic::alarm_schedule::{
    date_from_days, days_since_epoch, days_until, next_due, next_id, next_occurrence_date, Repeat,
    StoredAlarm,
};

pub struct AlarmStore {
    nvs: EspDefaultNvs,
}

impl AlarmStore {
    /// `partition` must be a clone of the one shared `EspDefaultNvsPartition`
    /// handle `main.rs` takes once - see the doc comment on
    /// `storage::PersistedCounters::open` for why a second independent
    /// `EspDefaultNvsPartition::take()` here would fail at boot.
    pub fn open(partition: EspDefaultNvsPartition) -> Result<Self> {
        let nvs = EspDefaultNvs::new(partition, NAMESPACE, true)
            .map_err(|e| anyhow!("failed to open NVS namespace '{NAMESPACE}': {e}"))?;
        Ok(Self { nvs })
    }

    /// Empty list if nothing has been saved yet.
    pub fn load(&self) -> Result<Vec<StoredAlarm>> {
        Ok(read_blob::<BLOB_BUF_LEN, _>(&self.nvs, KEY_ALARMS)?.unwrap_or_default())
    }

    pub fn save(&self, alarms: &[StoredAlarm]) -> Result<()> {
        write_blob::<BLOB_BUF_LEN, _>(&self.nvs, KEY_ALARMS, alarms)
    }

    // --- Two-way sync dirty tracking -------------------------------------
    //
    // Same contract as `TodoStore::mark_dirty`: only `local_id`s whose
    // `enabled` flag changed *locally* are uploaded, so a Server/Desktop
    // edit isn't clobbered by the device's stale copy on the next sync.

    fn dirty(&self) -> DirtySet<'_> {
        DirtySet::new(&self.nvs, KEY_DIRTY)
    }

    /// Marks `id` as locally changed (enabled flag) and pending upload.
    pub fn mark_dirty(&self, id: u8) -> Result<()> {
        self.dirty().mark(id)
    }

    /// `local_id`s changed locally since the last successful sync.
    pub fn dirty_ids(&self) -> Result<Vec<u8>> {
        self.dirty().ids()
    }

    /// Drops the dirty set after a successful sync.
    pub fn clear_dirty(&self) -> Result<()> {
        self.dirty().clear()
    }
}

pub fn ring_screen(
    board: &mut Note4Board,
    usb: &mut UsbConsole,
    ble: Option<&mut BleControl>,
) -> Result<()> {
    ring_until_dismissed(board, usb, ble)
}

/// Draws the alarm screen, then alternates short tone bursts with polling
/// ENTER until dismissed or the safety timeout elapses.
fn ring_until_dismissed(
    board: &mut Note4Board,
    usb: &mut UsbConsole,
    mut ble: Option<&mut BleControl>,
) -> Result<()> {
    let mut canvas = board.display.canvas_mut();
    canvas.clear();
    ui::header(&mut canvas, "ALARM");
    let alarm_w = Canvas::text_prop_width("ALARM", 4);
    canvas.draw_text_prop(200usize.saturating_sub(alarm_w / 2), 92, 4, "ALARM");
    let hint = "ENTER = DISMISS";
    let hint_w = Canvas::text_prop_width(hint, 1);
    canvas.draw_text_prop(200usize.saturating_sub(hint_w / 2), 184, 1, hint);
    drop(canvas);
    board.display.refresh_full_best_effort();

    let start = EspSystemTime {}.now();
    loop {
        watchdog::feed();
        reject_pending_command(usb);
        if let Some(ble) = ble.as_deref_mut() {
            crate::ble_control::reject_pending_command(ble);
        }
        board.key_enter.poll();
        if board.key_enter.is_raw_pressed() {
            while board.key_enter.poll().is_some() {}
            log::info!("Alarm dismissed");
            return Ok(());
        }
        let elapsed = EspSystemTime {}.now().saturating_sub(start);
        if elapsed >= Duration::from_secs(MAX_RING_SECS) {
            log::warn!("Alarm ring timed out after {MAX_RING_SECS}s with no dismiss");
            return Ok(());
        }
        if let Some(audio) = board.audio.as_mut() {
            if let Err(err) = audio.play_sine_stereo(880.0, 0.05, 8000) {
                log::warn!("Alarm tone playback failed: {err}");
            }
        } else {
            thread::sleep(Duration::from_millis(POLL_INTERVAL_MS as u64));
        }
        // `play_sine_stereo` blocks for ~210ms per call (see
        // audio.rs::drain_and_disable's unconditional 150ms drain sleep),
        // so polling the button only once per tone (as this loop used to)
        // samples it roughly every 210ms - a quick press entirely inside
        // that gap was invisible to the debounce state machine, which only
        // advances when `poll()` is actually called. Matches
        // `reminders.rs::show_urgent`'s already-verified pattern: a tight
        // 20ms-granularity poll window between tones instead.
        let poll_deadline = EspSystemTime {}.now() + Duration::from_millis(400);
        loop {
            watchdog::feed();
            reject_pending_command(usb);
            if let Some(ble) = ble.as_deref_mut() {
                crate::ble_control::reject_pending_command(ble);
            }
            board.key_enter.poll();
            if board.key_enter.is_pressed() {
                while board.key_enter.poll().is_some() {}
                log::info!("Alarm dismissed");
                return Ok(());
            }
            if (EspSystemTime {}).now() >= poll_deadline {
                break;
            }
            thread::sleep(Duration::from_millis(POLL_INTERVAL_MS as u64));
        }
    }
}

/// Timer wake needed to make a future-month one-shot alarm fully offline.
/// PCF8563 cannot compare month/year, so the ESP wakes one minute before the
/// earliest target month, remains in the normal main loop, and arms the RTC
/// alarm when the date boundary is observed. RTC alarm wake remains available
/// independently through EXT1 for already-armable alarms.
pub fn maintenance_wakeup_delay(alarms: &[StoredAlarm], now: &DateTime) -> Option<Duration> {
    let now_epoch = now.to_unix();
    alarms
        .iter()
        .filter(|alarm| alarm.enabled)
        .filter_map(|alarm| match alarm.repeat {
            Repeat::Once { year, month, .. }
                if (year, month) > (now.year, now.month) && (1..=12).contains(&month) =>
            {
                Some(
                    DateTime {
                        year,
                        month,
                        day: 1,
                        ..DateTime::default()
                    }
                    .to_unix(),
                )
            }
            _ => None,
        })
        .min()
        .map(|target_month| {
            // Wake before midnight so the ordinary date-boundary tick can do
            // the actual RTC programming without racing a 00:00 alarm.
            Duration::from_secs(target_month.saturating_sub(now_epoch + 60).max(1))
        })
}

/// Reprograms the PCF8563's single hardware alarm slot to whichever stored
/// alarm is nearest, or clears it if none are enabled. All RTC I2C goes
/// through the executor client (`rtc_executor::RtcExecutor`) — the only
/// owner of the driver — so this function takes the client instead of a
/// `&mut Pcf8563`.
pub fn program_hardware_alarm_via(
    rtc: &crate::rtc_executor::RtcExecutor,
    alarms: &[StoredAlarm],
    now: &DateTime,
) -> Result<()> {
    match next_due(alarms, now) {
        Some(alarm) => {
            // The once-outside-month case (defer arm to avoid a false ring)
            // and the register fields for each repeat are pure logic in
            // inkwash-logic; issue the executor command for the target.
            let regs = inkwash_logic::alarm_schedule::alarm_regs_for(alarm, now);
            match regs {
                Some(regs) => {
                    rtc.program(&regs)?;
                    log::info!(
                        "Hardware alarm armed via executor: id={} {:02}:{:02} ({:?})",
                        alarm.id,
                        alarm.hour,
                        alarm.minute,
                        alarm.repeat
                    );
                }
                None => {
                    log::info!(
                        "Once alarm id={} is outside the current month ({:04}-{:02}); deferring hardware arm to avoid an early false ring",
                        alarm.id,
                        now.year,
                        now.month
                    );
                    rtc.disable()?;
                }
            }
        }
        None => {
            rtc.disable()?;
            log::info!("No enabled alarms; hardware alarm cleared via executor");
        }
    }
    Ok(())
}
