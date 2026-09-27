#include "fonts.h"

#include <cstring>
#include <map>
#include <vector>

#include "assets/font8x16.h"

namespace fonts {
namespace {

constexpr int kScales[] = {1, 2, 3, 5};
constexpr uint32_t kMiddleDot = 0x00B7;
// The Rust firmware draws '·' from its 16x16 CJK font: a 17-pixel cell.
constexpr int kMiddleDotWidth = 16;
constexpr int kMiddleDotAdvance = 17;

lv_font_t g_fonts[sizeof(kScales) / sizeof(kScales[0])];

int ScaleOf(const lv_font_t* font) {
    return static_cast<int>(reinterpret_cast<intptr_t>(font->user_data));
}

// Width in font pixels of `letter` (before scaling) and its advance.
void Metrics(uint32_t letter, int* width, int* advance) {
    if (letter == kMiddleDot) {
        *width = kMiddleDotWidth;
        *advance = kMiddleDotAdvance;
        return;
    }
    if (letter < assets::kFont16First || letter > assets::kFont16Last) {
        letter = assets::kFont16First;  // unknown glyphs draw as a space
    }
    *width = assets::kFont16Widths[letter - assets::kFont16First];
    *advance = *width + 1;
}

// Decodes one UTF-8 code point starting at text[*i] and advances *i.
uint32_t NextCodePoint(const char* text, size_t* i) {
    const auto* s = reinterpret_cast<const unsigned char*>(text);
    const unsigned char c = s[*i];
    int extra = c < 0x80 ? 0 : (c >> 5) == 0x6 ? 1 : (c >> 4) == 0xE ? 2 : (c >> 3) == 0x1E ? 3 : -1;
    if (extra < 0) {
        ++*i;
        return '?';
    }
    uint32_t cp = extra == 0 ? c : c & (0x3F >> extra);
    ++*i;
    for (int k = 0; k < extra; ++k) {
        if ((s[*i] & 0xC0) != 0x80) {
            return '?';
        }
        cp = (cp << 6) | (s[*i] & 0x3F);
        ++*i;
    }
    return cp;
}

bool Pixel(uint32_t letter, int x, int y) {
    if (letter == kMiddleDot) {
        return (x == 7 || x == 8) && (y == 7 || y == 8);
    }
    if (letter < assets::kFont16First || letter > assets::kFont16Last) {
        return false;
    }
    const uint16_t row = assets::kFont16Glyphs[letter - assets::kFont16First][y];
    return (row & (0x8000u >> x)) != 0;
}

bool GetGlyphDsc(const lv_font_t* font, lv_font_glyph_dsc_t* dsc, uint32_t letter,
                 uint32_t /*letter_next*/) {
    const int scale = ScaleOf(font);
    int width = 0;
    int advance = 0;
    Metrics(letter, &width, &advance);
    dsc->adv_w = static_cast<uint16_t>(advance * scale);
    dsc->box_w = static_cast<uint16_t>(width * scale);
    dsc->box_h = static_cast<uint16_t>(assets::kFont16Height * scale);
    dsc->ofs_x = 0;
    dsc->ofs_y = 0;
    dsc->stride = 0;
    dsc->format = LV_FONT_GLYPH_FORMAT_A1;
    dsc->is_placeholder = 0;
    dsc->gid.index = letter + 1;  // 0 means "no glyph" to LVGL
    return true;
}

const void* GetGlyphBitmap(lv_font_glyph_dsc_t* dsc, lv_draw_buf_t* buf) {
    if (dsc->req_raw_bitmap || dsc->box_w == 0 || dsc->box_h == 0) {
        return nullptr;
    }
    const int scale = ScaleOf(dsc->resolved_font);
    const uint32_t letter = dsc->gid.index - 1;
    const uint32_t stride = lv_draw_buf_width_to_stride(dsc->box_w, LV_COLOR_FORMAT_A8);
    uint8_t* out = static_cast<uint8_t*>(buf->data);
    for (int y = 0; y < dsc->box_h; ++y) {
        uint8_t* row = out + y * stride;
        for (int x = 0; x < dsc->box_w; ++x) {
            row[x] = Pixel(letter, x / scale, y / scale) ? 0xFF : 0x00;
        }
    }
    lv_draw_buf_flush_cache(buf, nullptr);
    return buf;
}

// Icons become 1-bit indexed images: index 0 transparent, index 1 black.
struct IconImage {
    std::vector<uint8_t> data;
    lv_image_dsc_t dsc = {};
};
std::map<const assets::Icon*, IconImage> g_icons;

const lv_image_dsc_t* IconDsc(const assets::Icon& icon) {
    auto it = g_icons.find(&icon);
    if (it != g_icons.end()) {
        return &it->second.dsc;
    }
    IconImage& img = g_icons[&icon];
    const int stride = (icon.width + 7) / 8;
    img.data.assign(8 + stride * icon.height, 0);
    const uint32_t palette[2] = {0x00000000u, 0xFF000000u};  // ARGB8888
    std::memcpy(img.data.data(), palette, sizeof(palette));
    uint8_t* pixels = img.data.data() + 8;
    for (int y = 0; y < icon.height; ++y) {
        for (int x = 0; x < icon.width; ++x) {
            if (icon.rows[y] & (0x80000000u >> x)) {
                pixels[y * stride + x / 8] |= static_cast<uint8_t>(0x80 >> (x % 8));
            }
        }
    }
    img.dsc.header.magic = LV_IMAGE_HEADER_MAGIC;
    img.dsc.header.cf = LV_COLOR_FORMAT_I1;
    img.dsc.header.w = icon.width;
    img.dsc.header.h = icon.height;
    img.dsc.header.stride = static_cast<uint32_t>(stride);
    img.dsc.data_size = static_cast<uint32_t>(img.data.size());
    img.dsc.data = img.data.data();
    return &img.dsc;
}

}  // namespace

void Init() {
    for (size_t i = 0; i < sizeof(kScales) / sizeof(kScales[0]); ++i) {
        lv_font_t& font = g_fonts[i];
        std::memset(&font, 0, sizeof(font));
        font.get_glyph_dsc = GetGlyphDsc;
        font.get_glyph_bitmap = GetGlyphBitmap;
        font.line_height = assets::kFont16Height * kScales[i];
        font.base_line = 0;
        font.user_data = reinterpret_cast<void*>(static_cast<intptr_t>(kScales[i]));
    }
}

const lv_font_t* Prop(int scale) {
    for (size_t i = 0; i < sizeof(kScales) / sizeof(kScales[0]); ++i) {
        if (kScales[i] == scale) {
            return &g_fonts[i];
        }
    }
    return &g_fonts[0];
}

int PropWidth(const char* text, int scale) {
    int total = 0;
    size_t i = 0;
    while (text[i] != '\0') {
        const uint32_t letter = NextCodePoint(text, &i);
        int width = 0;
        int advance = 0;
        Metrics(letter, &width, &advance);
        total += advance * scale;
    }
    return total;
}

int FitScale(const char* text, int max_width, int max_scale) {
    for (int scale = max_scale; scale > 1; --scale) {
        if (PropWidth(text, scale) <= max_width) {
            return scale;
        }
    }
    return 1;
}

lv_obj_t* CreateIcon(lv_obj_t* parent, const assets::Icon& icon, int x, int y) {
    lv_obj_t* image = lv_image_create(parent);
    lv_image_set_src(image, IconDsc(icon));
    lv_obj_set_pos(image, x, y);
    return image;
}

}  // namespace fonts
