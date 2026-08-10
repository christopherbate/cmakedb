# Cache/dependency hygiene fixture: set-cache-force and
# fetchcontent-pinning shapes. NONE language + declare-only FetchContent
# (never FetchContent_MakeAvailable) keeps the configure fast and offline.
cmake_minimum_required(VERSION 3.25)
project(hygiene NONE)

include(FetchContent)

# Classic stomp: a user-facing STRING cache entry force-overwritten on
# every configure -> set-cache-force warning.
set(HYGIENE_MODE "release" CACHE STRING "build mode" FORCE)

# INTERNAL caches are bookkeeping; FORCE is idiomatic there -> no finding.
set(HYGIENE_STAMP "v1" CACHE INTERNAL "configure stamp" FORCE)

# Guard idiom: the FORCE write only runs while the entry is absent ->
# demoted to note.
if(NOT DEFINED HYGIENE_GUARDED)
  set(HYGIENE_GUARDED "default" CACHE STRING "guarded default" FORCE)
endif()

# Tracks a branch -> fetchcontent-pinning note (moving ref).
FetchContent_Declare(tracking_dep
  GIT_REPOSITORY https://example.invalid/tracking_dep.git
  GIT_TAG main)

# Pinned to a full 40-hex commit -> no finding.
FetchContent_Declare(pinned_dep
  GIT_REPOSITORY https://example.invalid/pinned_dep.git
  GIT_TAG 0123456789abcdef0123456789abcdef01234567)

# Archive download without URL_HASH -> fetchcontent-pinning note.
FetchContent_Declare(tarball_dep
  URL https://example.invalid/tarball_dep-1.0.tar.gz)

# URL + URL_HASH is pinned -> no finding.
FetchContent_Declare(hashed_dep
  URL https://example.invalid/hashed_dep-1.0.tar.gz
  URL_HASH SHA256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef)
