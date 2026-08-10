cmake_minimum_required(VERSION 3.25)
project(envdep NONE)

# Ambient env read: no set(ENV{...}) precedes it -> flagged.
set(tool_root "$ENV{MY_CUSTOM_TOOL_ROOT}")
message(STATUS "tool root: ${tool_root}")

# Ambient env probe in a condition -> flagged.
if(DEFINED ENV{MY_FEATURE_FLAG})
  message(STATUS "feature flag present")
endif()

# Project-internal: written before it is read -> must NOT be flagged.
set(ENV{INTERNAL_V} v)
set(y "$ENV{INTERNAL_V}")
message(STATUS "internal: ${y}")

# Expected-ambient allowlist -> must NOT be flagged.
set(p "$ENV{PATH}")
