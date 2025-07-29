cmake_minimum_required(VERSION 3.20)
project(spdk_flint VERSION 1.0.0 LANGUAGES CXX C)

# Set C++ standard
set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

# Enable testing
enable_testing()

# Add custom CMake modules path
list(APPEND CMAKE_MODULE_PATH "${CMAKE_CURRENT_SOURCE_DIR}/cmake")

# Build type
if(NOT CMAKE_BUILD_TYPE)
    set(CMAKE_BUILD_TYPE Release)
endif()

# Compiler flags with enhanced memory safety
set(CMAKE_CXX_FLAGS_DEBUG "-g -O0 -DDEBUG")
set(CMAKE_CXX_FLAGS_RELEASE "-O3 -DNDEBUG")

# Memory safety warnings for all builds
add_compile_options(
    -Wall -Wextra -Wpedantic
    -Wold-style-cast
    -Wnon-virtual-dtor
    -Woverloaded-virtual
)

# Debug builds with runtime memory checking
if(CMAKE_BUILD_TYPE STREQUAL "Debug")
    add_compile_options(
        -fsanitize=address,leak,undefined
        -fno-omit-frame-pointer
    )
    add_link_options(
        -fsanitize=address,leak,undefined
    )
endif()

# Find required packages
find_package(PkgConfig REQUIRED)
find_package(Protobuf REQUIRED)
# Manual gRPC detection (pkg-config has dependency issues)
find_library(gRPC_CPP_LIBRARY NAMES grpc++ PATHS /usr/local/lib REQUIRED)
set(gRPC_LIBRARIES ${gRPC_CPP_LIBRARY})
set(gRPC_INCLUDE_DIRS /usr/local/include)

# Find gRPC C++ plugin for protobuf code generation
find_program(gRPC_CPP_PLUGIN_EXECUTABLE grpc_cpp_plugin REQUIRED)
# Manual spdlog detection (CMake config not available)
find_library(spdlog_LIBRARY NAMES spdlog PATHS /usr/local/lib REQUIRED)
set(spdlog_LIBRARIES ${spdlog_LIBRARY})
set(spdlog_INCLUDE_DIRS /usr/local/include)
find_package(Threads REQUIRED)
# Use pkg-config for CURL (more reliable than find_package)
pkg_check_modules(CURL REQUIRED libcurl)
find_package(OpenSSL REQUIRED)

# Manual SPDK detection (pkg-config files not available)
# Based on debugging output, these libraries exist in /usr/local/lib/
# SPDK library discovery is done individually below for better control

set(SPDK_INCLUDE_DIRS /usr/local/include)

# Find Crow (header-only, will be downloaded if not found)
find_path(CROW_INCLUDE_DIR 
    NAMES crow.h crow/crow.h 
    PATHS /usr/include /usr/local/include /usr/local/include/crow
    PATH_SUFFIXES crow
)

if(CROW_INCLUDE_DIR)
    message(STATUS "Found Crow headers at: ${CROW_INCLUDE_DIR}")
    # Also find Crow library if it exists
    find_library(CROW_LIBRARY NAMES crow PATHS /usr/local/lib)
    if(CROW_LIBRARY)
        message(STATUS "Found Crow library at: ${CROW_LIBRARY}")
    endif()
else()
    message(STATUS "Crow not found, will use FetchContent")
    include(FetchContent)
    FetchContent_Declare(
        crow
        GIT_REPOSITORY https://github.com/CrowCpp/Crow.git
        GIT_TAG v1.0+5
    )
    FetchContent_MakeAvailable(crow)
endif()

# Find nlohmann/json for JSON parsing (used for Kubernetes API)
find_package(nlohmann_json 3.10.0 QUIET)
if(NOT nlohmann_json_FOUND)
    include(FetchContent)
    FetchContent_Declare(
        nlohmann_json
        GIT_REPOSITORY https://github.com/nlohmann/json.git
        GIT_TAG v3.11.3
    )
    FetchContent_MakeAvailable(nlohmann_json)
endif()

# Find kubernetes-client-cpp for Kubernetes API (optional - fallback to libcurl)
find_path(KUBERNETES_CLIENT_INCLUDE_DIR kubernetes/client-cpp.h PATHS /usr/include /usr/local/include)
if(NOT KUBERNETES_CLIENT_INCLUDE_DIR)
    message(STATUS "kubernetes-client-cpp not found, using libcurl for Kubernetes API instead")
    # Skip FetchContent for kubernetes-client-cpp in Docker builds due to Git auth issues
    # We'll use direct HTTP calls with libcurl for Kubernetes API integration
    message(STATUS "Kubernetes integration will use libcurl HTTP client")
endif()

# Find Catch2 for testing
find_package(Catch2 3 QUIET)
if(NOT Catch2_FOUND)
    include(FetchContent)
    FetchContent_Declare(
        Catch2
        GIT_REPOSITORY https://github.com/catchorg/Catch2.git
        GIT_TAG v3.4.0
    )
    FetchContent_MakeAvailable(Catch2)
