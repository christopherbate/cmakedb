# Fixture for the directory-commands lint pass (report-only demotion).
cmake_minimum_required(VERSION 3.25)
project(dircommands C)

# Positive: directory-scope commands affecting the targets below (and the
# subdirectory's target via inheritance).
include_directories(${CMAKE_CURRENT_SOURCE_DIR}/include)
add_definitions(-DDIRCMD_LEGACY=1)

add_library(util STATIC util.c)
add_executable(app app.c)

# Negative: target-scoped call — must NOT be flagged.
target_include_directories(app PRIVATE ${CMAKE_CURRENT_SOURCE_DIR}/appinc)

# Utility target: never compiles, must not appear in any affected set.
add_custom_target(stamp COMMAND ${CMAKE_COMMAND} -E true)

# Subdirectory inheriting the directory-scope commands above; its target
# must be listed in the affected sets (inheritance case).
add_subdirectory(sub)

# Retroactive command after the targets: still affects util and app
# (same directory), but NOT subtool — sub/ snapshotted its directory
# properties at add_subdirectory time.
add_compile_definitions(DIRCMD_TRAILING=1)

# Dead directory command: link_libraries is order-sensitive and no target
# is defined after it, so it affects nothing.
link_libraries(dircmd_nolib)
