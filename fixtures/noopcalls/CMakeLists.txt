# Fixture for the noop-calls lint (roadmap Track F).
cmake_minimum_required(VERSION 3.25)
project(noopcalls C)

add_library(app STATIC app.c)

# Positive: EXTRA_LIBS is never written anywhere -> no-op call with the
# "never set" enrichment (pairs with undefined-reads).
target_link_libraries(app ${EXTRA_LIBS})

# Positive: literally-empty call, distinct phrasing ("has no arguments").
add_definitions()

# Positive: list(APPEND V) with no items to add.
set(FLAGS "")
list(APPEND FLAGS)

# Positive: the variable is populated only in a branch that never ran
# (the B1 enrichment).
if(NOOPCALLS_EXTRA_MODE)
  set(COND_DEFS "EXTRA_MODE")
endif()
target_compile_definitions(app PRIVATE ${COND_DEFS})

# Negative: the list variable IS populated -> real arguments.
set(APP_DEFS "APP_ON")
target_compile_definitions(app PRIVATE ${APP_DEFS})

# Negative: a quoted empty-string argument is a real (empty) argument by
# the json-v1 arg-kind rule -- and carries no ${} reference either.
target_compile_definitions(app PRIVATE "")
