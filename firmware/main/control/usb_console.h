// Control commands over USB Serial/JTAG (rust-firmware/src/usb_console.rs):
// the host sends ">>IW {json}\n", the device answers "<<IW {json}\n".
#pragma once

#include <functional>
#include <string>

namespace usb_console {

// Starts the reader task; `on_line` receives each command's JSON text.
void Start(std::function<void(const std::string& json)> on_line);

// Writes one reply line.
void Reply(const std::string& json);

// True while a USB host is attached (it keeps the device out of deep sleep).
bool HostConnected();

}  // namespace usb_console
