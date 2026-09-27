idf_component_register(
    SRCS
        "main.cc"
        "board.cc"
        "keys.cc"
        "pcf8563.cc"
        "display.cc"
        "fonts.cc"
        "core/boot_guard.cc"
        "core/codec.cc"
        "core/datetime.cc"
        "core/protocol.cc"
        "core/schedule.cc"
        "core/sync_payload.cc"
        "storage/store.cc"
        "net/wifi.cc"
        "net/sync.cc"
        "audio/tones.cc"
        "control/usb_console.cc"
        "control/ble.cc"
        "power/power.cc"
        "ui/screens.cc"
        "app/app.cc"
        "app/safe_mode.cc"
    INCLUDE_DIRS "."
    REQUIRES
        driver
        esp_adc
        esp_driver_i2c
        esp_driver_i2s
        esp_driver_gpio
        esp_driver_usb_serial_jtag
        esp_timer
        esp_pm
        esp_wifi
        esp_netif
        esp_event
        esp_http_client
        mbedtls
        nvs_flash
        json
        bt
        lvgl
        zectrix_epd
    EMBED_FILES
        "../../rust-firmware/assets/hzk16.bin"
        "../../rust-firmware/assets/hzk12.bin"
        "../../rust-firmware/assets/cjk_index.bin"
)

target_compile_features(${COMPONENT_LIB} PUBLIC cxx_std_17)
target_compile_options(${COMPONENT_LIB} PRIVATE -Wall -Wextra -Werror -Wno-missing-field-initializers)

# The build time seeds the RTC after a power loss; the version is shown on
# the About screen.
string(TIMESTAMP INKWASH_BUILD_EPOCH "%s" UTC)
execute_process(
    COMMAND git describe --always --dirty
    WORKING_DIRECTORY ${CMAKE_CURRENT_LIST_DIR}
    OUTPUT_VARIABLE INKWASH_GIT_REV
    OUTPUT_STRIP_TRAILING_WHITESPACE
    ERROR_QUIET)
if(NOT INKWASH_GIT_REV)
    set(INKWASH_GIT_REV "unknown")
endif()
target_compile_definitions(${COMPONENT_LIB} PRIVATE
    INKWASH_BUILD_EPOCH=${INKWASH_BUILD_EPOCH}ULL
    INKWASH_VERSION="0.7.0"
    INKWASH_GIT_REV="${INKWASH_GIT_REV}")