endif()

# Include directories
include_directories(
    ${CMAKE_CURRENT_SOURCE_DIR}/include
    ${CMAKE_CURRENT_SOURCE_DIR}/src/proto
    ${SPDK_INCLUDE_DIRS}
)

# Generate protobuf and gRPC files
set(PROTO_DIR "${CMAKE_CURRENT_SOURCE_DIR}/proto")
set(GENERATED_PROTOBUF_PATH "${CMAKE_CURRENT_BINARY_DIR}/generated")
file(MAKE_DIRECTORY ${GENERATED_PROTOBUF_PATH})

# CSI protocol buffers
set(CSI_PROTO "${PROTO_DIR}/csi.proto")
set(CSI_PB_CPP_FILE "${GENERATED_PROTOBUF_PATH}/csi.pb.cc")
set(CSI_PB_H_FILE "${GENERATED_PROTOBUF_PATH}/csi.pb.h")
set(CSI_GRPC_CPP_FILE "${GENERATED_PROTOBUF_PATH}/csi.grpc.pb.cc")
set(CSI_GRPC_H_FILE "${GENERATED_PROTOBUF_PATH}/csi.grpc.pb.h")

# Generate protobuf files
add_custom_command(
    OUTPUT ${CSI_PB_CPP_FILE} ${CSI_PB_H_FILE} ${CSI_GRPC_CPP_FILE} ${CSI_GRPC_H_FILE}
    COMMAND ${Protobuf_PROTOC_EXECUTABLE}
    ARGS --cpp_out=${GENERATED_PROTOBUF_PATH}
         --grpc_out=${GENERATED_PROTOBUF_PATH}
         --plugin=protoc-gen-grpc=${gRPC_CPP_PLUGIN_EXECUTABLE}
         -I${PROTO_DIR}
         ${CSI_PROTO}
    DEPENDS ${CSI_PROTO}
    COMMENT "Generating protobuf files"
)

# Library source files (excluding main.cpp for reuse in tests)
set(LIB_SOURCES
    src/app.cpp
    src/logging.cpp
    src/spdk/spdk_wrapper.cpp
    src/utils/config.cpp
    src/utils/kube_client.cpp
    # Temporarily commented out due to Abseil compatibility issues
    # ${CSI_PB_CPP_FILE}
    # ${CSI_GRPC_CPP_FILE}
    # TODO: Add these files as they are implemented:
    # src/csi/identity_service.cpp
    # src/csi/controller_service.cpp
    # src/csi/node_service.cpp
    # src/spdk/volume_manager.cpp
    # src/spdk/raid_manager.cpp
    # src/spdk/nvmeof_manager.cpp
    # src/dashboard/dashboard_service.cpp
    # src/node_agent/node_agent.cpp
    # src/controller_operator/controller_operator.cpp
)

# Create shared library from main application code
add_library(spdk_flint_lib STATIC ${LIB_SOURCES})

# Main executable
add_executable(spdk_flint src/main.cpp)

# Include directories for the library (PUBLIC so they propagate to tests)
target_include_directories(spdk_flint_lib PUBLIC
    ${CMAKE_SOURCE_DIR}/include
    ${GENERATED_PROTOBUF_PATH}
    ${SPDK_INCLUDE_DIRS}
    ${gRPC_INCLUDE_DIRS}
    ${Protobuf_INCLUDE_DIRS}
    ${spdlog_INCLUDE_DIRS}
    ${CURL_INCLUDE_DIRS}
)

# Include directories for the main executable
target_include_directories(spdk_flint PRIVATE
    ${CMAKE_SOURCE_DIR}/include
    ${GENERATED_PROTOBUF_PATH}
    ${SPDK_INCLUDE_DIRS}
    ${gRPC_INCLUDE_DIRS}
    ${Protobuf_INCLUDE_DIRS}
    ${spdlog_INCLUDE_DIRS}
    ${CURL_INCLUDE_DIRS}
)

# Crow includes handled by target linkage (FetchContent provides Crow::Crow target)

# Try to find DPDK via pkg-config 
find_package(PkgConfig QUIET)
if(PKG_CONFIG_FOUND)
    pkg_check_modules(DPDK QUIET libdpdk)
    if(DPDK_FOUND)
        message(STATUS "Found DPDK via pkg-config")
    endif()
endif()

# Try to find shared SPDK libraries first (might have complete symbol sets)
find_library(SPDK_JSON_SHARED NAMES spdk_json PATHS /usr/local/lib /usr/lib/x86_64-linux-gnu /usr/lib NO_DEFAULT_PATH)
find_library(SPDK_JSONRPC_SHARED NAMES spdk_jsonrpc PATHS /usr/local/lib /usr/lib/x86_64-linux-gnu /usr/lib NO_DEFAULT_PATH)
find_library(SPDK_UTIL_SHARED NAMES spdk_util PATHS /usr/local/lib /usr/lib/x86_64-linux-gnu /usr/lib NO_DEFAULT_PATH)

