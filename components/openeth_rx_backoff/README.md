The pinned ESP-IDF v6.1 OpenCores Ethernet receive task retries a failed
packet allocation in a tight loop at high priority. On single-core C3 this
can starve the buffer owners that would release memory. The reproduced QEMU
failure produced over 500,000 allocation-error lines and stopped serving HTTP
immediately after receiving the first post-reboot encrypted ping.

This component compiles a build-local copy of that driver with `vTaskDelay(1)`
after a failed allocation. It preserves packet processing, retries, driver
priority, memory budgets and test deadlines. Persistent exhaustion still fails
the messaging test. It does not change WiFi or make a physical-board claim.
The original SDK checkout is untouched. Configuration fails if the allocation
guard or source target changes, requiring review when ESP-IDF is upgraded.

The driver source is byte-identical in ESP-IDF v5.5.5 and v6.1. The SHA-256
guard remains unchanged after reviewing both versions.

ESP-IDF 6 also moved the DP83848 PHY driver out of the SDK. The OpenETH
constructor in esp-idf-svc 0.53.0 still calls its factory. The official managed
component lists only physical EMAC targets, excluding the emulated S3/C3.

`vendor/dp83848` contains the C source, public header and Apache-2.0
license from Espressif's DP83848 1.1.0, tag `dp83848@v1.1.0`, commit
[`6ebd7a6e9e3f49ceb79f240da73a4d5839f5480a`](https://github.com/espressif/esp-eth-drivers/tree/6ebd7a6e9e3f49ceb79f240da73a4d5839f5480a/dp83848).
Local changes to this vendor copy add function documentation only.
CMake adds it to `esp_eth` only when OpenETH is enabled on IDF 6 or newer;
`bindings.h` exposes its factory under the same condition. Physical WiFi
builds do not compile this copy.
