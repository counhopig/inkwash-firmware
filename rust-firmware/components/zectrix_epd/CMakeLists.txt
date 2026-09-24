idf_component_register(
    SRCS "zectrix_epd.cc"
    INCLUDE_DIRS "include"
    PRIV_INCLUDE_DIRS "private_include"
    REQUIRES driver esp_timer
)

target_compile_features(${COMPONENT_LIB} PUBLIC cxx_std_17)
# ESP-IDF's defaults silence sign-compare and unused-parameter and keep
# -Wextra advisory; this driver compiles clean without those allowances.
target_compile_options(${COMPONENT_LIB} PRIVATE
    -Wextra -Wsign-compare -Wunused-parameter -Wshadow -Werror)