# Also find system DPDK library (provides all rte_* functions)
find_library(DPDK_MAIN NAMES dpdk rte libdpdk PATHS /usr/lib/x86_64-linux-gnu /usr/local/lib /usr/lib)
# Also try to find specific DPDK libraries
find_library(DPDK_PCI NAMES dpdk_pci rte_pci PATHS /usr/lib/x86_64-linux-gnu /usr/local/lib /usr/lib)
find_library(DPDK_RAW NAMES dpdk PATHS /usr/lib/x86_64-linux-gnu /usr/local/lib /usr/lib)

# Try alternative DPDK paths and naming patterns
find_library(DPDK_ALL NAMES librte_all rte_all PATHS /usr/lib/x86_64-linux-gnu /usr/local/lib)
find_library(DPDK_STATIC NAMES dpdk_static libdpdk_static PATHS /usr/lib/x86_64-linux-gnu /usr/local/lib)

# Try to find a combined SPDK library that might have all functions
find_library(SPDK_COMBINED NAMES spdk libspdk spdk_all PATHS /usr/local/lib /usr/lib)
# Try different naming patterns for JSON functions 
find_library(SPDK_JSON_ALL NAMES spdk_json_all PATHS /usr/local/lib)
# Try to find socket utilities that might have spdk_fd_set_nonblock
find_library(SPDK_SOCK_UTIL NAMES spdk_sock_util sock_util PATHS /usr/local/lib)
# Search more comprehensively for JSON parsing
find_library(SPDK_JSON_PARSER NAMES spdk_json_parser json_parser PATHS /usr/local/lib)
find_library(SPDK_JSON_DECODE NAMES spdk_json_decode json_decode PATHS /usr/local/lib)
# Search for fd/socket utilities
find_library(SPDK_FD_UTIL NAMES spdk_fd_util fd_util PATHS /usr/local/lib)

# Search for external JSON libraries as fallback
find_package(PkgConfig QUIET)
if(PKG_CONFIG_FOUND)
    pkg_check_modules(JANSSON QUIET jansson)
    pkg_check_modules(JSON_C QUIET json-c)
endif()
find_library(JANSSON_LIB NAMES jansson PATHS /usr/lib/x86_64-linux-gnu /usr/local/lib)
find_library(JSON_C_LIB NAMES json-c PATHS /usr/lib/x86_64-linux-gnu /usr/local/lib)

# Try to find more comprehensive SPDK library collections
find_library(SPDK_ALL_STATIC NAMES spdk_all libspdk_all PATHS /usr/local/lib /usr/lib)
find_library(SPDK_CORE NAMES spdk_core libspdk_core PATHS /usr/local/lib /usr/lib)

# Search for platform-specific implementations
find_library(SPDK_PLATFORM NAMES spdk_platform libspdk_platform PATHS /usr/local/lib /usr/lib)
find_library(SPDK_APP NAMES spdk_app libspdk_app PATHS /usr/local/lib /usr/lib)

# Try to find SPDK libraries in alternative directories that might be more complete
file(GLOB SPDK_LIB_SEARCH_PATHS
    "/usr/local/lib/spdk/*"
    "/usr/lib/x86_64-linux-gnu/spdk/*" 
    "/opt/spdk/lib/*"
    "/usr/share/spdk/lib/*"
)
foreach(search_path ${SPDK_LIB_SEARCH_PATHS})
    if(IS_DIRECTORY ${search_path})
        list(APPEND CMAKE_LIBRARY_PATH ${search_path})
    endif()
endforeach()

# Try one more comprehensive search for the missing JSON functions
find_library(SPDK_JSON_IMPL NAMES spdk_json_impl json_impl PATHS ${SPDK_LIB_SEARCH_PATHS} /usr/local/lib)
find_library(SPDK_JSONRPC_IMPL NAMES spdk_jsonrpc_impl jsonrpc_impl PATHS ${SPDK_LIB_SEARCH_PATHS} /usr/local/lib)

# Add library search directories for SPDK
link_directories(/usr/local/lib)

