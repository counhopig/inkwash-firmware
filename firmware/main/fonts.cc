#include "fonts.h"

#include <cctype>
#include <cstring>
#include <map>

#include "assets/font5x7.h"
#include "assets/font8x16.h"

extern const uint8_t hzk16_start[] asm("_binary_hzk16_bin_start");
extern const uint8_t hzk16_end[] asm("_binary_hzk16_bin_end");
extern const uint8_t hzk12_start[] asm("_binary_hzk12_bin_start");
extern const uint8_t cjk_index_start[] asm("_binary_cjk_index_bin_start");
extern const uint8_t cjk_index_end[] asm("_binary_cjk_index_bin_end");

namespace fonts {
namespace {

constexpr int kScales[] = {1, 2, 3, 4, 5};
constexpr int kNumScales = sizeof(kScales) / sizeof(kScales[0]);
constexpr int kCjk16Bytes = 32;
constexpr int kCjk16Advance = 17;
constexpr int kCjk12Bytes = 24;
constexpr int kCjk12Advance = 13;
constexpr int kSmallLine = 12;

lv_font_t g_prop[kNumScales];
lv_font_t g_small;

// Binary search in the (code point, cell) table, little-endian u16 pairs.
int CjkCell(uint32_t cp) {
    if (cp > 0xFFFF) return -1;
    const size_t entries = static_cast<size_t>(cjk_index_end - cjk_index_start) / 4;
    size_t lo = 0, hi = entries;
    auto code_at = [](size_t i) {
        return static_cast<uint32_t>(cjk_index_start[i * 4] | (cjk_index_start[i * 4 + 1] << 8));
    };
    while (lo < hi) {
        const size_t mid = (lo + hi) / 2;
        if (code_at(mid) < cp) lo = mid + 1;
        else hi = mid;
    }
    if (lo < entries && code_at(lo) == cp) {
        return cjk_index_start[lo * 4 + 2] | (cjk_index_start[lo * 4 + 3] << 8);
    }
    return -1;
}

uint32_t NextCodePoint(const std::string& text, size_t* i) {
    const auto* s = reinterpret_cast<const unsigned char*>(text.data());
    const unsigned char c = s[*i];
    const int extra = c < 0x80 ? 0 : (c >> 5) == 0x6 ? 1 : (c >> 4) == 0xE ? 2 : (c >> 3) == 0x1E ? 3 : -1;
    ++*i;
    if (extra < 0) return '?';
    uint32_t cp = extra == 0 ? c : c & (0x3F >> extra);
    for (int k = 0; k < extra; ++k) {
        if (*i >= text.size() || (s[*i] & 0xC0) != 0x80) return '?';
        cp = (cp << 6) | (s[*i] & 0x3F);
        ++*i;
    }
    return cp;
}

uint32_t AsciiOrSpace(uint32_t cp) {
    return cp >= assets::kFont16First && cp <= assets::kFont16Last ? cp : assets::kFont16First;
}

// Unscaled glyph box width and advance for the proportional font.
void PropMetrics(uint32_t cp, int* width, int* advance) {
    if (CjkCell(cp) >= 0) {
        *width = 16;
        *advance = kCjk16Advance;
        return;
    }
    *width = assets::kFont16Widths[AsciiOrSpace(cp) - assets::kFont16First];
    *advance = *width + 1;
}

void SmallMetrics(uint32_t cp, int* width, int* height, int* advance) {
    if (CjkCell(cp) >= 0) {
        *width = 12;
        *height = 12;
        *advance = kCjk12Advance;
        return;
    }
    *width = assets::kFont7Width;
    *height = assets::kFont7Height;
    *advance = assets::kFont7Width + 1;
}

bool PropPixel(uint32_t cp, int x, int y) {
    const int cell = CjkCell(cp);
    if (cell >= 0) {
        const uint8_t* g = hzk16_start + cell * kCjk16Bytes;
        const uint8_t b = x < 8 ? g[y * 2] : g[y * 2 + 1];
        return (b >> (7 - (x & 7))) & 1;
    }
    const uint16_t row = assets::kFont16Glyphs[AsciiOrSpace(cp) - assets::kFont16First][y];
    return (row & (0x8000u >> x)) != 0;
}

bool SmallPixel(uint32_t cp, int x, int y) {
    const int cell = CjkCell(cp);
    if (cell >= 0) {
        const uint8_t* g = hzk12_start + cell * kCjk12Bytes;
        const uint8_t b = x < 8 ? g[y * 2] : g[y * 2 + 1];
        return (b >> (7 - (x & 7))) & 1;
    }
    const uint32_t c = cp >= 0x20 && cp <= assets::kFont7Last ? cp : 0x20;
    return (assets::kFont7Glyphs[c - 0x20][y] >> (4 - x)) & 1;
}

int ScaleOf(const lv_font_t* font) {
    return static_cast<int>(reinterpret_cast<intptr_t>(font->user_data));
}

bool PropDsc(const lv_font_t* font, lv_font_glyph_dsc_t* dsc, uint32_t cp, uint32_t) {
    const int scale = ScaleOf(font);
    int width = 0, advance = 0;
    PropMetrics(cp, &width, &advance);
    dsc->adv_w = static_cast<uint16_t>(advance * scale);
    dsc->box_w = static_cast<uint16_t>(width * scale);
    dsc->box_h = static_cast<uint16_t>(16 * scale);
    dsc->ofs_x = 0;
    dsc->ofs_y = 0;
    dsc->stride = 0;
    dsc->format = LV_FONT_GLYPH_FORMAT_A1;
    dsc->is_placeholder = 0;
    dsc->gid.index = cp + 1;
    return true;
}

bool SmallDsc(const lv_font_t*, lv_font_glyph_dsc_t* dsc, uint32_t cp, uint32_t) {
    int width = 0, height = 0, advance = 0;
    SmallMetrics(cp, &width, &height, &advance);
    dsc->adv_w = static_cast<uint16_t>(advance);
    dsc->box_w = static_cast<uint16_t>(width);
    dsc->box_h = static_cast<uint16_t>(height);
    dsc->ofs_x = 0;
    dsc->ofs_y = static_cast<int16_t>(kSmallLine - height);  // tops aligned
    dsc->stride = 0;
    dsc->format = LV_FONT_GLYPH_FORMAT_A1;
    dsc->is_placeholder = 0;
    dsc->gid.index = cp + 1;
    return true;
}

template <bool kSmall>
const void* Bitmap(lv_font_glyph_dsc_t* dsc, lv_draw_buf_t* buf) {
    if (dsc->req_raw_bitmap || dsc->box_w == 0 || dsc->box_h == 0) return nullptr;
    const int scale = kSmall ? 1 : ScaleOf(dsc->resolved_font);
    const uint32_t cp = dsc->gid.index - 1;
    const uint32_t stride = lv_draw_buf_width_to_stride(dsc->box_w, LV_COLOR_FORMAT_A8);
    uint8_t* out = static_cast<uint8_t*>(buf->data);
    for (int y = 0; y < dsc->box_h; ++y) {
        for (int x = 0; x < dsc->box_w; ++x) {
            const bool on = kSmall ? SmallPixel(cp, x, y) : PropPixel(cp, x / scale, y / scale);
            out[y * stride + x] = on ? 0xFF : 0x00;
        }
    }
    lv_draw_buf_flush_cache(buf, nullptr);
    return buf;
}

struct IconImage {
    std::vector<uint8_t> data;
    lv_image_dsc_t dsc = {};
};
std::map<const assets::Icon*, IconImage> g_icons;

const lv_image_dsc_t* IconDsc(const assets::Icon& icon) {
    auto it = g_icons.find(&icon);
    if (it != g_icons.end()) return &it->second.dsc;
    IconImage& img = g_icons[&icon];
    const int stride = (icon.width + 7) / 8;
    img.data.assign(8 + stride * icon.height, 0);
    const uint32_t palette[2] = {0x00000000u, 0xFF000000u};  // transparent, black
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

template <typename WidthFn>
std::vector<std::string> Wrap(const std::string& text, int max_width, WidthFn width) {
    std::vector<std::string> lines;
    std::string current;
    size_t i = 0;
    while (i < text.size()) {
        while (i < text.size() && std::isspace(static_cast<unsigned char>(text[i]))) ++i;
        size_t end = i;
        while (end < text.size() && !std::isspace(static_cast<unsigned char>(text[end]))) ++end;
        if (end == i) break;
        std::string remaining = text.substr(i, end - i);
        i = end;
        while (true) {
            const std::string candidate = current.empty() ? remaining : current + " " + remaining;
            if (width(candidate) <= max_width) {
                current = candidate;
                break;
            }
            if (!current.empty()) {
                lines.push_back(current);
                current.clear();
                continue;
            }
            // A single word wider than the line: split it at a character.
            size_t split = remaining.size();
            while (split > 0 && width(remaining.substr(0, split)) > max_width) {
                do {
                    --split;
                } while (split > 0 && (static_cast<unsigned char>(remaining[split]) & 0xC0) == 0x80);
            }
            if (split == 0) {
                split = 1;
                while (split < remaining.size() &&
                       (static_cast<unsigned char>(remaining[split]) & 0xC0) == 0x80) {
                    ++split;
                }
            }
            lines.push_back(remaining.substr(0, split));
            remaining = remaining.substr(split);
            if (remaining.empty()) break;
        }
    }
    if (!current.empty()) lines.push_back(current);
    return lines;
}

}  // namespace

void Init() {
    for (int i = 0; i < kNumScales; ++i) {
        lv_font_t& f = g_prop[i];
        std::memset(&f, 0, sizeof(f));
        f.get_glyph_dsc = PropDsc;
        f.get_glyph_bitmap = Bitmap<false>;
        f.line_height = 16 * kScales[i];
        f.base_line = 0;
        f.user_data = reinterpret_cast<void*>(static_cast<intptr_t>(kScales[i]));
    }
    std::memset(&g_small, 0, sizeof(g_small));
    g_small.get_glyph_dsc = SmallDsc;
    g_small.get_glyph_bitmap = Bitmap<true>;
    g_small.line_height = kSmallLine;
    g_small.base_line = 0;
}

const lv_font_t* Prop(int scale) {
    for (int i = 0; i < kNumScales; ++i) {
        if (kScales[i] == scale) return &g_prop[i];
    }
    return &g_prop[0];
}

const lv_font_t* Small() { return &g_small; }

int PropWidth(const std::string& text, int scale) {
    int total = 0;
    for (size_t i = 0; i < text.size();) {
        int width = 0, advance = 0;
        PropMetrics(NextCodePoint(text, &i), &width, &advance);
        total += advance * scale;
    }
    return total;
}

int SmallWidth(const std::string& text) {
    int total = 0;
    for (size_t i = 0; i < text.size();) {
        int width = 0, height = 0, advance = 0;
        SmallMetrics(NextCodePoint(text, &i), &width, &height, &advance);
        total += advance;
    }
    return total;
}

int FitScale(const std::string& text, int max_width, int max_scale) {
    for (int scale = max_scale; scale > 1; --scale) {
        if (PropWidth(text, scale) <= max_width) return scale;
    }
    return 1;
}

std::string TruncateProp(const std::string& text, int max_width) {
    static const std::string kEllipsis = "\xE2\x80\xA6";
    const int ellipsis_w = PropWidth(kEllipsis, 1);
    int width = 0;
    for (size_t i = 0; i < text.size();) {
        const size_t start = i;
        int w = 0, advance = 0;
        PropMetrics(NextCodePoint(text, &i), &w, &advance);
        if (width + advance + ellipsis_w > max_width) {
            return text.substr(0, start) + kEllipsis;
        }
        width += advance;
    }
    return text;
}

std::vector<std::string> WrapProp(const std::string& text, int max_width) {
    return Wrap(text, max_width, [](const std::string& s) { return PropWidth(s, 1); });
}

std::vector<std::string> WrapSmall(const std::string& text, int max_width) {
    return Wrap(text, max_width, [](const std::string& s) { return SmallWidth(s); });
}

lv_obj_t* CreateIcon(lv_obj_t* parent, const assets::Icon& icon, int x, int y) {
    lv_obj_t* image = lv_image_create(parent);
    lv_image_set_src(image, IconDsc(icon));
    lv_obj_set_pos(image, x, y);
    return image;
}

}  // namespace fonts
