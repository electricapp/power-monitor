//! C ABI shim for embedding the sampler in Swift / Objective-C / plain-C apps.
//!
//! See `include/power_monitor.h` for the C interface.
//!
//! Threading: the handle is a single mutable `Sampler`. Call from one thread
//! at a time. `pm_sampler_sample` blocks for `duration_ms` — invoke from a
//! background thread so the host app's UI doesn't stall.

use std::ffi::{c_char, c_void};
use std::ptr;

use crate::Sampler;
use crate::memory;

/// Crate version (`CARGO_PKG_VERSION`) as a NUL-terminated `'static` C string.
/// Single source of truth for the version — every UI reads from here.
#[unsafe(no_mangle)]
pub extern "C" fn pm_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0")
        .as_ptr()
        .cast::<c_char>()
}

/// Flat power / utilisation / memory snapshot. Mirrors [`crate::Metrics`]
/// minus nested struct layers so the C side can read fields directly.
#[repr(C)]
pub struct PmMetrics {
    pub sys_power: f32,
    pub cpu_power: f32,
    pub gpu_power: f32,
    pub ane_power: f32,
    pub dram_power: f32,
    pub cpu_temp: f32,
    pub gpu_temp: f32,
    pub pcpu_util: f32,
    pub pcpu_mhz: u32,
    pub ecpu_util: f32,
    pub ecpu_mhz: u32,
    pub gpu_util: f32,
    pub gpu_mhz: u32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub fan_rpm: u32,
    pub fan_max_rpm: u32,
    pub interval_ms: f32,
}

/// Static SoC info read once at `pm_sampler_new`.
#[repr(C)]
pub struct PmSocInfo {
    pub pcpu_cores: u32,
    pub ecpu_cores: u32,
    pub gpu_cores: u32,
    pub total_ram: u64,
}

/// Open all subsystems (IOReport + SMC + SoC probe). Returns an opaque handle,
/// or null if any subsystem fails.
#[unsafe(no_mangle)]
pub extern "C" fn pm_sampler_new() -> *mut c_void {
    match Sampler::new() {
        Some(s) => Box::into_raw(Box::new(s)).cast(),
        None => ptr::null_mut(),
    }
}

/// Close a handle returned by `pm_sampler_new`. Safe to call with null.
///
/// # Safety
/// `handle` must be a pointer previously returned by `pm_sampler_new` and not
/// already freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_sampler_free(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: handle originated from Box::into_raw(Box::new(Sampler)).
    unsafe {
        drop(Box::from_raw(handle.cast::<Sampler>()));
    }
}

/// Sample metrics over a `duration_ms` window. Writes into `out`. Blocks the
/// caller for the full duration.
///
/// Returns `false` if `handle` or `out` is null.
///
/// # Safety
/// `handle` must be a live `pm_sampler_new` handle. `out` must point to a
/// writable `PmMetrics`. The handle must not be accessed concurrently from
/// other threads.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_sampler_sample(
    handle: *mut c_void,
    duration_ms: u64,
    out: *mut PmMetrics,
) -> bool {
    if handle.is_null() || out.is_null() {
        return false;
    }
    // SAFETY: handle is a live Sampler; caller guarantees exclusive access.
    let sampler = unsafe { &mut *handle.cast::<Sampler>() };
    let m = sampler.get_metrics(duration_ms);
    let snap = PmMetrics {
        sys_power: m.sys_power,
        cpu_power: m.cpu_power,
        gpu_power: m.gpu_power,
        ane_power: m.ane_power,
        dram_power: m.dram_power,
        cpu_temp: m.cpu_temp,
        gpu_temp: m.gpu_temp,
        pcpu_util: m.pcpu.utilization,
        pcpu_mhz: m.pcpu.freq_mhz,
        ecpu_util: m.ecpu.utilization,
        ecpu_mhz: m.ecpu.freq_mhz,
        gpu_util: m.gpu_util,
        gpu_mhz: m.gpu_freq_mhz,
        mem_used: m.memory.used,
        mem_total: m.memory.total,
        swap_used: m.swap.used,
        swap_total: m.swap.total,
        fan_rpm: m.fan_rpm,
        fan_max_rpm: m.fan_max_rpm,
        interval_ms: m.interval_ms,
    };
    // SAFETY: out is non-null and properly aligned per the C ABI contract.
    unsafe {
        ptr::write(out, snap);
    }
    true
}

/// Populate static SoC info. Returns `false` on null inputs.
///
/// # Safety
/// `handle` must be a live `pm_sampler_new` handle. `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_sampler_soc_info(handle: *const c_void, out: *mut PmSocInfo) -> bool {
    if handle.is_null() || out.is_null() {
        return false;
    }
    // SAFETY: handle is a live Sampler; shared read only.
    let sampler = unsafe { &*handle.cast::<Sampler>() };
    let info = PmSocInfo {
        pcpu_cores: sampler.soc.pcpu_level().map_or(0, |l| l.cores),
        ecpu_cores: sampler.soc.ecpu_level().map_or(0, |l| l.cores),
        gpu_cores: sampler.soc.gpu_cores,
        total_ram: memory::read_memory().map_or(0, |m| m.total),
    };
    // SAFETY: out is non-null and properly aligned.
    unsafe {
        ptr::write(out, info);
    }
    true
}

/// Copy the chip name (e.g. `"Apple M5"`) into `buf` as a NUL-terminated
/// UTF-8 string. Truncates to `buf_len - 1` bytes if necessary.
///
/// Returns the number of bytes written, excluding the trailing NUL.
///
/// # Safety
/// `handle` must be a live `pm_sampler_new` handle. `buf` must be a writable
/// buffer of at least `buf_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_sampler_chip_name(
    handle: *const c_void,
    buf: *mut u8,
    buf_len: usize,
) -> usize {
    if handle.is_null() || buf.is_null() || buf_len == 0 {
        return 0;
    }
    // SAFETY: handle is a live Sampler; shared read only.
    let sampler = unsafe { &*handle.cast::<Sampler>() };
    let bytes = sampler.soc.chip_name.as_bytes();
    let n = bytes.len().min(buf_len - 1);
    // SAFETY: buf has at least buf_len bytes; n < buf_len.
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), buf, n);
        *buf.add(n) = 0;
    }
    n
}