# Find ALL SPDK libraries that might be needed
find_library(SPDK_ENV_DPDK NAMES spdk_env_dpdk PATHS /usr/local/lib)
find_library(SPDK_UTIL NAMES spdk_util PATHS /usr/local/lib)
find_library(SPDK_LOG NAMES spdk_log PATHS /usr/local/lib)
find_library(SPDK_THREAD NAMES spdk_thread PATHS /usr/local/lib)
find_library(SPDK_TRACE NAMES spdk_trace PATHS /usr/local/lib)
find_library(SPDK_NOTIFY NAMES spdk_notify PATHS /usr/local/lib)
find_library(SPDK_JSON NAMES spdk_json PATHS /usr/local/lib)
find_library(SPDK_JSONRPC NAMES spdk_jsonrpc PATHS /usr/local/lib)
# Note: jsonrpc_client functions are embedded in spdk_jsonrpc
find_library(SPDK_MEMORY NAMES spdk_memory spdk_memory_domain spdk_dma PATHS /usr/local/lib)
find_library(SPDK_SOCK NAMES spdk_sock PATHS /usr/local/lib)
find_library(SPDK_BDEV NAMES spdk_bdev PATHS /usr/local/lib)
find_library(SPDK_ACCEL NAMES spdk_accel PATHS /usr/local/lib)
find_library(SPDK_INIT NAMES spdk_init PATHS /usr/local/lib)
find_library(SPDK_RPC NAMES spdk_rpc PATHS /usr/local/lib)
find_library(SPDK_EVENT NAMES spdk_event PATHS /usr/local/lib)
find_library(SPDK_NVME NAMES spdk_nvme PATHS /usr/local/lib)
# Try to find additional SPDK libraries for missing functions
find_library(SPDK_FD_GROUP NAMES spdk_fd_group fd_group PATHS /usr/local/lib)
find_library(SPDK_IOBUF NAMES spdk_iobuf iobuf PATHS /usr/local/lib)
find_library(SPDK_NOTIFY NAMES spdk_notify notify PATHS /usr/local/lib) 
find_library(SPDK_JSON_WRITE NAMES spdk_json_write json_write PATHS /usr/local/lib)
find_library(SPDK_LOG_FLAG NAMES spdk_log_flag log_flag PATHS /usr/local/lib)
find_library(SPDK_UTIL_EXT NAMES spdk_util_ext util_ext PATHS /usr/local/lib)
# Try to find JSONRPC client library specifically
find_library(SPDK_JSONRPC_CLIENT NAMES spdk_jsonrpc_client jsonrpc_client PATHS /usr/local/lib)
# Try to find additional JSON libraries
find_library(SPDK_JSON_UTIL NAMES spdk_json_util json_util PATHS /usr/local/lib)
find_library(SPDK_JSON_PARSE NAMES spdk_json_parse json_parse PATHS /usr/local/lib)
# Also search for some additional common SPDK libraries
find_library(SPDK_VTM NAMES spdk_vtm vtm PATHS /usr/local/lib)
find_library(SPDK_LOG_RATELIMIT NAMES spdk_log_ratelimit log_ratelimit PATHS /usr/local/lib)
find_library(SPDK_TRACE_PARSER NAMES spdk_trace_parser trace_parser PATHS /usr/local/lib)
find_library(SPDK_CONF NAMES spdk_conf PATHS /usr/local/lib)
find_library(SPDK_BLOB NAMES spdk_blob PATHS /usr/local/lib)
find_library(SPDK_LVOL NAMES spdk_lvol PATHS /usr/local/lib)
find_library(SPDK_NVMF NAMES spdk_nvmf PATHS /usr/local/lib)
find_library(SPDK_UBLK NAMES spdk_ublk PATHS /usr/local/lib)

# Find ISA-L libraries (Intel Storage Acceleration Library)
# SPDK builds ISA-L internally, so check multiple naming patterns and locations
find_library(ISAL NAMES isal libisal spdk_isal PATHS /usr/local/lib /usr/local/lib/spdk /usr/lib)
find_library(ISAL_CRYPTO NAMES isal_crypto libisal_crypto spdk_isal_crypto PATHS /usr/local/lib /usr/local/lib/spdk /usr/lib)

# Also check if ISA-L symbols might be embedded in SPDK libraries
# In some SPDK builds, ISA-L is statically linked into SPDK libraries
if(NOT ISAL)
    message(STATUS "ISA-L not found as separate library - may be embedded in SPDK")
endif()
if(NOT ISAL_CRYPTO)
    message(STATUS "ISA-L-crypto not found as separate library - may be embedded in SPDK")
endif()

