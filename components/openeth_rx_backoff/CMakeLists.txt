idf_component_register(REQUIRES esp_eth)

if(CMAKE_BUILD_EARLY_EXPANSION OR NOT CONFIG_ETH_USE_OPENETH)
    return()
endif()

# ESP-IDF 6.1 retries malloc without blocking in the high-priority OpenETH
# receive task. On C3 a transient low-heap condition then starves the tasks
# that could finish their work and free buffers. Patch a build-local copy;
# never mutate the SDK checkout or alter the hardware WiFi drivers.
idf_component_get_property(eth_lib esp_eth COMPONENT_LIB)
idf_component_get_property(eth_dir esp_eth COMPONENT_DIR)

# IDF 6 moved DP83848 out of the SDK. esp-idf-svc's OpenETH constructor still
# needs its factory, but the managed driver's target list excludes emulated
# S3/C3. Compile the unmodified official 1.1.0 driver only for OpenETH.
if(IDF_VERSION_MAJOR GREATER_EQUAL 6)
    target_sources(${eth_lib} PRIVATE
        "${CMAKE_CURRENT_LIST_DIR}/vendor/dp83848/esp_eth_phy_dp83848.c")
endif()

set(original "${eth_dir}/src/openeth/esp_eth_mac_openeth.c")
file(SHA256 "${original}" source_hash)
if(NOT source_hash STREQUAL "b0526b76120abffe37ee8bd87b9e204880da18a5c02383e5d39b8e001bbcbb7d")
    message(FATAL_ERROR "OpenETH backoff requires review for this ESP-IDF source hash")
endif()
file(READ "${original}" source)
set(before "ESP_LOGE(TAG, \"no mem for receive buffer\");")
set(after "ESP_LOGE(TAG, \"no mem for receive buffer\");\n                    vTaskDelay(1); // Let lower-priority buffer owners make progress.")
string(REGEX MATCHALL "no mem for receive buffer" occurrences "${source}")
list(LENGTH occurrences count)
if(NOT count EQUAL 1)
    message(FATAL_ERROR "OpenETH receive backoff needs review for this ESP-IDF source")
endif()
string(REPLACE "${before}" "${after}" patched "${source}")
if(patched STREQUAL source)
    message(FATAL_ERROR "OpenETH receive allocation guard no longer matches")
endif()
set(replacement "${CMAKE_CURRENT_BINARY_DIR}/esp_eth_mac_openeth_backoff.c")
file(WRITE "${replacement}" "${patched}")
get_target_property(sources ${eth_lib} SOURCES)
set(found FALSE)
foreach(candidate IN LISTS sources)
    if(candidate MATCHES "(^|/)esp_eth_mac_openeth\\.c$")
        list(REMOVE_ITEM sources "${candidate}")
        set(found TRUE)
    endif()
endforeach()
if(NOT found)
    message(FATAL_ERROR "OpenETH receive source is absent from esp_eth target")
endif()
set_property(TARGET ${eth_lib} PROPERTY SOURCES "${sources}")
target_sources(${eth_lib} PRIVATE "${replacement}")
target_include_directories(${eth_lib} PRIVATE "${eth_dir}/src/openeth")
message(STATUS "OpenETH: block for one tick after a failed receive allocation")
