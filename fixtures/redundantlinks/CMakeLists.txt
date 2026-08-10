# Fixture for the redundant-links lint (redundantlinks_e2e.rs).
cmake_minimum_required(VERSION 3.25)
project(redundantlinks LANGUAGES C)

# Positive (one hop): app links bcore directly, but bcore already
# arrives via amid's PUBLIC interface.
add_library(bcore STATIC lib.c)
add_library(amid STATIC lib.c)
target_link_libraries(amid PUBLIC bcore)
add_executable(app main.c)
target_link_libraries(app PRIVATE amid bcore)

# Negative: `m` also arrives via amid's PUBLIC interface, but it is an
# external/string destination (no real target) — never flagged.
target_link_libraries(amid PUBLIC m)
target_link_libraries(app PRIVATE m)

# Positive (two hops): deep arrives via top2 -> mid2 -> deep.
add_library(deep STATIC lib.c)
add_library(mid2 STATIC lib.c)
target_link_libraries(mid2 PUBLIC deep)
add_library(top2 STATIC lib.c)
target_link_libraries(top2 PUBLIC mid2)
add_executable(app2 main.c)
target_link_libraries(app2 PRIVATE top2 deep)

# Negative: priva links privb PRIVATE — no interface route, so app3's
# direct privb edge is load-bearing.
add_library(privb STATIC lib.c)
add_library(priva STATIC lib.c)
target_link_libraries(priva PRIVATE privb)
add_executable(app3 main.c)
target_link_libraries(app3 PRIVATE priva privb)

# Positive (INTERFACE-library hop): bcore2 arrives via iface's
# INTERFACE link.
add_library(bcore2 STATIC lib.c)
add_library(iface INTERFACE)
target_link_libraries(iface INTERFACE bcore2)
add_executable(app4 main.c)
target_link_libraries(app4 PRIVATE iface bcore2)
