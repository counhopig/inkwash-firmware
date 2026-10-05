idf_component_register(
    SRCS
        "main.cc"
        "board.cc"
        "keys.cc"
        "pcf8563.cc"
        "display.cc"
        "fonts.cc"
        "core/boot_guard.cc"
        "core/sleep_trace.cc"
        "core/event_journal.cc"
        "diagnostics/event_log.cc"
        "core/power_policy.cc"
        "core/refresh_policy.cc"
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
        "app/state.cc"
        "app/view.cc"
        "app/alarms.cc"
        "app/reminders.cc"
        "app/commands.cc"
        "app/sync.cc"
        "app/pairing.cc"
        "app/input.cc"
        "app/lifecycle.cc"
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
        esp_partition
        esp_pm
        esp_wifi
        esp_netif
        esp_event
        esp_http_client
        mbedtls
        nvs_flash
        espcoredump
        json
        bt
        lvgl
        zectrix_epd
    EMBED_FILES
        "../assets/hzk16.bin"
        "../assets/hzk12.bin"
        "../assets/cjk_index.bin"
)

target_compile_features(${COMPONENT_LIB} PUBLIC cxx_std_17)
target_compile_options(${COMPONENT_LIB} PRIVATE -Wall -Wextra -Werror -Wno-missing-field-initializers)

# The build time seeds the RTC after a power loss; the version is shown on
# the About screen.
if(DEFINED ENV{SOURCE_DATE_EPOCH} AND "$ENV{SOURCE_DATE_EPOCH}" MATCHES "^[0-9]+$")
    set(INKWASH_BUILD_EPOCH "$ENV{SOURCE_DATE_EPOCH}")
else()
    string(TIMESTAMP INKWASH_BUILD_EPOCH "%s" UTC)
endif()
execute_process(
    COMMAND git describe --always --dirty --tags
    WORKING_DIRECTORY ${CMAKE_CURRENT_LIST_DIR}
    OUTPUT_VARIABLE INKWASH_GIT_REV
    OUTPUT_STRIP_TRAILING_WHITESPACE
    ERROR_QUIET)
if(NOT INKWASH_GIT_REV)
    set(INKWASH_GIT_REV "unknown")
endif()
target_compile_definitions(${COMPONENT_LIB} PRIVATE
    INKWASH_BUILD_EPOCH=${INKWASH_BUILD_EPOCH}ULL
    INKWASH_VERSION="0.7.1"
    INKWASH_GIT_REV="${INKWASH_GIT_REV}")
