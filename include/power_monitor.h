// power-monitor — C interface. See src/ffi.rs for implementation.
//
// Link against libpower_monitor.a plus:
//   -framework IOKit -framework CoreFoundation -lIOReport
//
// All calls are macOS / Apple Silicon only.

#ifndef POWER_MONITOR_H
#define POWER_MONITOR_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
    float    sys_power;   // watts, SMC PSTR rail
    float    cpu_power;   // watts, IOReport CPU energy model
    float    gpu_power;   // watts, IOReport GPU delta
    float    ane_power;   // watts, Apple Neural Engine
    float    dram_power;  // watts, memory subsystem
    float    cpu_temp;    // °C, max across Tp01..Tp0f (NaN if rail gated)
    float    gpu_temp;    // °C, max across Tg0a..Tg0f (NaN if rail gated)
    float    pcpu_util;   // 0..1
    uint32_t pcpu_mhz;
    float    ecpu_util;   // 0..1
    uint32_t ecpu_mhz;
    float    gpu_util;    // 0..1
    uint32_t gpu_mhz;
    uint64_t mem_used;    // bytes
    uint64_t mem_total;   // bytes
    uint64_t swap_used;   // bytes
    uint64_t swap_total;  // bytes
    uint32_t fan_rpm;     // highest-duty fan current RPM (0 if fanless)
    uint32_t fan_max_rpm; // highest-duty fan max RPM (0 if fanless)
    float    interval_ms; // full sample window duration
} PmMetrics;

typedef struct {
    uint32_t pcpu_cores;
    uint32_t ecpu_cores;
    uint32_t gpu_cores;
    uint64_t total_ram;   // bytes
} PmSocInfo;

// Open a sampler. Returns NULL if IOReport or SMC are unavailable.
void *pm_sampler_new(void);

// Close a sampler. NULL-safe.
void pm_sampler_free(void *handle);

// Block for `duration_ms`, then write averaged metrics into `out`.
// Returns false on NULL inputs.
bool pm_sampler_sample(void *handle, uint64_t duration_ms, PmMetrics *out);

// Populate static SoC info. Returns false on NULL inputs.
bool pm_sampler_soc_info(const void *handle, PmSocInfo *out);

// Copy chip name as NUL-terminated UTF-8 into `buf`. Returns bytes written.
size_t pm_sampler_chip_name(const void *handle, uint8_t *buf, size_t buf_len);

// Crate version (CARGO_PKG_VERSION). Static string, do not free.
const char *pm_version(void);

#ifdef __cplusplus
}
#endif

#endif  // POWER_MONITOR_H
