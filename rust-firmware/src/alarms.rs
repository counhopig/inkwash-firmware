//! Multi-alarm store, backed by one NVS blob. The PCF8563 only has a single
//! live hardware alarm slot, so `program_hardware_alarm_via` always figures
//! out which stored alarm is chronologically nearest and reprograms the RTC
//! to just that one through the executor - see `rtc::Pcf8563::set_alarm`.

use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition};

use crate::nvs_blob::{read_blob, write_blob, DirtySet};
use crate::rtc::DateTime;

const NAMESPACE: &str = "inkwash_alrm";
const KEY_ALARMS: &str = "alarms";
/// Locally-changed `local_id`s pending upload (two-way sync dirty set).
const KEY_DIRTY: &str = "dirty";
/// Generous headroom over what a few dozen short JSON alarm records need;
/// NVS blob entries top out around ~4000 bytes on this partition anyway.
const BLOB_BUF_LEN: usize = 1024;
/// `Repeat`/`StoredAlarm` and every pure ordering/recurrence/ID-allocation
/// function live in `inkwash-logic` so they can be unit-tested on the host
/// — this crate is the single source of truth, re-exported here so every
/// existing `alarms::Repeat` / `alarms::StoredAlarm` / `alarms::next_due`
/// call site keeps working unchanged.
pub use inkwash_logic::alarm_schedule::{
    date_from_days, days_since_epoch, days_until, maintenance_wakeup_delay, next_due,
    next_occurrence_date, Repeat, StoredAlarm,
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
