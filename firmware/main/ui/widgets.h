// Pixel-positioned LVGL primitives.
#pragma once

#include <string>

#include "fonts.h"
#include "lvgl.h"

namespace ui {

// A plain object with every default style removed.
inline lv_obj_t* Bare(lv_obj_t* parent) {
    lv_obj_t* obj = lv_obj_create(parent);
    lv_obj_remove_style_all(obj);
    lv_obj_remove_flag(obj, LV_OBJ_FLAG_SCROLLABLE);
    lv_obj_remove_flag(obj, LV_OBJ_FLAG_CLICKABLE);
    return obj;
}

// Canvas::fill_rect(x, y, w, h, black)
inline lv_obj_t* FillRect(lv_obj_t* parent, int x, int y, int w, int h, bool black = true) {
    lv_obj_t* obj = Bare(parent);
    lv_obj_set_pos(obj, x, y);
    lv_obj_set_size(obj, w, h);
    lv_obj_set_style_bg_opa(obj, LV_OPA_COVER, 0);
    lv_obj_set_style_bg_color(obj, black ? lv_color_black() : lv_color_white(), 0);
    return obj;
}

// Canvas::stroke_rect(x, y, w, h, thickness): a black outline, clear inside.
inline lv_obj_t* StrokeRect(lv_obj_t* parent, int x, int y, int w, int h, int thickness) {
    lv_obj_t* obj = Bare(parent);
    lv_obj_set_pos(obj, x, y);
    lv_obj_set_size(obj, w, h);
    lv_obj_set_style_border_width(obj, thickness, 0);
    lv_obj_set_style_border_color(obj, lv_color_black(), 0);
    lv_obj_set_style_border_opa(obj, LV_OPA_COVER, 0);
    return obj;
}

inline lv_obj_t* Label(lv_obj_t* parent, int x, int y, const lv_font_t* font, const std::string& text) {
    lv_obj_t* label = lv_label_create(parent);
    lv_obj_remove_style_all(label);
    lv_obj_set_style_text_font(label, font, 0);
    lv_obj_set_style_text_color(label, lv_color_black(), 0);
    lv_obj_set_style_text_letter_space(label, 0, 0);
    lv_obj_set_style_text_line_space(label, 0, 0);
    lv_label_set_text(label, text.c_str());
    lv_obj_set_pos(label, x, y);
    return label;
}

// Canvas::draw_text_prop(x, y, scale, text): glyph tops at y.
inline lv_obj_t* Text(lv_obj_t* parent, int x, int y, int scale, const std::string& text) {
    return Label(parent, x, y, fonts::Prop(scale), text);
}

// Canvas::draw_text_small(x, y, text)
inline lv_obj_t* SmallText(lv_obj_t* parent, int x, int y, const std::string& text) {
    return Label(parent, x, y, fonts::Small(), text);
}

}  // namespace ui
