#pragma once
#include "sdkconfig.h"
#include "esp_idf_version.h"

#if defined(CONFIG_ETH_USE_OPENETH) && ESP_IDF_VERSION >= ESP_IDF_VERSION_VAL(6, 0, 0)
#include "vendor/dp83848/include/esp_eth_phy_dp83848.h"
#endif
