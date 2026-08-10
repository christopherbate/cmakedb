add_library(http STATIC http.c)
target_include_directories(http PUBLIC ${CMAKE_CURRENT_SOURCE_DIR}/include)
target_link_libraries(http INTERFACE ZLIB::ZLIB)