# Log what we found
message(STATUS "SPDK libraries found:")
message(STATUS "  ENV_DPDK: ${SPDK_ENV_DPDK}")
message(STATUS "  UTIL: ${SPDK_UTIL}")
message(STATUS "  LOG: ${SPDK_LOG}")
message(STATUS "  JSON: ${SPDK_JSON}")
message(STATUS "  JSONRPC: ${SPDK_JSONRPC}")
message(STATUS "  MEMORY: ${SPDK_MEMORY}")
message(STATUS "  SOCK: ${SPDK_SOCK}")
message(STATUS "  BDEV: ${SPDK_BDEV}")
message(STATUS "  ACCEL: ${SPDK_ACCEL}")
message(STATUS "  INIT: ${SPDK_INIT}")
message(STATUS "  RPC: ${SPDK_RPC}")
message(STATUS "  EVENT: ${SPDK_EVENT}")
message(STATUS "  NVME: ${SPDK_NVME}")
message(STATUS "  CONF: ${SPDK_CONF}")
message(STATUS "  BLOB: ${SPDK_BLOB}")
message(STATUS "  LVOL: ${SPDK_LVOL}")
message(STATUS "  NVMF: ${SPDK_NVMF}")
message(STATUS "  UBLK: ${SPDK_UBLK}")
message(STATUS "Additional SPDK libraries:")
message(STATUS "  FD_GROUP: ${SPDK_FD_GROUP}")
message(STATUS "  IOBUF: ${SPDK_IOBUF}")
message(STATUS "  NOTIFY: ${SPDK_NOTIFY}")
message(STATUS "  JSON_WRITE: ${SPDK_JSON_WRITE}")
message(STATUS "  LOG_FLAG: ${SPDK_LOG_FLAG}")
message(STATUS "  UTIL_EXT: ${SPDK_UTIL_EXT}")
message(STATUS "  JSONRPC_CLIENT: ${SPDK_JSONRPC_CLIENT}")
message(STATUS "  JSON_UTIL: ${SPDK_JSON_UTIL}")
message(STATUS "  JSON_PARSE: ${SPDK_JSON_PARSE}")
message(STATUS "  VTM: ${SPDK_VTM}")
message(STATUS "  LOG_RATELIMIT: ${SPDK_LOG_RATELIMIT}")
message(STATUS "  TRACE_PARSER: ${SPDK_TRACE_PARSER}")
message(STATUS "DPDK libraries found:")
message(STATUS "  DPDK_FOUND: ${DPDK_FOUND}")
message(STATUS "  DPDK_LIBRARIES: ${DPDK_LIBRARIES}")
message(STATUS "  DPDK_MAIN: ${DPDK_MAIN}")
message(STATUS "  DPDK_PCI: ${DPDK_PCI}")
message(STATUS "  DPDK_RAW: ${DPDK_RAW}")
message(STATUS "  DPDK_ALL: ${DPDK_ALL}")
message(STATUS "  DPDK_STATIC: ${DPDK_STATIC}")
message(STATUS "Shared SPDK libraries:")
message(STATUS "  SPDK_JSON_SHARED: ${SPDK_JSON_SHARED}")
message(STATUS "  SPDK_JSONRPC_SHARED: ${SPDK_JSONRPC_SHARED}")
message(STATUS "  SPDK_UTIL_SHARED: ${SPDK_UTIL_SHARED}")
message(STATUS "Special SPDK libraries:")
message(STATUS "  SPDK_COMBINED: ${SPDK_COMBINED}")
message(STATUS "  SPDK_JSON_ALL: ${SPDK_JSON_ALL}")
message(STATUS "  SPDK_JSON_PARSER: ${SPDK_JSON_PARSER}")
message(STATUS "  SPDK_JSON_DECODE: ${SPDK_JSON_DECODE}")
message(STATUS "  SPDK_SOCK_UTIL: ${SPDK_SOCK_UTIL}")
message(STATUS "  SPDK_FD_UTIL: ${SPDK_FD_UTIL}")
message(STATUS "  SPDK_ALL_STATIC: ${SPDK_ALL_STATIC}")
message(STATUS "  SPDK_CORE: ${SPDK_CORE}")
message(STATUS "Fallback JSON libraries:")
message(STATUS "  JANSSON: ${JANSSON_FOUND}")
message(STATUS "  JANSSON_LIB: ${JANSSON_LIB}")
message(STATUS "  JSON_C: ${JSON_C_FOUND}")
message(STATUS "  JSON_C_LIB: ${JSON_C_LIB}")
message(STATUS "Extended SPDK libraries:")
message(STATUS "  SPDK_PLATFORM: ${SPDK_PLATFORM}")
message(STATUS "  SPDK_APP: ${SPDK_APP}")
message(STATUS "  SPDK_JSON_IMPL: ${SPDK_JSON_IMPL}")
message(STATUS "  SPDK_JSONRPC_IMPL: ${SPDK_JSONRPC_IMPL}")
message(STATUS "ISA-L libraries found:")
message(STATUS "  ISAL: ${ISAL}")
message(STATUS "  ISAL_CRYPTO: ${ISAL_CRYPTO}")

# Link libraries for the main library with proper SPDK dependency order
target_link_libraries(spdk_flint_lib
    # SPDK libraries are linked conditionally below with whole-archive
    # Add DPDK if found (removed from here, handled conditionally below)
    ${Protobuf_LIBRARIES}
    ${gRPC_LIBRARIES}
    ${spdlog_LIBRARIES}
    Threads::Threads
    ${CURL_LIBRARIES}
    # System libraries (before OpenSSL)
    dl
    rt
    uuid
    numa
    pthread
    m
    # Additional math and utility libraries that might contain missing symbols
    c
    # OpenSSL libraries (at the end for proper dependency resolution)  
    -lssl
    -lcrypto
)

# Add DPDK libraries first (SPDK depends on DPDK)
if(DPDK_FOUND)
    target_link_libraries(spdk_flint_lib ${DPDK_LIBRARIES})
