The pinned ESP-IDF v5.5.5 OpenCores Ethernet receive task retries a failed
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
