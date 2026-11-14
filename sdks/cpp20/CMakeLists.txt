# The C++20 SDK as a CMake package: a header-only INTERFACE library. The Makefile next to this
# file builds the conformance agent and the tests; this file is for applications.
#
#   In the application's CMakeLists.txt, either
#     add_subdirectory(path/to/sdks/cpp20)                     # vendored, or FetchContent
#   or, after `cmake -S sdks/cpp20 -B build && cmake --install build --prefix /opt/scz`,
#     find_package(sidecarzero 0.1 REQUIRED)                   # with CMAKE_PREFIX_PATH=/opt/scz
#   then
#     target_link_libraries(app PRIVATE sidecarzero::sidecarzero)
cmake_minimum_required(VERSION 3.20)
project(sidecarzero VERSION 0.1.0 LANGUAGES CXX
        DESCRIPTION "SidecarZero C++20 client: model inference through shared memory, with a fallback that always answers")

include(GNUInstallDirs)
include(CMakePackageConfigHelpers)

find_package(Threads REQUIRED)

add_library(sidecarzero INTERFACE)
add_library(sidecarzero::sidecarzero ALIAS sidecarzero)
target_compile_features(sidecarzero INTERFACE cxx_std_20)
target_link_libraries(sidecarzero INTERFACE Threads::Threads)
target_include_directories(sidecarzero INTERFACE
  $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>
  $<INSTALL_INTERFACE:${CMAKE_INSTALL_INCLUDEDIR}>)

install(DIRECTORY include/scz DESTINATION ${CMAKE_INSTALL_INCLUDEDIR} FILES_MATCHING PATTERN "*.hpp")
install(TARGETS sidecarzero EXPORT sidecarzeroTargets)
install(EXPORT sidecarzeroTargets NAMESPACE sidecarzero:: DESTINATION ${CMAKE_INSTALL_LIBDIR}/cmake/sidecarzero)
configure_package_config_file(cmake/sidecarzeroConfig.cmake.in ${CMAKE_CURRENT_BINARY_DIR}/sidecarzeroConfig.cmake
  INSTALL_DESTINATION ${CMAKE_INSTALL_LIBDIR}/cmake/sidecarzero)
# The ABI's minor versions are compatible within a major (shm-abi §8): so are the SDK's.
write_basic_package_version_file(${CMAKE_CURRENT_BINARY_DIR}/sidecarzeroConfigVersion.cmake COMPATIBILITY SameMajorVersion ARCH_INDEPENDENT)
install(FILES ${CMAKE_CURRENT_BINARY_DIR}/sidecarzeroConfig.cmake ${CMAKE_CURRENT_BINARY_DIR}/sidecarzeroConfigVersion.cmake
  DESTINATION ${CMAKE_INSTALL_LIBDIR}/cmake/sidecarzero)
