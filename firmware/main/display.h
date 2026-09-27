// E-paper display behind LVGL.
//
// LVGL renders RGB565 strips; the flush callback thresholds them into the
// panel's 1 bpp frame. Update() compares that frame with what the panel
// shows and partially refreshes the bounding box of the changed pixels, so a
// screen can be rebuilt from scratch and still refresh only what changed. A
// full refresh clears accumulated ghosting now and then.
#pragma once

#include <cstdint>

#include "lvgl.h"

namespace display {

bool Init();

lv_display_t* Lvgl();

enum class Refresh { Auto, Full };

// Renders pending LVGL changes and refreshes the panel. Blocks until the
// panel refresh completes (~0.5 s partial, ~2-3 s full).
void Update(Refresh mode = Refresh::Auto);

// After a deep-sleep wake: renders the current LVGL screen and declares it to
// be what the panel already shows, without refreshing (see
// zectrix_epd_seed_shadow_1bpp). Call with the screen shown before sleep.
void AdoptPanelContent(uint8_t partial_refreshes);

// Partial refreshes since the last full one (persisted across deep sleep).
uint8_t PartialRefreshes();

}  // namespace display
