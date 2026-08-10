# Fixture for the single-use-variables lint (roadmap Track F).
cmake_minimum_required(VERSION 3.25)
project(singleuse NONE)

# Positive: set once, read once -> finding.
set(GREETING "hello from singleuse")
message(STATUS "greeting: ${GREETING}")

# Negative: read at two sites.
set(TWICE_READ "x")
message(STATUS "first: ${TWICE_READ}")
message(STATUS "second: ${TWICE_READ}")

# Negative: one executed read, but a second reference sits in a branch
# that never executed (the ast_var_refs cross-check, B1 guard).
set(MAYBE_SHARED "y")
message(STATUS "maybe: ${MAYBE_SHARED}")
if(SINGLEUSE_EXTRA_MODE)
  message(STATUS "extra: ${MAYBE_SHARED}")
endif()

# Negative: function-local set + read (idiomatic locals, never flagged).
function(emit_note)
  set(local_note "inside function")
  message(STATUS "note: ${local_note}")
endfunction()
emit_note()

# Negative: set inside a foreach body (loop guard).
foreach(item IN ITEMS solo)
  set(loop_val "${item}")
endforeach()
message(STATUS "loop: ${loop_val}")
