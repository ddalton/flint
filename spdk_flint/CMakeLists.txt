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
set(SPDK_LIBRARY_NAMES
    spdk_env_dpdk
    spdk_util
    spdk_nvme
    spdk_bdev
    spdk_blob
    spdk_lvol
    spdk_nvmf
    spdk_rpc
    spdk_thread
    spdk_log
    spdk_sock
    spdk_json
    spdk_jsonrpc
    spdk_trace
    spdk_notify
    spdk_init
    spdk_event
    spdk_accel
    spdk_ublk
    spdk_conf
)

set(SPDK_LIBRARIES "")
foreach(lib_name ${SPDK_LIBRARY_NAMES})
    find_library(SPDK_${lib_name}_LIBRARY 
        NAMES ${lib_name} 
        PATHS /usr/local/lib 
        REQUIRED
    )
    list(APPEND SPDK_LIBRARIES ${SPDK_${lib_name}_LIBRARY})
endforeach()

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

# Source files
set(SOURCES
    src/main.cpp
    src/app.cpp
    src/logging.cpp
    src/spdk/spdk_wrapper.cpp
    src/utils/config.cpp
    src/utils/kube_client.cpp
    ${CSI_PB_CPP_FILE}
    ${CSI_GRPC_CPP_FILE}
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

# Main executable
add_executable(spdk_flint ${SOURCES})

# Include directories (including gRPC from pkg-config)
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

# Link libraries
target_link_libraries(spdk_flint
    ${SPDK_LIBRARIES}
    ${Protobuf_LIBRARIES}
    ${gRPC_LIBRARIES}
    ${spdlog_LIBRARIES}
    Threads::Threads
    ${CURL_LIBRARIES}
    OpenSSL::SSL
    OpenSSL::Crypto
    dl
    rt
    uuid
    numa
)

# Link Crow (FetchContent or manually found)
if(TARGET Crow::Crow)
    target_link_libraries(spdk_flint Crow::Crow)
elseif(CROW_LIBRARY)
    target_link_libraries(spdk_flint ${CROW_LIBRARY})
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