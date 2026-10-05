// BLE control channel: a GATT service
// with a write characteristic for commands and a notify characteristic for
// replies, both requiring an authenticated (passkey) encrypted link.
#pragma once

#include <cstdint>
#include <functional>
#include <string>

namespace ble {

enum class Event { Connected, Disconnected, Encrypted, Failed };

// Starts NimBLE and advertises as "Inkwash". The passkey to show on screen is
// returned in *passkey. Callbacks run on the NimBLE host task.
bool Start(uint32_t* passkey, std::function<bool(const std::string& json)> on_command,
           std::function<void(Event)> on_event);

// Stops advertising, drops the link and releases the BLE stack.
void Stop();

bool Active();

// Sends a reply on the notify characteristic; false when no client listens.
bool Notify(const std::string& json);

}  // namespace ble
