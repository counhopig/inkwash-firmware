// Minimum safe mode (rust-firmware/src/main.rs run_safe_mode): entered after
// kMaxBootFailures consecutive failed boots. Shows why, never touches stored
// data, keeps the USB console answering, and does not bring up Wi-Fi, BLE,
// audio or the RTC alarm path that may be what keeps crashing.
#pragma once

#include "core/boot_guard.h"

namespace safe_mode {

[[noreturn]] void Run(const boot_guard::Ledger& ledger, boot_guard::ResetKind last_reset);

}  // namespace safe_mode
