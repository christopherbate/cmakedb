add_library(net STATIC net.c)
target_link_libraries(net PUBLIC http)
target_compile_definitions(net PUBLIC NET_VERSION=2)
