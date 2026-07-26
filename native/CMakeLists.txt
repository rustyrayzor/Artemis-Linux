cmake_minimum_required(VERSION 3.16)
project(artemis_moonlight_shim LANGUAGES C)

set(BUILD_SHARED_LIBS OFF CACHE BOOL "" FORCE)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)

add_subdirectory(
  "${CMAKE_CURRENT_SOURCE_DIR}/../vendor/moonlight-common-c"
  "${CMAKE_CURRENT_BINARY_DIR}/moonlight-common-c"
)

add_library(artemis_moonlight_shim STATIC artemis_moonlight.c)
target_compile_features(artemis_moonlight_shim PRIVATE c_std_11)
target_include_directories(
  artemis_moonlight_shim
  PUBLIC "${CMAKE_CURRENT_SOURCE_DIR}"
  PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}/../vendor/moonlight-common-c/src"
)
target_link_libraries(artemis_moonlight_shim PUBLIC moonlight-common-c)

if(CMAKE_C_COMPILER_ID MATCHES "GNU|Clang")
  target_compile_options(
    artemis_moonlight_shim
    PRIVATE -Wall -Wextra -Wpedantic -Werror
  )
endif()

install(
  TARGETS artemis_moonlight_shim moonlight-common-c enet
  ARCHIVE DESTINATION lib
)
install(FILES artemis_moonlight.h DESTINATION include)
