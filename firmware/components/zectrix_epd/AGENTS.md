# SSD2683 panel driver

## OVERVIEW

Synchronous ESP-IDF component for 400 × 300 monochrome and 16-gray e-paper refresh.

## WHERE TO LOOK

| Task | File | Notes |
| --- | --- | --- |
| Public API and formats | `include/zectrix_epd.h` | Opaque C handle, configuration and frame contracts |
| SPI/GPIO lifecycle | `zectrix_epd.cc` | Allocation, bus ownership, rails and teardown |
| Refresh transactions | `zectrix_epd.cc` | Full B/W, partial transition and grayscale passes |
| Calibrated gray waveform | `private_include/ssd2683_waveform.h` | Private controller data |
| Build flags | `CMakeLists.txt` | Extra warning checks treated as errors |

## CONVENTIONS

- Public operations return `esp_err_t`; mutex guards serialize controller operations.
- 1bpp frames: row-major, MSB first, 1 = white; full buffer exactly 15000 bytes.
- Partial input rows use `(width + 7) / 8` bytes; destination windows align to byte boundaries.
- 4bpp frames: two pixels per byte, left pixel in high nibble, 0 = black and 15 = white; exactly 60000 bytes.
- Large SPI writes copy through internal DMA-capable staging memory; short writes use transaction inline data.
- BUSY is active low; waits yield and return timeout errors.
- A driver may own its SPI bus or attach to a caller-owned bus; teardown releases only owned resources.

## ANTI-PATTERNS

- Do not pass a non-DMA framebuffer directly to large SPI transactions (`zectrix_epd.cc:152`).
- Do not destroy and recreate the SPI bus for each temperature read (`zectrix_epd.cc:310`).
- Do not refresh partially without powered, ready controller and valid shadow (`zectrix_epd.cc:696`).
- Do not retain shadow validity after an interrupted partial transition (`zectrix_epd.cc:750`).
- Do not use a grayscale result as a B/W partial baseline (`zectrix_epd.cc:796`).

## NOTES

- Shadow seeding declares the exact image already on the glass; it performs no refresh.
- Temperature setup uses the 25°C fallback and one long-lived TX SPI bus.
- Successful full B/W refresh establishes shadow; failed full refresh invalidates it.
- Default BUSY timeout is 2000 ms; individual operations may provide specific limits.
- Panel output and waveform changes require visual hardware validation.
