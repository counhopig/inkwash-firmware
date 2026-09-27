// The Rust firmware's 8x16 proportional bitmap font, exposed as LVGL fonts
// at the integer scales the UI uses (1, 2, 3, 5), plus the status icons.
#pragma once

#include "assets/icons.h"
#include "lvgl.h"

namespace fonts {

// Registers the fonts; call once after lv_init().
void Init();

// The font at `scale` (1, 2, 3 or 5; other values fall back to 1).
const lv_font_t* Prop(int scale);

// Rendered width of `text` in pixels, matching Canvas::text_prop_width.
int PropWidth(const char* text, int scale);

// Largest scale <= max_scale whose width fits max_width (at least 1).
int FitScale(const char* text, int max_width, int max_scale);

// Places `icon` with its top-left corner at (x, y), drawn in black.
lv_obj_t* CreateIcon(lv_obj_t* parent, const assets::Icon& icon, int x, int y);

}  // namespace fonts
