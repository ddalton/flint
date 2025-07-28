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

# Compiler flags
set(CMAKE_CXX_FLAGS_DEBUG "-g -O0 -Wall -Wextra -DDEBUG")
set(CMAKE_CXX_FLAGS_RELEASE "-O3 -DNDEBUG")

# Find required packages
find_package(PkgConfig REQUIRED)
find_package(Protobuf REQUIRED)
find_package(gRPC REQUIRED)
find_package(spdlog REQUIRED)
find_package(Threads REQUIRED)
find_package(CURL REQUIRED)
find_package(OpenSSL REQUIRED)

# Find SPDK using pkg-config
pkg_check_modules(SPDK REQUIRED spdk_env_dpdk spdk_util spdk_nvme spdk_bdev spdk_blob spdk_lvol spdk_nvmf spdk_rpc)

# Find Crow (header-only, will be downloaded if not found)
find_path(CROW_INCLUDE_DIR crow.h PATHS /usr/include /usr/local/include)
if(NOT CROW_INCLUDE_DIR)
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

# Find kubernetes-client-cpp for Kubernetes API
find_path(KUBERNETES_CLIENT_INCLUDE_DIR kubernetes/client-cpp.h PATHS /usr/include /usr/local/include)
if(NOT KUBERNETES_CLIENT_INCLUDE_DIR)
    message(STATUS "kubernetes-client-cpp not found, will use FetchContent")
    include(FetchContent)
    FetchContent_Declare(
        kubernetes-client-cpp
        GIT_REPOSITORY https://github.com/kubernetes-client/cpp.git
        GIT_TAG master
    )
    FetchContent_MakeAvailable(kubernetes-client-cpp)
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
    ${CROW_INCLUDE_DIR}
)

# Generate protobuf and gRPC files
set(PROTO_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../spdk-csi-driver/proto")
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
    src/csi/identity_service.cpp
    src/csi/controller_service.cpp
    src/csi/node_service.cpp
    src/spdk/spdk_wrapper.cpp
    src/spdk/volume_manager.cpp
    src/spdk/raid_manager.cpp
    src/spdk/nvmeof_manager.cpp
    src/dashboard/dashboard_service.cpp
    src/node_agent/node_agent.cpp
    src/controller_operator/controller_operator.cpp
    src/utils/config.cpp
    src/utils/kube_client.cpp
    ${CSI_PB_CPP_FILE}
    ${CSI_GRPC_CPP_FILE}
)

# Main executable
add_executable(spdk_flint ${SOURCES})

# Link libraries
target_link_libraries(spdk_flint
    ${SPDK_LIBRARIES}
    ${Protobuf_LIBRARIES}
    gRPC::grpc++
    spdlog::spdlog
    Threads::Threads
    CURL::libcurl
    OpenSSL::SSL
    OpenSSL::Crypto
    dl
    rt
    uuid
    numa
)

# If using FetchContent Crow
if(TARGET Crow::Crow)
    target_link_libraries(spdk_flint Crow::Crow)
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