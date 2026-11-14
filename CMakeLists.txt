cmake_minimum_required(VERSION 3.21)
project(NXFormat VERSION 1.0.4 LANGUAGES CXX)

include(CTest)
# Optional single-header C++17 reader target; no writer or conversion core is
# introduced into emulator read builds.
add_library(nxcore_reader INTERFACE)
target_include_directories(nxcore_reader INTERFACE
  "$<BUILD_INTERFACE:${PROJECT_SOURCE_DIR}/reader>"
  "$<INSTALL_INTERFACE:include>"
)
target_compile_features(nxcore_reader INTERFACE cxx_std_17)
add_library(nxcore::reader ALIAS nxcore_reader)

option(NX_ENABLE_RUNTIME_READER "Expose the optional persistent threaded reader target" OFF)
if(NX_ENABLE_RUNTIME_READER)
  find_package(Threads REQUIRED)
  add_library(nxcore_runtime INTERFACE)
  target_link_libraries(nxcore_runtime INTERFACE nxcore::reader Threads::Threads)
  if(WIN32)
    target_link_libraries(nxcore_runtime INTERFACE bcrypt)
  elseif(NOT APPLE)
    find_package(OpenSSL QUIET COMPONENTS Crypto)
    if(OpenSSL_FOUND)
      target_compile_definitions(nxcore_runtime INTERFACE NX_USE_OPENSSL=1)
      target_link_libraries(nxcore_runtime INTERFACE OpenSSL::Crypto)
    endif()
  endif()
  add_library(nxcore::runtime ALIAS nxcore_runtime)
endif()

if(BUILD_TESTING)
  add_executable(nx_reader_cxx17_smoke tests/conformance/reader_cxx17_smoke.cpp)
  target_link_libraries(nx_reader_cxx17_smoke PRIVATE nxcore::reader)
  add_test(NAME nx_reader_cxx17_smoke COMMAND nx_reader_cxx17_smoke)
endif()
