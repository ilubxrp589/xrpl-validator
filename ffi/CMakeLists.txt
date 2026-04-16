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

# Conan-provided dependencies (matching rippled 3.1.2's own find_package list)
find_package(Boost REQUIRED)
find_package(OpenSSL REQUIRED)
find_package(protobuf REQUIRED)
find_package(gRPC REQUIRED)
find_package(date REQUIRED)
find_package(xxHash REQUIRED)
find_package(lz4 REQUIRED)
find_package(LibArchive REQUIRED)
find_package(SOCI REQUIRED)
find_package(SQLite3 REQUIRED)
find_package(RocksDB REQUIRED)
find_package(nudb REQUIRED)
find_package(absl REQUIRED)
find_package(Snappy REQUIRED)
find_package(BZip2 REQUIRED)
find_package(ZLIB REQUIRED)

# ed25519 and secp256k1 are built from rippled's external/ subdirectories,
# not conan packages. Import the pre-built .a files from the rippled build tree.
add_library(ed25519_lib STATIC IMPORTED)
set_target_properties(ed25519_lib PROPERTIES
    IMPORTED_LOCATION ${RIPPLED_BUILD_DIR}/external/ed25519-donna/libed25519.a
    INTERFACE_INCLUDE_DIRECTORIES ${RIPPLED_SRC_DIR}/external/ed25519-donna
)

add_library(secp256k1_lib STATIC IMPORTED)
set_target_properties(secp256k1_lib PROPERTIES
    IMPORTED_LOCATION ${RIPPLED_BUILD_DIR}/external/secp256k1/lib/libsecp256k1.a
    INTERFACE_INCLUDE_DIRECTORIES ${RIPPLED_SRC_DIR}/external/secp256k1/include
)

# xrpld sources: 3.1.2 compiles these directly into the rippled executable
# (no libxrpld.a). We glob them into our shim library instead.
file(GLOB_RECURSE xrpld_sources CONFIGURE_DEPENDS
    "${RIPPLED_SRC_DIR}/src/xrpld/*.cpp"
)
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
    ${RIPPLED_SRC_DIR}/src
    ${RIPPLED_BUILD_DIR}/proto
    ${RIPPLED_BUILD_DIR}/libxrpl
)
target_compile_definitions(xrpl_shim PUBLIC
    BOOST_ASIO_DISABLE_CONCEPTS
    RIPPLE_ROCKSDB_AVAILABLE=1
)
target_link_libraries(xrpl_shim PUBLIC
    ${RIPPLED_BUILD_DIR}/libxrpl.a
    ${RIPPLED_BUILD_DIR}/libxrpl.libpb.a
    ed25519_lib
    secp256k1_lib
    boost::boost
    openssl::openssl
    protobuf::libprotobuf
    gRPC::grpc++
    date::date
    xxHash::xxhash
    lz4::lz4
    LibArchive::LibArchive
    soci::soci
    SQLite::SQLite3
    RocksDB::rocksdb
    NuDB::nudb
    absl::random_random
    Snappy::snappy
    BZip2::BZip2
    ZLIB::ZLIB
    pthread
    dl
)

# Test binary — boost_test_exec_monitor provides main() that calls test_main()
# which we define in test_shim.c (compiled as C++ so the mangled name matches)
set_source_files_properties(test_shim.c PROPERTIES LANGUAGE CXX)
add_executable(test_shim test_shim.c)
target_link_libraries(test_shim PRIVATE xrpl_shim)
