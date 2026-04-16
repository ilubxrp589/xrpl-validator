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
#
# For 3.1.2 compatibility the shim needs symbols from rippled's xrpld tree
# (Application, HashRouter, LoadFeeTrack, OrderBookDB). 3.1.2's cmake does
# NOT produce a standalone libxrpld.a — xrpld sources are compiled directly
# into the rippled executable target in RippledCore.cmake. We work around
# that by globbing all xrpld/*.cpp and compiling them as part of our shim
# library. This keeps the rippled build config unchanged (-Dxrpld=OFF).
file(GLOB_RECURSE xrpld_sources CONFIGURE_DEPENDS
    "${RIPPLED_SRC_DIR}/src/xrpld/*.cpp"
)
# Exclude the rippled main entrypoint (Main.cpp defines main() which would
# conflict with live_viewer's main).
list(FILTER xrpld_sources EXCLUDE REGEX "/src/xrpld/app/main/Main\\.cpp$")

add_library(xrpl_shim STATIC
    xrpl_shim.cpp
    MinimalApp.cpp
    CallbackReadView.cpp
    MutationCollector.cpp
    ${xrpld_sources}
)
target_include_directories(xrpl_shim PUBLIC
    ${CMAKE_CURRENT_SOURCE_DIR}
    ${RIPPLED_SRC_DIR}/include
    ${RIPPLED_SRC_DIR}/src        # for <xrpld/...> headers
    ${RIPPLED_BUILD_DIR}/proto
    ${RIPPLED_BUILD_DIR}/libxrpl
)
target_compile_definitions(xrpl_shim PUBLIC
    BOOST_ASIO_DISABLE_CONCEPTS  # rippled disables this, we must match
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
