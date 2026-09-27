idf_component_register(
    SRCS
        "main.cc"
        "board.cc"
        "pcf8563.cc"
        "display.cc"
        "fonts.cc"
        "ui/home_screen.cc"
    INCLUDE_DIRS "."
    REQUIRES
        driver
        esp_adc
        esp_driver_i2c
        esp_timer
        esp_pm
        lvgl
        zectrix_epd
)

target_compile_features(${COMPONENT_LIB} PUBLIC cxx_std_17)
target_compile_options(${COMPONENT_LIB} PRIVATE -Wall -Wextra -Werror -Wno-missing-field-initializers)
