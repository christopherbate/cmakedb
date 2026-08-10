# Fixture for the shadowed-requirements lint pass: the same requirement
# reaching a target via multiple routes.
cmake_minimum_required(VERSION 3.25)
project(shadowedreqs C)

# Directory-scope include, re-declared per-target below (positive 1).
include_directories(${CMAKE_CURRENT_SOURCE_DIR}/include)

add_library(dep STATIC dep.c)
# PUBLIC define on the dependency, re-declared directly on app below
# (positive 2, one-hop transitive route).
target_compile_definitions(dep PUBLIC SHARED_DEF=1)
# Genex-valued PUBLIC define: config-dependent, must never be flagged.
target_compile_definitions(dep PUBLIC $<$<CONFIG:Debug>:GXDEF=1>)

add_executable(app app.c)
target_link_libraries(app PRIVATE dep)

# Positive 1: duplicates the directory-scope include above (same file).
target_include_directories(app PRIVATE ${CMAKE_CURRENT_SOURCE_DIR}/include)
# Positive 2: duplicates dep's PUBLIC define (same file).
target_compile_definitions(app PRIVATE SHARED_DEF=1)
# Negative: genex duplicate of dep's PUBLIC genex define — skipped.
target_compile_definitions(app PRIVATE $<$<CONFIG:Debug>:GXDEF=1>)
# Negative: unrelated include dir, only one route.
target_include_directories(app PRIVATE ${CMAKE_CURRENT_SOURCE_DIR}/other)

# Negative: subdirectory target re-declaring the parent directory-scope
# include — standalone-build heuristic (different directories' files).
add_subdirectory(sub)
