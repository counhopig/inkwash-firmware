// The Rust firmware's bitmap fonts as LVGL fonts (rust-firmware/src/
// {canvas,font8x16,font5x7,font_cjk}.rs), so text renders pixel for pixel
// the same:
//  - Prop(scale): 8x16 proportional ASCII plus 16x16 CJK, integer-scaled.
//  - Small(): 5x7 ASCII plus 12x12 CJK.
// Glyph tops sit at the label's y, like Canvas::draw_text_*.
#pragma once

#include <string>
#include <vector>

#include "assets/icons.h"
#include "lvgl.h"

namespace fonts {

void Init();

const lv_font_t* Prop(int scale);  // 1 to 5
const lv_font_t* Small();

int PropWidth(const std::string& text, int scale);  // Canvas::text_prop_width
int SmallWidth(const std::string& text);            // Canvas::text_small_width
int FitScale(const std::string& text, int max_width, int max_scale);

// screens.rs truncate_prop: cut to max_width with a trailing ellipsis.
std::string TruncateProp(const std::string& text, int max_width);
// screens.rs wrap_text_prop / wrap_text_small.
std::vector<std::string> WrapProp(const std::string& text, int max_width);
std::vector<std::string> WrapSmall(const std::string& text, int max_width);

lv_obj_t* CreateIcon(lv_obj_t* parent, const assets::Icon& icon, int x, int y);

}  // namespace fonts