endif()
if(DPDK_MAIN)
    target_link_libraries(spdk_flint_lib ${DPDK_MAIN})
endif()
if(DPDK_PCI)
    target_link_libraries(spdk_flint_lib ${DPDK_PCI})
endif()
if(DPDK_RAW)
    target_link_libraries(spdk_flint_lib ${DPDK_RAW})
endif()
if(DPDK_ALL)
    target_link_libraries(spdk_flint_lib ${DPDK_ALL})
endif()
if(DPDK_STATIC)
    target_link_libraries(spdk_flint_lib ${DPDK_STATIC})
endif()

# Try shared SPDK libraries first (might have complete symbols)
if(SPDK_JSON_SHARED)
    target_link_libraries(spdk_flint_lib ${SPDK_JSON_SHARED})
endif()
if(SPDK_JSONRPC_SHARED)
    target_link_libraries(spdk_flint_lib ${SPDK_JSONRPC_SHARED})
endif()
if(SPDK_UTIL_SHARED)
    target_link_libraries(spdk_flint_lib ${SPDK_UTIL_SHARED})
endif()

# Link all SPDK libraries multiple times to resolve circular dependencies
# Sometimes linking order matters and functions are spread across libraries
if(SPDK_UTIL)
    target_link_libraries(spdk_flint_lib ${SPDK_UTIL})
endif()
if(SPDK_JSON)
    target_link_libraries(spdk_flint_lib ${SPDK_JSON})
endif()
if(SPDK_LOG)
    target_link_libraries(spdk_flint_lib ${SPDK_LOG})
endif()

# Add SPDK libraries conditionally in dependency order (core first, then high-level)
# Core SPDK libraries (foundation)
if(SPDK_ENV_DPDK)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_ENV_DPDK} -Wl,--no-whole-archive)
endif()
if(SPDK_UTIL)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_UTIL} -Wl,--no-whole-archive)
endif()
if(SPDK_LOG)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_LOG} -Wl,--no-whole-archive)
endif()
if(SPDK_MEMORY)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_MEMORY} -Wl,--no-whole-archive)
endif()

# Infrastructure libraries (I/O, networking, etc.)
if(SPDK_SOCK)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_SOCK} -Wl,--no-whole-archive)
endif()
if(SPDK_FD_GROUP)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_FD_GROUP} -Wl,--no-whole-archive)
endif()
if(SPDK_IOBUF)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_IOBUF} -Wl,--no-whole-archive)
endif()
if(SPDK_THREAD)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_THREAD} -Wl,--no-whole-archive)
endif()
if(SPDK_TRACE)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_TRACE} -Wl,--no-whole-archive)
endif()
if(SPDK_NOTIFY)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_NOTIFY} -Wl,--no-whole-archive)
endif()
if(SPDK_LOG_FLAG)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_LOG_FLAG} -Wl,--no-whole-archive)
endif()
if(SPDK_UTIL_EXT)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_UTIL_EXT} -Wl,--no-whole-archive)
endif()
if(SPDK_VTM)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_VTM} -Wl,--no-whole-archive)
endif()
if(SPDK_LOG_RATELIMIT)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_LOG_RATELIMIT} -Wl,--no-whole-archive)
endif()
if(SPDK_TRACE_PARSER)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_TRACE_PARSER} -Wl,--no-whole-archive)
endif()

# JSON and RPC libraries (protocol handling) - Link multiple times for dependencies
if(SPDK_JSON)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_JSON} -Wl,--no-whole-archive)
endif()
if(SPDK_JSON_UTIL)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_JSON_UTIL} -Wl,--no-whole-archive)
endif()
if(SPDK_JSON_PARSE)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_JSON_PARSE} -Wl,--no-whole-archive)
endif()
if(SPDK_JSON_WRITE)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_JSON_WRITE} -Wl,--no-whole-archive)
endif()
if(SPDK_JSONRPC)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_JSONRPC} -Wl,--no-whole-archive)
endif()
if(SPDK_JSONRPC_CLIENT)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_JSONRPC_CLIENT} -Wl,--no-whole-archive)
endif()
if(SPDK_RPC)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_RPC} -Wl,--no-whole-archive)
endif()
# Link JSON libraries again (circular dependencies)
if(SPDK_JSON)
    target_link_libraries(spdk_flint_lib ${SPDK_JSON})
endif()
if(SPDK_JSONRPC)
    target_link_libraries(spdk_flint_lib ${SPDK_JSONRPC})
endif()

# Configuration and file handling
if(SPDK_CONF)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_CONF} -Wl,--no-whole-archive)
endif()
# Note: file and subsystem functions are embedded in other libraries

