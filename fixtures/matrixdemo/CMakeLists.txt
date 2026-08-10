# Fixture for `cmakedb matrix` (mirrors fixtures/multiconfig): FEATURE_X
# is read only under the "extra" preset (WITH_EXTRA_READS=ON), so the
# matrix intersection must drop it; ALWAYS_DEAD survives every preset.
cmake_minimum_required(VERSION 3.25)
project(matrixdemo NONE)

option(FEATURE_X "read only when extras are on" OFF)
option(ALWAYS_DEAD "read in no configuration" OFF)
if(WITH_EXTRA_READS)
  if(FEATURE_X)
    message(STATUS "feature x enabled")
  endif()
endif()
