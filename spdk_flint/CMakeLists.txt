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

# Simplified SPDK and DPDK library linking
# Based on the Dockerfile.base build, SPDK libraries are in /usr/local/lib/

# Core system libraries needed by SPDK
set(SYSTEM_LIBS
    dl
    rt
    numa
    uuid
    pthread
    ssl
    crypto
)

# Essential SPDK libraries in dependency order
set(SPDK_LIBS
    spdk_json
    spdk_jsonrpc  
    spdk_rpc
    spdk_util
    spdk_log
    spdk_thread
    spdk_trace
    spdk_notify
    spdk_sock
    spdk_init
    spdk_env_dpdk
    spdk_memory
    spdk_conf
    spdk_bdev
    spdk_nvme
    spdk_event
)

# Try to find DPDK via pkg-config first
find_package(PkgConfig QUIET)
if(PKG_CONFIG_FOUND)
    pkg_check_modules(DPDK QUIET libdpdk)
    if(DPDK_FOUND)
        message(STATUS "Found DPDK via pkg-config: ${DPDK_LIBRARIES}")
    endif()
endif()

# Fallback: try to find DPDK library manually
if(NOT DPDK_FOUND)
    find_library(DPDK_LIBRARY NAMES dpdk libdpdk 
        PATHS /usr/lib/x86_64-linux-gnu /usr/local/lib /usr/lib
        DOC "DPDK library")
    if(DPDK_LIBRARY)
        set(DPDK_LIBRARIES ${DPDK_LIBRARY})
        message(STATUS "Found DPDK library: ${DPDK_LIBRARY}")
    else()
        message(WARNING "DPDK library not found - may cause linking issues")
    endif()
endif()

# Find AIO library (different names possible)
find_library(AIO_LIBRARY NAMES aio libaio 
    PATHS /usr/lib/x86_64-linux-gnu /usr/local/lib /usr/lib
    DOC "AIO library")
if(AIO_LIBRARY)
    message(STATUS "Found AIO library: ${AIO_LIBRARY}")
else()
    message(WARNING "AIO library not found - may cause linking issues")
endif()

# Find uring library (different names possible)  
find_library(URING_LIBRARY NAMES uring liburing
    PATHS /usr/lib/x86_64-linux-gnu /usr/local/lib /usr/lib
    DOC "uring library")
if(URING_LIBRARY)
    message(STATUS "Found uring library: ${URING_LIBRARY}")
else()
    message(WARNING "uring library not found - may cause linking issues")
endif()

# Link libraries for spdk_flint_lib
target_link_libraries(spdk_flint_lib
    # External dependencies
    ${Protobuf_LIBRARIES}
    ${gRPC_LIBRARIES}
    ${spdlog_LIBRARIES}
    Threads::Threads
    ${CURL_LIBRARIES}
)

# Link DPDK first (SPDK depends on it)
if(DPDK_FOUND)
    target_link_libraries(spdk_flint_lib ${DPDK_LIBRARIES})
elseif(DPDK_LIBRARY)
    target_link_libraries(spdk_flint_lib ${DPDK_LIBRARY})
endif()

# Link SPDK libraries using --whole-archive to ensure all symbols are included
foreach(spdk_lib ${SPDK_LIBS})
    find_library(${spdk_lib}_LIBRARY NAMES ${spdk_lib} lib${spdk_lib}
        PATHS /usr/local/lib
        DOC "${spdk_lib} library")
    
    if(${spdk_lib}_LIBRARY)
        message(STATUS "Found ${spdk_lib}: ${${spdk_lib}_LIBRARY}")
        target_link_libraries(spdk_flint_lib 
            -Wl,--whole-archive ${${spdk_lib}_LIBRARY} -Wl,--no-whole-archive)
    else()
        message(WARNING "Could not find ${spdk_lib} library")
    endif()
endforeach()

# Link system libraries
target_link_libraries(spdk_flint_lib ${SYSTEM_LIBS})

# Link AIO and uring libraries if found
if(AIO_LIBRARY)
    target_link_libraries(spdk_flint_lib ${AIO_LIBRARY})
endif()
if(URING_LIBRARY)
    target_link_libraries(spdk_flint_lib ${URING_LIBRARY})
endif()

# Set linker flags to handle potential issues
set_target_properties(spdk_flint_lib PROPERTIES
    LINK_FLAGS "-Wl,--no-as-needed -Wl,--allow-multiple-definition"
)

# Link libraries for the main executable
target_link_libraries(spdk_flint
    spdk_flint_lib
    ${CMAKE_DL_LIBS}
)

# Set linker flags for main executable
set_target_properties(spdk_flint PROPERTIES
    LINK_FLAGS "-Wl,--no-as-needed -Wl,--allow-multiple-definition"
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