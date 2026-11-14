# test.sh builds this against the SDK installed by its CMakeLists.txt: the package is usable.
cmake_minimum_required(VERSION 3.20)
project(consumer LANGUAGES CXX)
find_package(sidecarzero 0.1 REQUIRED)
add_executable(consumer main.cpp)
target_link_libraries(consumer PRIVATE sidecarzero::sidecarzero)
