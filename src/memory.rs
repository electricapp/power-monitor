//! RAM and swap usage via Mach `host_statistics64` and `vm.swapusage` sysctl.

use std::ffi::c_void;
use std::mem;
use std::sync::OnceLock;

unsafe extern "C" {
    /// Returns the host port for the current machine. No release needed.
    pub fn mach_host_self() -> u32;
    /// Fill `info` with system statistics. `flavor = 4` (`HOST_VM_INFO64`) for VM stats.
    pub fn host_statistics64(host: u32, flavor: i32, info: *mut c_void, count: *mut u32) -> i32;
    fn sysctlbyname(
        name: *const u8,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *const c_void,
        newlen: usize,
    ) -> i32;
}

const HOST_VM_INFO64: i32 = 4;
const HOST_VM_INFO64_COUNT: u32 = 40; // sizeof(vm_statistics64_data_t) / sizeof(int)

/// Layout matches `vm_statistics64_data_t` from <mach/vm_statistics.h>.
/// 160 bytes total on arm64 macOS.
#[repr(C)]
struct VmStats {
    free_count: u32,                             //  0
    active_count: u32,                           //  4
    inactive_count: u32,                         //  8
    wire_count: u32,                             // 12
    zero_fill_count: u64,                        // 16
    reactivations: u64,                          // 24
    pageins: u64,                                // 32
    pageouts: u64,                               // 40
    faults: u64,                                 // 48
    cow_faults: u64,                             // 56
    lookups: u64,                                // 64
    hits: u64,                                   // 72
    purges: u64,                                 // 80
    purgeable_count: u32,                        // 88
    speculative_count: u32,                      // 92
    decompressions: u64,                         // 96
    compressions: u64,                           // 104
    swapins: u64,                                // 112
    swapouts: u64,                               // 120
    compressor_page_count: u32,                  // 128
    throttled_count: u32,                        // 132
    external_page_count: u32,                    // 136
    internal_page_count: u32,                    // 140
    total_uncompressed_pages_in_compressor: u64, // 144
    swapped_out_vm_total: u64,                   // 152
                                                 // 160 bytes = 40 × sizeof(i32)
}

/// Layout matches `struct xsw_usage` from <sys/sysctl.h>.
#[repr(C)]
struct XswUsage {
    xsu_total: u64,
    xsu_avail: u64,
    xsu_used: u64,
    xsu_pagesize: u32,
    xsu_encrypted: u32, // boolean_t
}

// ── Public types ──────────────────────────────────────────────────────────────

/// Physical RAM statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MemoryInfo {
    /// Total physical RAM in bytes.
    pub total: u64,
    /// Active + wired pages in bytes (app-visible usage).
    pub used: u64,
    /// Pages available for reuse (free + inactive + speculative) in bytes.
    pub available: u64,
    /// Wired (non-reclaimable) pages in bytes.
    pub wired: u64,
    /// Pages currently held in the compressor in bytes.
    pub compressed: u64,
}

/// Swap file statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SwapInfo {
    pub total: u64,
    pub used: u64,
    pub free: u64,
}

// ── Implementation ────────────────────────────────────────────────────────────

/// Read a scalar sysctl key into a `T` (assumes the kernel returns exactly
/// `size_of::<T>()` bytes). Returns `default` on failure.
fn sysctl_scalar<T: Copy>(name: &std::ffi::CStr, default: T) -> T {
    let mut val = default;
    let mut len = mem::size_of::<T>();
    unsafe {
        sysctlbyname(
            name.as_ptr() as *const u8,
            &mut val as *mut _ as *mut c_void,
            &mut len,
            std::ptr::null(),
            0,
        );
    }
    val
}

/// Both `hw.pagesize` and `hw.memsize` are immutable for the process lifetime;
/// cache the first sysctl result and reuse on every `read_memory` call.
fn page_size() -> u64 {
    static CACHE: OnceLock<u64> = OnceLock::new();
    *CACHE.get_or_init(|| sysctl_scalar::<usize>(c"hw.pagesize", 16384) as u64)
}

fn total_ram() -> u64 {
    static CACHE: OnceLock<u64> = OnceLock::new();
    *CACHE.get_or_init(|| sysctl_scalar::<u64>(c"hw.memsize", 0))
}

/// Read current physical memory statistics.
pub fn read_memory() -> Option<MemoryInfo> {
    let ps = page_size();
    if ps == 0 {
        return None;
    }

    let mut stats: VmStats = unsafe { mem::zeroed() };
    let mut count = HOST_VM_INFO64_COUNT;

    let kr = unsafe {
        host_statistics64(
            mach_host_self(),
            HOST_VM_INFO64,
            &mut stats as *mut _ as *mut c_void,
            &mut count,
        )
    };
    if kr != 0 {
        return None;
    }

    let total = total_ram();

    // Activity Monitor formula (confirmed by macmon):
    // used = (active + inactive + wire + speculative + compressor - purgeable - external) × page
    let used = (stats.active_count as u64
        + stats.inactive_count as u64
        + stats.wire_count as u64
        + stats.speculative_count as u64
        + stats.compressor_page_count as u64)
        .saturating_sub(stats.purgeable_count as u64)
        .saturating_sub(stats.external_page_count as u64)
        * ps;

    let available = stats.free_count as u64 * ps;
    let wired = stats.wire_count as u64 * ps;
    let compressed = stats.compressor_page_count as u64 * ps;

    Some(MemoryInfo {
        total,
        used,
        available,
        wired,
        compressed,
    })
}

/// Read swap file statistics.
pub fn read_swap() -> Option<SwapInfo> {
    let mut usage: XswUsage = unsafe { mem::zeroed() };
    let mut len = mem::size_of::<XswUsage>();
    let kr = unsafe {
        sysctlbyname(
            c"vm.swapusage".as_ptr() as *const u8,
            &mut usage as *mut _ as *mut c_void,
            &mut len,
            std::ptr::null(),
            0,
        )
    };
    if kr != 0 {
        return None;
    }

    Some(SwapInfo {
        total: usage.xsu_total,
        used: usage.xsu_used,
        free: usage.xsu_avail,
    })
}
