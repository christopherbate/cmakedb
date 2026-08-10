add_library(subtool STATIC subtool.c)
# Re-declares the parent's directory-scope include so sub/ also builds
# standalone — must NOT be flagged (same-directory heuristic).
target_include_directories(subtool PRIVATE ${CMAKE_SOURCE_DIR}/include)
