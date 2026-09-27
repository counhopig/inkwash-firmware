# Inkwash firmware for the Zectrix Note 4 (ESP32-S3), C++ on ESP-IDF 5.5.
cmake_minimum_required(VERSION 3.16)

# The e-paper driver is shared with the Rust firmware until it is retired.
set(EXTRA_COMPONENT_DIRS "${CMAKE_CURRENT_LIST_DIR}/../rust-firmware/components/zectrix_epd")

include($ENV{IDF_PATH}/tools/cmake/project.cmake)
project(inkwash)
