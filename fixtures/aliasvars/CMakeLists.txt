# Fixture for the alias-variables lint (roadmap Track F).
cmake_minimum_required(VERSION 3.25)
project(aliasvars NONE)

# Positive (shape 1): pure alias — set from a single ${} reference,
# source never rewritten, alias read afterwards.
set(BASE_LABEL "demo")
set(ALIAS_LABEL ${BASE_LABEL})
message(STATUS "alias: ${ALIAS_LABEL}")

# Negative: the source is rewritten between the alias write and the
# read — the copy is a deliberate snapshot, not an alias.
set(SOURCE_VAL "one")
set(STALE_COPY ${SOURCE_VAL})
set(SOURCE_VAL "two")
message(STATUS "stale: ${STALE_COPY}")

# Negative: composite value `${...}extra` — fails the exact-shape check.
set(STEM "base")
set(COMPOSITE_VAL ${STEM}extra)
message(STATUS "composite: ${COMPOSITE_VAL}")

# Negative: alias also referenced in a branch that never executed
# (the ast_var_refs cross-check, B1 guard).
set(ROOT_LABEL "root")
set(MIRROR_LABEL ${ROOT_LABEL})
message(STATUS "mirror: ${MIRROR_LABEL}")
if(ALIASVARS_EXTRA_MODE)
  message(STATUS "extra: ${MIRROR_LABEL}")
endif()

# Positive (shape 2): PARENT_SCOPE round-trip — the function writes back
# the value it inherited from here, unchanged.
set(RELEASE_TAG "v1")
function(refresh_tag)
  set(RELEASE_TAG ${RELEASE_TAG} PARENT_SCOPE)
endfunction()
refresh_tag()
message(STATUS "tag: ${RELEASE_TAG}")

# Negative (shape 2): a real export — the function computes a local
# value first, so the PARENT_SCOPE write resolves to the local set.
function(compute_stamp)
  set(BUILD_STAMP "stamp-value")
  set(BUILD_STAMP ${BUILD_STAMP} PARENT_SCOPE)
endfunction()
compute_stamp()
message(STATUS "stamp: ${BUILD_STAMP}")
