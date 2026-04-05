cmake_minimum_required(VERSION 3.22)
project(xrpl_shim CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)

# The shim links against libxrpl (built inside vendor/rippled/.build)
# and the conan-provided C++ dependencies (boost, openssl, etc).

# Point to the vendored rippled build dir
set(RIPPLED_BUILD_DIR ${CMAKE_CURRENT_SOURCE_DIR}/vendor/rippled/.build)
set(RIPPLED_SRC_DIR ${CMAKE_CURRENT_SOURCE_DIR}/vendor/rippled)

# Pull in conan's generated find_package helpers
list(PREPEND CMAKE_PREFIX_PATH ${RIPPLED_BUILD_DIR}/build/generators)
list(PREPEND CMAKE_MODULE_PATH ${RIPPLED_BUILD_DIR}/build/generators)

# Find the conan-provided deps that libxrpl pulls in transitively
find_package(Boost REQUIRED)
find_package(OpenSSL REQUIRED)
find_package(protobuf REQUIRED)
find_package(date REQUIRED)
find_package(xxHash REQUIRED)
find_package(ed25519 REQUIRED)
find_package(secp256k1 REQUIRED)

# Shim library
add_library(xrpl_shim STATIC
    xrpl_shim.cpp
    MinimalServiceRegistry.cpp
    CallbackReadView.cpp
)
target_include_directories(xrpl_shim PUBLIC
    ${CMAKE_CURRENT_SOURCE_DIR}
    ${RIPPLED_SRC_DIR}/include
    ${RIPPLED_BUILD_DIR}/proto
    ${RIPPLED_BUILD_DIR}/libxrpl
)
target_link_libraries(xrpl_shim PUBLIC
    ${RIPPLED_BUILD_DIR}/libxrpl.a
    ${RIPPLED_BUILD_DIR}/libxrpl.libpb.a
    boost::boost
    openssl::openssl
    protobuf::protobuf
    date::date
    xxHash::xxhash
    ed25519::ed25519
    secp256k1::secp256k1
    pthread
    dl
)

# Test binary — boost_test_exec_monitor provides main() that calls test_main()
# which we define in test_shim.c (compiled as C++ so the mangled name matches)
set_source_files_properties(test_shim.c PROPERTIES LANGUAGE CXX)
add_executable(test_shim test_shim.c)
target_link_libraries(test_shim PRIVATE xrpl_shim)
