# Fixture for `cmakedb profile`: a function called repeatedly from a
# foreach so the scope section has something to aggregate (11 calls),
# plus a spread of leaf commands for the self-time sections. Timings just
# need to exist — tests assert on structure, never on absolute values.
cmake_minimum_required(VERSION 3.25)
project(profiledemo NONE)

function(build_label out prefix)
  set(acc "")
  foreach(i RANGE 20)
    string(APPEND acc "${prefix}-${i};")
  endforeach()
  set(${out} "${acc}" PARENT_SCOPE)
endfunction()

foreach(round RANGE 10)
  build_label(LABELS "round${round}")
endforeach()

list(LENGTH LABELS n)
message(STATUS "built ${n} labels")
