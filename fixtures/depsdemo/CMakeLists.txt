# Dependency-inventory fixture for `cmakedb deps`: NONE language and
# offline-safe — a stub find module that succeeds, a QUIET miss, and
# declare-only FetchContent (never FetchContent_MakeAvailable) so the
# configure needs neither network nor compiler.
cmake_minimum_required(VERSION 3.25)
project(depsdemo NONE)

list(APPEND CMAKE_MODULE_PATH "${CMAKE_CURRENT_SOURCE_DIR}/cmake")

# Module-mode hit: cmake/FindStub.cmake sets Stub_FOUND / Stub_VERSION,
# a Stub_DIR cache hint, and an imported target -> "found".
find_package(Stub 1.0 QUIET COMPONENTS core)

# No find module, no config package -> "not found" (QUIET keeps the
# configure clean).
find_package(NoSuchPackage QUIET)

include(FetchContent)

# Pinned to a full 40-hex commit on github -> pinned-commit, and the one
# entry that earns a pkg:github purl in the CycloneDX export.
FetchContent_Declare(pinneddep
  GIT_REPOSITORY https://github.com/example/pinneddep.git
  GIT_TAG 0123456789abcdef0123456789abcdef01234567)

# Tracks a branch -> mutable-ref, no purl.
FetchContent_Declare(trackingdep
  GIT_REPOSITORY https://github.com/example/trackingdep.git
  GIT_TAG main)

# An imported target whose namespace matches no find_package -> listed
# under "imported targets with no matching find_package".
add_library(Orphan::helper INTERFACE IMPORTED)
