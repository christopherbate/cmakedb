cmake_minimum_required(VERSION 3.25)
project(linkgraph C)

add_library(core STATIC core.c)
add_library(util STATIC util.c)
add_library(extra STATIC extra.c)
add_executable(app main.c)

# Conflicting-visibility duplicate: util is linked PUBLIC and PRIVATE.
target_link_libraries(core PUBLIC util)
target_link_libraries(core PRIVATE util)

# Benign same-visibility duplicate: extra listed twice in one call.
target_link_libraries(app PRIVATE extra extra)

# Two-cycle between static libraries (legal in CMake): util -> core -> util.
target_link_libraries(util PRIVATE core)

# The per-config keyword idiom must NOT count as a duplicate.
target_link_libraries(app PRIVATE optimized core debug core)
