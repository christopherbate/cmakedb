# constant-conditions fixture: repeated condition evaluations that never
# vary (flagged) next to legitimately varying / literal ones (not flagged).
cmake_minimum_required(VERSION 3.25)
project(constconds NONE)

set(INVARIANT_FLAG ON)

foreach(item alpha beta gamma delta)
  # FLAG: loop-invariant condition — same value on all 4 iterations.
  if(INVARIANT_FLAG)
    set(TOOK_INVARIANT_BRANCH 1)
  endif()
  # no flag: depends on the loop variable (expanded args differ per iteration).
  if("${item}" STREQUAL "beta")
    set(FOUND_BETA 1)
  endif()
  # no flag: bare read of the loop variable (invariance unprovable — the
  # loop variable's values share one synthesized write).
  if(item STREQUAL "gamma")
    set(FOUND_GAMMA 1)
  endif()
  # no flag: literal condition, references no variables — deliberate.
  if(TRUE)
    set(ALWAYS_TAKEN 1)
  endif()
endforeach()

set(GLOBAL_MODE "strict")
function(check_mode label)
  # FLAG: call-invariant condition on a global that never changed.
  if(GLOBAL_MODE STREQUAL "strict")
    set(${label}_STRICT 1 PARENT_SCOPE)
  endif()
  # no flag: depends on the per-call argument.
  if("${label}" STREQUAL "two")
    set(SAW_TWO 1 PARENT_SCOPE)
  endif()
endfunction()
check_mode(one)
check_mode(two)
check_mode(three)

function(mark_done flag_name)
  # no flag: ${flag_name} expands to a variable NAME which if() then
  # dereferences — that second-order value varies (undefined on the
  # first call only) but is invisible to read tracking.
  if(NOT ${flag_name})
    set(${flag_name} 1 PARENT_SCOPE)
  endif()
endfunction()
mark_done(done_marker)
mark_done(done_marker)
mark_done(done_marker)

foreach(i RANGE 3)
  # no flag: undefined on every iteration (defined only after the loop)
  # — undefined-reads territory, not a constant condition.
  if(LATER_DEFINED)
    set(NEVER_REACHED 1)
  endif()
endforeach()
set(LATER_DEFINED 1)

message(STATUS "${TOOK_INVARIANT_BRANCH}${FOUND_BETA}${FOUND_GAMMA}")
message(STATUS "${ALWAYS_TAKEN}${one_STRICT}${SAW_TWO}")
message(STATUS "${done_marker}${LATER_DEFINED}${NEVER_REACHED}")
