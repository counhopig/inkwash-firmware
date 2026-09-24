idf_component_register(SRCS "recorder.c" INCLUDE_DIRS "." REQUIRES esp_system WHOLE_ARCHIVE)
target_compile_options(${COMPONENT_LIB} PRIVATE
    -Wextra -Wsign-compare -Wunused-parameter -Wshadow -Werror)

# 验证固件闸门：仅当 INKWASH_P06_VALIDATE=1 时启用 V2 故障触发。
# 默认构建不定义该宏 => 组件内构造函数不做任何触发动作（行为不变）。
if(DEFINED ENV{INKWASH_P06_VALIDATE} AND "$ENV{INKWASH_P06_VALIDATE}" STREQUAL "1")
    target_compile_definitions(${COMPONENT_LIB} PRIVATE P06_VALIDATE_V2=1)
    message(STATUS "p06_recorder: V2 故障触发已启用")
else()
    message(STATUS "p06_recorder: 仅编译，不启用触发")
endif()
