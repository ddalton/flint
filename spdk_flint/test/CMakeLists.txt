# Tests CMakeLists.txt

# Test sources
set(TEST_SOURCES
    test_main.cpp
    # TODO: Add these test files as they are implemented:
    # test_spdk_wrapper.cpp
    # test_volume_manager.cpp
    # test_raid_manager.cpp
    # test_logging.cpp
    # test_config.cpp
)

# Create test executable
add_executable(spdk_flint_tests ${TEST_SOURCES})

# Add comprehensive linker flags to resolve DSO and symbol issues  
set_target_properties(spdk_flint_tests PROPERTIES
    LINK_FLAGS "-Wl,--no-as-needed -Wl,--allow-multiple-definition -Wl,--undefined-version"
)

# Link against main application library and all its dependencies
target_link_libraries(spdk_flint_tests
    spdk_flint_lib
    
    # Since we removed PUBLIC linking, we need to explicitly add all dependencies
    ${Protobuf_LIBRARIES}
    ${gRPC_LIBRARIES}
    ${spdlog_LIBRARIES}
    Threads::Threads
    ${CURL_LIBRARIES}
    # System libraries
    dl
    rt
    uuid
    numa
    pthread
    m
    # OpenSSL libraries
    -lssl
    -lcrypto
    ${CMAKE_DL_LIBS}
)

# Link Catch2
if(TARGET Catch2::Catch2WithMain)
    target_link_libraries(spdk_flint_tests Catch2::Catch2WithMain)
else()
    target_link_libraries(spdk_flint_tests Catch2::Catch2)
endif()

# Include test-specific headers
target_include_directories(spdk_flint_tests PRIVATE
    ${CMAKE_CURRENT_SOURCE_DIR}
    ${CMAKE_CURRENT_SOURCE_DIR}/../include
    ${CMAKE_CURRENT_SOURCE_DIR}/../src
    ${GENERATED_PROTOBUF_PATH}
    ${gRPC_INCLUDE_DIRS}
    ${Protobuf_INCLUDE_DIRS}
    ${spdlog_INCLUDE_DIRS}
)

# Compiler definitions for tests
target_compile_definitions(spdk_flint_tests PRIVATE
    ${SPDK_CFLAGS_OTHER}
    CATCH_CONFIG_MAIN
    TESTING
)

# Add as a test
add_test(NAME spdk_flint_unit_tests COMMAND spdk_flint_tests)

# Integration tests (requires SPDK to be running) - DISABLED until files exist
# add_executable(spdk_flint_integration_tests
#     integration_test_main.cpp
#     integration_test_spdk.cpp
#     integration_test_csi.cpp
# )

# DISABLED - Integration test configuration (until test files exist)
# target_link_libraries(spdk_flint_integration_tests
#     ${SPDK_LIBRARIES}
#     ${Protobuf_LIBRARIES}
#     ${gRPC_LIBRARIES}
#     ${spdlog_LIBRARIES}
#     Threads::Threads
#     dl
#     rt
#     uuid
#     numa
# )
# 
# if(TARGET Catch2::Catch2WithMain)
#     target_link_libraries(spdk_flint_integration_tests Catch2::Catch2WithMain)
# else()
#     target_link_libraries(spdk_flint_integration_tests Catch2::Catch2)
# endif()
# 
# target_include_directories(spdk_flint_integration_tests PRIVATE
#     ${CMAKE_CURRENT_SOURCE_DIR}
#     ${CMAKE_CURRENT_SOURCE_DIR}/../include
#     ${CMAKE_CURRENT_SOURCE_DIR}/../src
#     ${GENERATED_PROTOBUF_PATH}
#     ${gRPC_INCLUDE_DIRS}
#     ${Protobuf_INCLUDE_DIRS}
#     ${spdlog_INCLUDE_DIRS}
# )
# 
# target_compile_definitions(spdk_flint_integration_tests PRIVATE
#     ${SPDK_CFLAGS_OTHER}
#     CATCH_CONFIG_MAIN
#     TESTING
#     INTEGRATION_TESTING
# )
# 
# # Integration tests require SPDK environment
# add_test(NAME spdk_flint_integration_tests 
#          COMMAND spdk_flint_integration_tests
#          WORKING_DIRECTORY ${CMAKE_CURRENT_BINARY_DIR})
# set_tests_properties(spdk_flint_integration_tests PROPERTIES
#     ENVIRONMENT "SPDK_RPC_URL=unix:///var/tmp/spdk.sock"
#     LABELS "integration"
# ) 