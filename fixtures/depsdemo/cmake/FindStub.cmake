# Offline stand-in for a find module: succeeds unconditionally and
# leaves the full evidence trail `cmakedb deps` reads back — the _FOUND
# and _VERSION writes, a _DIR cache hint, and an imported target.
set(Stub_FOUND TRUE)
set(Stub_VERSION 1.2.3)
set(Stub_DIR "${CMAKE_CURRENT_LIST_DIR}" CACHE PATH "location of the stub package")
if(NOT TARGET Stub::core)
  add_library(Stub::core INTERFACE IMPORTED)
endif()