# High-level storage libraries
if(SPDK_BDEV)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_BDEV} -Wl,--no-whole-archive)
endif()
if(SPDK_BLOB)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_BLOB} -Wl,--no-whole-archive)
endif()
if(SPDK_LVOL)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_LVOL} -Wl,--no-whole-archive)
endif()
if(SPDK_NVME)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_NVME} -Wl,--no-whole-archive)
endif()
if(SPDK_NVMF)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_NVMF} -Wl,--no-whole-archive)
endif()
if(SPDK_UBLK)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_UBLK} -Wl,--no-whole-archive)
endif()
if(SPDK_ACCEL)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_ACCEL} -Wl,--no-whole-archive)
endif()

# Initialization and event handling (depends on everything else)
if(SPDK_INIT)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_INIT} -Wl,--no-whole-archive)
endif()
if(SPDK_EVENT)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_EVENT} -Wl,--no-whole-archive)
endif()

# Add ISA-L libraries conditionally (only if found)
if(ISAL)
    target_link_libraries(spdk_flint_lib ${ISAL})
endif()
if(ISAL_CRYPTO)
    target_link_libraries(spdk_flint_lib ${ISAL_CRYPTO})
endif()

# Second pass: Link key SPDK libraries again to resolve any circular dependencies
# This helps catch symbols that weren't resolved in the first pass
if(SPDK_UTIL)
    target_link_libraries(spdk_flint_lib ${SPDK_UTIL})
endif()
if(SPDK_JSON)
    target_link_libraries(spdk_flint_lib ${SPDK_JSON})
endif()
if(SPDK_JSONRPC)
    target_link_libraries(spdk_flint_lib ${SPDK_JSONRPC})
endif()
if(SPDK_JSONRPC_CLIENT)
    target_link_libraries(spdk_flint_lib ${SPDK_JSONRPC_CLIENT})
endif()
if(SPDK_LOG)
    target_link_libraries(spdk_flint_lib ${SPDK_LOG})
endif()
if(SPDK_MEMORY)
    target_link_libraries(spdk_flint_lib ${SPDK_MEMORY})
endif()
if(SPDK_SOCK)
    target_link_libraries(spdk_flint_lib ${SPDK_SOCK})
endif()
if(SPDK_THREAD)
    target_link_libraries(spdk_flint_lib ${SPDK_THREAD})
endif()
if(SPDK_ENV_DPDK)
    target_link_libraries(spdk_flint_lib ${SPDK_ENV_DPDK})
endif()

# Third pass: Link all DPDK libraries again to resolve rte_pci functions
if(DPDK_FOUND)
    target_link_libraries(spdk_flint_lib ${DPDK_LIBRARIES})
endif()
if(DPDK_PCI)
    target_link_libraries(spdk_flint_lib ${DPDK_PCI})
endif()
if(DPDK_RAW)
    target_link_libraries(spdk_flint_lib ${DPDK_RAW})
endif()

# Fourth pass: Link JSON and JSONRPC libraries one more time
if(SPDK_JSON)
    target_link_libraries(spdk_flint_lib ${SPDK_JSON})
endif()
if(SPDK_JSONRPC)
    target_link_libraries(spdk_flint_lib ${SPDK_JSONRPC})
endif()
if(SPDK_RPC)
    target_link_libraries(spdk_flint_lib ${SPDK_RPC})
endif()

# Link special combined libraries if found
if(SPDK_COMBINED)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_COMBINED} -Wl,--no-whole-archive)
endif()
if(SPDK_JSON_ALL)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_JSON_ALL} -Wl,--no-whole-archive)
endif()
if(SPDK_SOCK_UTIL)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_SOCK_UTIL} -Wl,--no-whole-archive)
endif()
if(SPDK_JSON_PARSER)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_JSON_PARSER} -Wl,--no-whole-archive)
endif()
if(SPDK_JSON_DECODE)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_JSON_DECODE} -Wl,--no-whole-archive)
endif()
if(SPDK_FD_UTIL)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_FD_UTIL} -Wl,--no-whole-archive)
endif()
if(SPDK_ALL_STATIC)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_ALL_STATIC} -Wl,--no-whole-archive)
endif()
if(SPDK_CORE)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_CORE} -Wl,--no-whole-archive)
endif()

# Add fallback JSON libraries if available
if(JANSSON_FOUND)
    target_link_libraries(spdk_flint_lib ${JANSSON_LIBRARIES})
endif()
if(JANSSON_LIB)
    target_link_libraries(spdk_flint_lib ${JANSSON_LIB})
endif()
if(JSON_C_FOUND)
    target_link_libraries(spdk_flint_lib ${JSON_C_LIBRARIES})
endif()
if(JSON_C_LIB)
    target_link_libraries(spdk_flint_lib ${JSON_C_LIB})
endif()

# Use comprehensive group linking with aggressive symbol resolution
target_link_libraries(spdk_flint_lib -Wl,--start-group)

# First pass: All JSON and RPC related libraries
if(SPDK_JSON)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_JSON} -Wl,--no-whole-archive)
endif()
if(SPDK_JSON_SHARED)
    target_link_libraries(spdk_flint_lib ${SPDK_JSON_SHARED})
