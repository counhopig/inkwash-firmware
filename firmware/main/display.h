// E-paper display behind LVGL.
//
// LVGL renders RGB565 strips; the flush callback thresholds them into the
// panel's 1 bpp frame and records the dirty area. Update() then pushes that
// area to the panel with a partial refresh, or the whole frame with a full
// refresh when asked, on first use, or periodically to clear ghosting.
#pragma once

#include "lvgl.h"

namespace display {

bool Init();

lv_display_t* Lvgl();

// Renders pending LVGL changes and refreshes the panel. Blocks until the
// panel refresh completes (~0.5 s partial, ~2-3 s full).
void Update(bool full);

}  // namespace display
