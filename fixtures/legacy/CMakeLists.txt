cmake_minimum_required(VERSION 3.20)
project(legacy C)
# legacy directory-scope commands to modernize
include_directories(${CMAKE_CURRENT_SOURCE_DIR}/include)
add_definitions(-DLEGACY_MODE=1)
add_library(core STATIC core.c)   # keep this trailing comment
add_subdirectory(sub)