endif()
if(SPDK_JSONRPC) 
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_JSONRPC} -Wl,--no-whole-archive)
endif()
if(SPDK_JSONRPC_SHARED)
    target_link_libraries(spdk_flint_lib ${SPDK_JSONRPC_SHARED})
endif()
if(SPDK_RPC)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_RPC} -Wl,--no-whole-archive)
endif()

# Core libraries
if(SPDK_INIT)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_INIT} -Wl,--no-whole-archive)
endif()
if(SPDK_UTIL)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_UTIL} -Wl,--no-whole-archive)
endif()
if(SPDK_UTIL_SHARED)
    target_link_libraries(spdk_flint_lib ${SPDK_UTIL_SHARED})
endif()
if(SPDK_SOCK)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_SOCK} -Wl,--no-whole-archive)
endif()

# All comprehensive libraries
if(SPDK_ALL_STATIC)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_ALL_STATIC} -Wl,--no-whole-archive)
endif()
if(SPDK_CORE)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_CORE} -Wl,--no-whole-archive)
endif()
if(SPDK_COMBINED)
    target_link_libraries(spdk_flint_lib -Wl,--whole-archive ${SPDK_COMBINED} -Wl,--no-whole-archive)
endif()

# All DPDK libraries again
if(DPDK_FOUND)
    target_link_libraries(spdk_flint_lib ${DPDK_LIBRARIES})
endif()
if(DPDK_ALL)
    target_link_libraries(spdk_flint_lib ${DPDK_ALL})
endif()

target_link_libraries(spdk_flint_lib -Wl,--end-group)

# Final desperate attempt: Link all discovered libraries one more time
# This approach uses the most aggressive linking possible
if(SPDK_JSON)
    target_link_libraries(spdk_flint_lib ${SPDK_JSON})
endif()
if(SPDK_JSON_SHARED)
    target_link_libraries(spdk_flint_lib ${SPDK_JSON_SHARED})
endif()
if(SPDK_JSONRPC)
    target_link_libraries(spdk_flint_lib ${SPDK_JSONRPC})
endif()
if(SPDK_JSONRPC_SHARED)
    target_link_libraries(spdk_flint_lib ${SPDK_JSONRPC_SHARED})
endif()
if(SPDK_JSON_IMPL)
    target_link_libraries(spdk_flint_lib ${SPDK_JSON_IMPL})
endif()
if(SPDK_JSONRPC_IMPL)
    target_link_libraries(spdk_flint_lib ${SPDK_JSONRPC_IMPL})
endif()
if(SPDK_PLATFORM)
    target_link_libraries(spdk_flint_lib ${SPDK_PLATFORM})
endif()
if(SPDK_APP)
    target_link_libraries(spdk_flint_lib ${SPDK_APP})
endif()
if(DPDK_FOUND)
    target_link_libraries(spdk_flint_lib ${DPDK_LIBRARIES})
endif()
if(DPDK_ALL)
    target_link_libraries(spdk_flint_lib ${DPDK_ALL})
endif()
if(DPDK_PCI)
    target_link_libraries(spdk_flint_lib ${DPDK_PCI})
endif()

# Add more system libraries that might contain missing symbols
# Note: nsl and resolv are not available in modern systems
# target_link_libraries(spdk_flint_lib
#     # Additional system libraries for networking and I/O
#     resolv
#     nsl
# )

# DPDK libraries are now linked above before SPDK libraries

# Add comprehensive linker flags to resolve DSO and symbol issues
set_target_properties(spdk_flint PROPERTIES
    LINK_FLAGS "-Wl,--no-as-needed -Wl,--allow-multiple-definition -Wl,--undefined-version"
)

# Also set aggressive flags for the library
set_target_properties(spdk_flint_lib PROPERTIES
    LINK_FLAGS "-Wl,--no-as-needed -Wl,--allow-multiple-definition"
)

# Link libraries for the main executable
target_link_libraries(spdk_flint
    spdk_flint_lib
    # Force OpenSSL linking with raw library names and full paths
    -lssl
    -lcrypto
    ${CMAKE_DL_LIBS}
)

# Link Crow (FetchContent or manually found)
if(TARGET Crow::Crow)
    target_link_libraries(spdk_flint_lib Crow::Crow)
elseif(CROW_LIBRARY)
    target_link_libraries(spdk_flint_lib ${CROW_LIBRARY})
endif()

# Link nlohmann_json
if(TARGET nlohmann_json::nlohmann_json)
    target_link_libraries(spdk_flint nlohmann_json::nlohmann_json)
else()
    target_link_libraries(spdk_flint nlohmann_json)
endif()

# Link kubernetes client if available
if(TARGET kubernetes-client-cpp)
    target_link_libraries(spdk_flint kubernetes-client-cpp)
endif()

# Compiler definitions
target_compile_definitions(spdk_flint PRIVATE
    ${SPDK_CFLAGS_OTHER}
)

# Tests
add_subdirectory(test)

# Install
install(TARGETS spdk_flint DESTINATION bin) 