//! Classification of persistent-store reads performed while the device boots.
//!
//! A first-time device has nothing stored under a key, and that is not a
//! failure: the boot path applies the documented default. A value that exists
//! but cannot be turned back into a `Vec<Todo>` is a different event, and
//! folding it into that default silently loses user state — an empty Todo list
//! or an unconfigured server looks exactly like a device nobody has set up yet.
//!
//! So every boot-time read reports either a value, an absent key, or a fault
//! class, and [`resolve`] turns a fault into a [`BootFault`] carrying the store
//! name and the failure class only. That keeps the boot log and the safe-mode
//! panel informative (the operator learns *which* store broke and *how*) while
//! never echoing stored bytes, which may hold a Wi-Fi password or a Bearer
//! token.

/// A persistent store the boot path must read before it can run the device.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum StoreId {
    Alarms,
    Todos,
    Inbox,
    ServerConfig,
    WifiCredentials,
    Timezone,
    SyncInterval,
    LastSyncEpoch,
}

impl StoreId {
    pub const ALL: [StoreId; 8] = [
        StoreId::Alarms,
        StoreId::Todos,
        StoreId::Inbox,
        StoreId::ServerConfig,
        StoreId::WifiCredentials,
        StoreId::Timezone,
        StoreId::SyncInterval,
        StoreId::LastSyncEpoch,
    ];

    /// Human-readable, ASCII-only name used in boot logs and on the panel.
    pub const fn label(self) -> &'static str {
        match self {
            StoreId::Alarms => "alarm store",
            StoreId::Todos => "todo store",
            StoreId::Inbox => "inbox store",
            StoreId::ServerConfig => "server config",
            StoreId::WifiCredentials => "wifi credentials",
            StoreId::Timezone => "timezone setting",
            StoreId::SyncInterval => "sync interval",
            StoreId::LastSyncEpoch => "sync metadata",
        }
    }
}

/// Why a stored value could not be turned into a domain value.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum StoreFault {
    /// The bytes are there but are not a value this firmware can decode.
    Corrupt,

    /// The value decodes, but it carries a version this firmware does not know.
    UnsupportedVersion,

    /// The storage layer itself failed (read error, key of the wrong type,
    /// stored object larger than the buffer reserved for it).
    Io,
}

impl StoreFault {
    pub const ALL: [StoreFault; 3] = [
        StoreFault::Corrupt,
        StoreFault::UnsupportedVersion,
        StoreFault::Io,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            StoreFault::Corrupt => "corrupt data",
            StoreFault::UnsupportedVersion => "newer data version",
            StoreFault::Io => "storage I/O error",
        }
    }
}

impl std::fmt::Display for StoreFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl std::error::Error for StoreFault {}

/// An unreadable store that blocks the boot path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BootFault {
    store: StoreId,
    cause: StoreFault,
}

impl BootFault {
    pub const fn new(store: StoreId, cause: StoreFault) -> Self {
        Self { store, cause }
    }

    pub const fn store(self) -> StoreId {
        self.store
    }

    pub const fn cause(self) -> StoreFault {
        self.cause
    }

    /// Message for the boot log and the safe-mode panel. Names the store and
    /// the failure class, never the stored value, and stays inside one panel
    /// row so it is legible on the device itself.
    pub fn reason(self) -> String {
        format!("{}: {}", self.store.label(), self.cause.label())
    }
}

/// Timezone assumed when the device has never stored one (UTC).
pub const DEFAULT_TIMEZONE_OFFSET_MINUTES: i16 = 0;

/// Synchronization period assumed when the device has never stored one.
pub const DEFAULT_SYNC_INTERVAL_MINUTES: u16 = 60;

/// Boot-time read policy: a read that succeeded is returned as it came, and a
/// fault is reported so the caller can stop booting instead of degrading real
/// data to the default for an absent key.
///
/// Stores with a meaningful absent state read as `Option<T>` and are unpacked
/// with the documented default at the call site; stores where absent and empty
/// are the same state read as `Vec<T>` directly.
pub fn resolve<T>(store: StoreId, read: Result<T, StoreFault>) -> Result<T, BootFault> {
    read.map_err(|cause| BootFault::new(store, cause))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_value_passes_through_unchanged() {
        assert_eq!(resolve(StoreId::Todos, Ok(Some(7u32))), Ok(Some(7)));
    }

    #[test]
    fn an_absent_key_is_first_time_configuration_not_a_fault() {
        assert_eq!(
            resolve::<Option<u32>>(StoreId::WifiCredentials, Ok(None)),
            Ok(None)
        );
    }

    #[test]
    fn a_store_whose_absent_state_is_empty_passes_the_empty_value_through() {
        let empty = resolve::<Vec<u32>>(StoreId::Todos, Ok(Vec::new()));
        assert_eq!(empty, Ok(Vec::new()));
    }

    #[test]
    fn every_fault_class_of_every_store_stops_the_boot() {
        for store in StoreId::ALL {
            for cause in StoreFault::ALL {
                let decided = resolve::<u32>(store, Err(cause));
                assert_eq!(
                    decided,
                    Err(BootFault::new(store, cause)),
                    "{store:?}/{cause:?} must reach the boot path as a fault"
                );
                let fault = decided.expect_err("fault must not be swallowed");
                assert_eq!(fault.store(), store);
                assert_eq!(fault.cause(), cause);
            }
        }
    }

    #[test]
    fn a_fault_never_becomes_a_default_value() {
        let degraded = resolve::<Vec<u32>>(StoreId::Todos, Err(StoreFault::Corrupt));
        assert!(
            degraded.is_err(),
            "corrupted data must not be readable as an empty list"
        );
    }

    #[test]
    fn reasons_name_the_store_and_the_failure_class() {
        for store in StoreId::ALL {
            for cause in StoreFault::ALL {
                let reason = BootFault::new(store, cause).reason();
                assert!(
                    reason.contains(store.label()) && reason.contains(cause.label()),
                    "`{reason}` must identify both the store and the failure class"
                );
            }
        }
    }

    #[test]
    fn reasons_are_ascii_and_fit_one_panel_row() {
        for store in StoreId::ALL {
            for cause in StoreFault::ALL {
                let reason = BootFault::new(store, cause).reason();
                assert!(
                    reason.is_ascii(),
                    "`{reason}` is drawn with the ASCII panel font"
                );
                assert!(
                    reason.chars().count() <= 40,
                    "`{reason}` must stay inside one 40-column row"
                );
            }
        }
    }

    #[test]
    fn every_store_and_fault_class_has_a_distinct_label() {
        let mut labels: Vec<&str> = StoreId::ALL.iter().map(|store| store.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), StoreId::ALL.len());

        let mut causes: Vec<&str> = StoreFault::ALL.iter().map(|cause| cause.label()).collect();
        causes.sort_unstable();
        causes.dedup();
        assert_eq!(causes.len(), StoreFault::ALL.len());
    }
}
