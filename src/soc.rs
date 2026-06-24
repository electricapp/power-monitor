//! Apple Silicon SoC configuration via sysctl and IOKit.
//!
//! Reads chip name, performance level core counts, and attempts to read
//! CPU frequency tables. Frequency tables are also populated lazily by
//! the Sampler from IOReport CPU Stats state names on the first sample.

use std::ffi::{CString, c_void};
use std::mem;

unsafe extern "C" {
    fn sysctlbyname(
        name: *const u8,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *const c_void,
        newlen: usize,
    ) -> i32;

    // IOKit (already linked via framework=IOKit in build.rs)
    /// Returns the first IOService matching the given matching dict (consumes dict).
    pub fn IOServiceGetMatchingService(port: u32, matching: *const c_void) -> u32;
    /// Iterate all services matching the dict (consumes dict). Returns an iterator object.
    pub fn IOServiceGetMatchingServices(
        port: u32,
        matching: *const c_void,
        existing: *mut u32,
    ) -> i32;
    /// Advance an IOKit iterator; returns the next object (0 = exhausted). Caller must IOObjectRelease.
    pub fn IOIteratorNext(iterator: u32) -> u32;
    /// Create a matching dict for a service by class name. Caller must CFRelease.
    pub fn IOServiceMatching(name: *const u8) -> *const c_void;
    /// Open a registry entry directly by its IORegistry path string.
    pub fn IORegistryEntryFromPath(master_port: u32, path: *const u8) -> u32;

    /// Read a single named property from a registry entry.
    pub fn IORegistryEntryCreateCFProperty(
        entry: u32,
        key: *const c_void,
        allocator: *const c_void,
        options: u32,
    ) -> *const c_void;

    /// Read all properties of a registry entry into a new mutable dictionary.
    /// Caller must CFRelease the returned dict.
    pub fn IORegistryEntryCreateCFProperties(
        entry: u32,
        properties: *mut *const c_void, // *mut CFMutableDictionaryRef
        allocator: *const c_void,
        options: u32,
    ) -> i32; // kern_return_t

    /// Search a registry entry and its parents/children for a named property.
    /// `plane` is typically `b"IOService\0"`. Traverses up the tree if not found locally.
    pub fn IORegistryEntrySearchCFProperty(
        entry: u32,
        plane: *const u8,
        key: *const c_void,
        allocator: *const c_void,
        options: u32,
    ) -> *const c_void;

    /// Release an IOKit object (decrements retain count).
    pub fn IOObjectRelease(obj: u32) -> i32;

    // CoreFoundation
    pub fn CFRetain(cf: *const c_void) -> *const c_void;
    /// Decrement retain count; frees when it reaches zero.
    pub fn CFRelease(cf: *const c_void);
    /// Extract a number from a CFNumber into a native type. `the_type` is a `CFNumberType`.
    pub fn CFNumberGetValue(number: *const c_void, the_type: i32, value: *mut c_void) -> bool;
    fn CFStringCreateWithCString(alloc: *const c_void, s: *const u8, enc: u32) -> *const c_void;
    pub fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
    pub fn CFDataGetLength(data: *const c_void) -> isize;
    /// Runtime type identifier of a CF object (`CFTypeID` is `unsigned long`).
    pub fn CFGetTypeID(cf: *const c_void) -> usize;
    /// The `CFTypeID` shared by all `CFData` objects.
    pub fn CFDataGetTypeID() -> usize;
}

const CF_UTF8: u32 = 0x0800_0100;
const CF_NUMBER_SINT32: i32 = 3;

// ── sysctl helpers ────────────────────────────────────────────────────────────

fn sysctl_string(name: &str) -> Option<String> {
    let cname = CString::new(name).ok()?;
    unsafe {
        let mut len: usize = 0;
        if sysctlbyname(
            cname.as_ptr() as *const u8,
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null(),
            0,
        ) != 0
        {
            return None;
        }
        let mut buf = vec![0u8; len];
        if sysctlbyname(
            cname.as_ptr() as *const u8,
            buf.as_mut_ptr() as *mut c_void,
            &mut len,
            std::ptr::null(),
            0,
        ) != 0
        {
            return None;
        }
        while buf.last() == Some(&0) {
            buf.pop();
        }
        String::from_utf8(buf).ok()
    }
}

fn sysctl_u32(name: &str) -> Option<u32> {
    let cname = CString::new(name).ok()?;
    let mut val: u32 = 0;
    let mut len = mem::size_of::<u32>();
    unsafe {
        if sysctlbyname(
            cname.as_ptr() as *const u8,
            &mut val as *mut _ as *mut c_void,
            &mut len,
            std::ptr::null(),
            0,
        ) != 0
        {
            return None;
        }
    }
    Some(val)
}

/// Read a packed array of `T` values from a sysctl key.
///
/// Single allocation: probes the size, allocates exactly once, truncates to
/// the actual byte count returned by the kernel.
fn sysctl_array<T: Copy + Default>(name: &str) -> Vec<T> {
    let Ok(cname) = CString::new(name) else {
        return Vec::new();
    };
    let stride = mem::size_of::<T>();
    unsafe {
        let mut len: usize = 0;
        if sysctlbyname(
            cname.as_ptr() as *const u8,
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null(),
            0,
        ) != 0
            || len == 0
        {
            return Vec::new();
        }
        let mut buf = vec![T::default(); len / stride];
        if sysctlbyname(
            cname.as_ptr() as *const u8,
            buf.as_mut_ptr() as *mut c_void,
            &mut len,
            std::ptr::null(),
            0,
        ) != 0
        {
            return Vec::new();
        }
        buf.truncate(len / stride);
        buf
    }
}

#[inline]
fn sysctl_u32s(name: &str) -> Vec<u32> {
    sysctl_array(name)
}

/// Read a packed array of u64 values (used for Hz-precision frequency tables).
#[inline]
fn sysctl_u64s(name: &str) -> Vec<u64> {
    sysctl_array(name)
}

// ── IOKit helpers ─────────────────────────────────────────────────────────────

/// Create a `CFString` from `s`. Returns null (not a panic) if `s` contains an
/// interior NUL or the encoding fails; callers must null-check before use and
/// must not `CFRelease` a null result.
fn cfstr_raw(s: &str) -> *const c_void {
    let Ok(cs) = CString::new(s) else {
        return std::ptr::null();
    };
    unsafe { CFStringCreateWithCString(std::ptr::null(), cs.as_ptr() as *const u8, CF_UTF8) }
}

/// Read a packed `CFData` property as a slice of `T` values.
/// Caller must ensure `T` layout matches the raw bytes (e.g. `u32` for freq tables).
unsafe fn read_iokit_data<T: Copy>(service: u32, property_name: &str) -> Vec<T> {
    unsafe {
        let key = cfstr_raw(property_name);
        if key.is_null() {
            return Vec::new();
        }
        let prop = IORegistryEntryCreateCFProperty(service, key, std::ptr::null(), 0);
        CFRelease(key);
        if prop.is_null() {
            return Vec::new();
        }

        // The property may be any CF type (CFNumber, CFString, …). Calling the
        // CFData accessors on a non-CFData object is undefined behaviour, so
        // verify the runtime type first and release otherwise.
        if CFGetTypeID(prop) != CFDataGetTypeID() {
            CFRelease(prop);
            return Vec::new();
        }

        let len = CFDataGetLength(prop) as usize;
        let bytes = CFDataGetBytePtr(prop);
        if bytes.is_null() {
            CFRelease(prop);
            return Vec::new();
        }
        let count = len / mem::size_of::<T>();
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let ptr = bytes.add(i * mem::size_of::<T>()) as *const T;
            out.push(ptr.read_unaligned());
        }
        CFRelease(prop);
        out
    }
}

/// Scan all `AppleARMIODevice` nodes and print every `voltage-states*` property
/// that contains parseable frequency data. Prints to stdout for the probe binary.
/// Used to discover the correct property names on new chip generations.
pub fn probe_voltage_state_properties() {
    // Generate candidate names: voltage-states{N} and voltage-states{N}-sram for N 1..=15
    let candidates: Vec<String> = (1u32..=15)
        .flat_map(|n| {
            [
                format!("voltage-states{n}-sram"),
                format!("voltage-states{n}"),
            ]
        })
        .collect();

    unsafe {
        let matching = IOServiceMatching(c"AppleARMIODevice".as_ptr() as *const u8);
        if matching.is_null() {
            println!("  IOServiceMatching failed");
            return;
        }

        let mut iter: u32 = 0;
        if IOServiceGetMatchingServices(0, matching, &mut iter) != 0 || iter == 0 {
            println!("  IOServiceGetMatchingServices failed");
            return;
        }

        let mut node_idx = 0;
        loop {
            let service = IOIteratorNext(iter);
            if service == 0 {
                break;
            }
            node_idx += 1;

            for prop_name in &candidates {
                let data: Vec<u8> = read_iokit_data(service, prop_name);
                if data.is_empty() {
                    continue;
                }
                let freqs = parse_voltage_states(&data);
                if !freqs.is_empty() {
                    println!(
                        "  node {node_idx}: {prop_name} ({} bytes, {} freqs): {:?}",
                        data.len(),
                        freqs.len(),
                        &freqs
                    );
                } else {
                    println!(
                        "  node {node_idx}: {prop_name} ({} bytes, could not parse as freq table)",
                        data.len()
                    );
                }
            }

            IOObjectRelease(service);
        }
        IOObjectRelease(iter);
    }
}

/// Parse a `voltage-states*` IORegistry property into MHz frequency values.
///
/// Each 8-byte entry is `[freq_u32_le, volt_u32_le]`. The frequency unit
/// varies by chip generation:
/// - M1–M4 GPU (`voltage-states9`): **Hz** — max ~2.5e9 → divide by 1_000_000
/// - M5 CPU (`voltage-states1/5-sram`): **kHz** — max ~4.5e6 → divide by 1_000
///
/// Unit is inferred from the max raw value:
/// - > 100_000_000 → Hz (divide by 1_000_000)
/// - > 100_000      → kHz (divide by 1_000)
/// - else           → already MHz
fn parse_voltage_states(data: &[u8]) -> Vec<u32> {
    // Each entry is 8 bytes: little-endian u32 freq + u32 voltage. We only
    // need the freq, parsed in a single pass alongside the running max so the
    // unit decision below doesn't require a second iteration.
    let n = data.len() / 8;
    let mut raw: Vec<u32> = Vec::with_capacity(n);
    let mut max = 0u32;
    for chunk in data.chunks_exact(8) {
        let f = u32::from_le_bytes(chunk[..4].try_into().unwrap());
        if f > max {
            max = f;
        }
        raw.push(f);
    }
    if raw.is_empty() {
        return Vec::new();
    }

    let divisor: u32 = if max > 100_000_000 {
        1_000_000
    } else if max > 100_000 {
        1_000
    } else {
        1
    };

    raw.into_iter()
        .map(|v| v / divisor)
        .filter(|&mhz| mhz >= 100)
        .collect()
}

/// Read CPU/GPU DVFS frequency tables from the `AppleARMIODevice` pmgr node.
///
/// Confirmed property names (M1–M5, macmon blog post, 2024):
///   `voltage-states1-sram` → E-cluster
///   `voltage-states5-sram` → P-cluster
///   `voltage-states9`      → GPU
///
/// Returns `(ecpu_mhz, pcpu_mhz, gpu_mhz)`.
fn read_dvfs_frequencies() -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    unsafe {
        // Iterate all AppleARMIODevice instances to find the pmgr node.
        let matching = IOServiceMatching(c"AppleARMIODevice".as_ptr() as *const u8);
        if matching.is_null() {
            return (Vec::new(), Vec::new(), Vec::new());
        }

        let mut iter: u32 = 0;
        if IOServiceGetMatchingServices(0, matching, &mut iter) != 0 || iter == 0 {
            return (Vec::new(), Vec::new(), Vec::new());
        }

        let mut ecpu = Vec::new();
        let mut pcpu = Vec::new();
        let mut gpu = Vec::new();

        loop {
            let service = IOIteratorNext(iter);
            if service == 0 {
                break;
            }

            let e: Vec<u8> = read_iokit_data(service, "voltage-states1-sram");
            // voltage-states5-sram = P-cluster (M1–M5 base/Pro/Max)
            // voltage-states11-sram = second P-cluster on Max/Ultra chips
            let p: Vec<u8> = {
                let v = read_iokit_data::<u8>(service, "voltage-states5-sram");
                if v.is_empty() {
                    read_iokit_data(service, "voltage-states11-sram")
                } else {
                    v
                }
            };
            let g: Vec<u8> = read_iokit_data(service, "voltage-states9");

            if !e.is_empty() {
                ecpu = parse_voltage_states(&e);
            }
            if !p.is_empty() {
                pcpu = parse_voltage_states(&p);
            }
            if !g.is_empty() {
                gpu = parse_voltage_states(&g);
            }

            IOObjectRelease(service);

            if !ecpu.is_empty() && !pcpu.is_empty() && !gpu.is_empty() {
                break;
            }
        }

        IOObjectRelease(iter);
        (ecpu, pcpu, gpu)
    }
}

fn read_iokit_u32(service_name: &str, property_name: &str) -> Option<u32> {
    unsafe {
        let matching = IOServiceMatching(CString::new(service_name).ok()?.as_ptr() as *const u8);
        if matching.is_null() {
            return None;
        }
        let service = IOServiceGetMatchingService(0, matching);
        if service == 0 {
            return None;
        }

        let key = cfstr_raw(property_name);
        if key.is_null() {
            IOObjectRelease(service);
            return None;
        }
        let prop = IORegistryEntryCreateCFProperty(service, key, std::ptr::null(), 0);
        CFRelease(key);
        IOObjectRelease(service);

        if prop.is_null() {
            return None;
        }
        let mut val: i32 = 0;
        let ok = CFNumberGetValue(prop, CF_NUMBER_SINT32, &mut val as *mut _ as *mut c_void);
        CFRelease(prop);
        if ok { Some(val as u32) } else { None }
    }
}

// ── Public types ──────────────────────────────────────────────────────────────

/// One CPU performance level (cluster tier) as reported by sysctl.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct PerfLevel {
    /// Human-readable name: `"Performance"`, `"Efficiency"`, `"Standard"`, etc.
    pub name: String,
    /// Physical core count.
    pub cores: u32,
    /// Operating frequencies in MHz, ascending. Empty until populated from IOReport
    /// state names on the first sample (see [`crate::Sampler`]).
    pub freqs_mhz: Vec<u32>,
}

/// Apple Silicon SoC configuration.
///
/// Built once at startup via [`SocInfo::from_system`]. CPU `freqs_mhz` are read
/// from sysctl; GPU freqs are attempted from IORegistry.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct SocInfo {
    /// Marketing name, e.g. `"Apple M5"`.
    pub chip_name: String,
    /// Performance levels ordered highest→lowest (index 0 = P-cluster / PCPU).
    pub perf_levels: Vec<PerfLevel>,
    /// Number of GPU shader cores from IORegistry `AGXAccelerator`. Zero if unavailable.
    pub gpu_cores: u32,
    /// GPU P-state frequencies in MHz from IORegistry. Empty if the property is absent.
    /// When non-empty, index `i` maps to the `i`-th active GPU P-state.
    pub gpu_freqs_mhz: Vec<u32>,
}

impl SocInfo {
    /// Read SoC configuration from sysctl and IOKit with safe fallbacks.
    ///
    /// Never fails — missing sysctl keys or IOKit lookups silently default:
    /// - `chip_name` → `"Apple Silicon"`
    /// - `perf_levels` → 2 levels with empty frequency tables
    /// - `gpu_cores` → 0
    ///
    /// Frequency tables (`freqs_mhz`) are populated lazily by [`crate::Sampler`]
    /// from IOReport state names on the first sample.
    pub fn from_system() -> Self {
        let chip_name = sysctl_string("machdep.cpu.brand_string")
            .unwrap_or_else(|| "Apple Silicon".to_string());

        let nlevels = sysctl_u32("hw.nperflevels").unwrap_or(2) as usize;

        let mut levels: Vec<PerfLevel> = Vec::with_capacity(nlevels);
        for i in 0..nlevels {
            let name = sysctl_string(&format!("hw.perflevel{i}.name"))
                .unwrap_or_else(|| format!("Level{i}"));
            let cores = sysctl_u32(&format!("hw.perflevel{i}.physicalcpu")).unwrap_or(0);

            // hw.perflevelN.frequencies may return Hz (u64) or MHz (u32).
            // Try u64s first (Hz), convert; fall back to u32s.
            let freqs_mhz = {
                let hz64 = sysctl_u64s(&format!("hw.perflevel{i}.frequencies"));
                if !hz64.is_empty() {
                    // Convert Hz → MHz; if values are already < 100_000 they're probably MHz
                    if hz64[0] > 100_000 {
                        hz64.iter().map(|&v| (v / 1_000_000) as u32).collect()
                    } else {
                        hz64.iter().map(|&v| v as u32).collect()
                    }
                } else {
                    let mhz32 = sysctl_u32s(&format!("hw.perflevel{i}.frequencies"));
                    if mhz32.iter().all(|&v| v < 100_000) {
                        mhz32 // already MHz
                    } else {
                        mhz32.iter().map(|&v| v / 1_000_000).collect()
                    }
                }
            };

            levels.push(PerfLevel {
                name,
                cores,
                freqs_mhz,
            });
        }

        // perflevel0 is always the highest-performance cluster on all M-series chips.
        // Lower indices = higher performance.

        let gpu_cores = read_iokit_u32("AGXAccelerator", "gpu-core-count").unwrap_or(0);

        // Read CPU/GPU DVFS frequency tables from AppleARMIODevice pmgr node.
        // perflevel0 = P-cluster (highest), last = E-cluster (lowest).
        let (ecpu_dvfs, pcpu_dvfs, gpu_freqs_mhz) = read_dvfs_frequencies();

        // Populate freq tables into the appropriate perf levels if sysctl left them empty.
        if let Some(l) = levels.first_mut()
            && l.freqs_mhz.is_empty()
        {
            l.freqs_mhz = pcpu_dvfs;
        }
        if let Some(l) = levels.last_mut()
            && l.freqs_mhz.is_empty()
        {
            l.freqs_mhz = ecpu_dvfs;
        }

        SocInfo {
            chip_name,
            perf_levels: levels,
            gpu_cores,
            gpu_freqs_mhz,
        }
    }

    // ── Convenience accessors ─────────────────────────────────────────────────

    /// Highest-performance cluster (PCPU / "Super" on M5).
    pub fn pcpu_level(&self) -> Option<&PerfLevel> {
        self.perf_levels.first()
    }

    /// Efficiency cluster (ECPU / lowest tier).
    pub fn ecpu_level(&self) -> Option<&PerfLevel> {
        self.perf_levels.last()
    }

    /// Total logical CPU count across all clusters.
    pub fn total_cores(&self) -> u32 {
        self.perf_levels.iter().map(|l| l.cores).sum()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::parse_voltage_states;

    fn entry(freq_raw: u32, volt: u32) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&freq_raw.to_le_bytes());
        b[4..].copy_from_slice(&volt.to_le_bytes());
        b
    }

    fn make_data(entries: &[(u32, u32)]) -> Vec<u8> {
        entries.iter().flat_map(|&(f, v)| entry(f, v)).collect()
    }

    #[test]
    fn hz_values_m1_to_m4_style() {
        // M1-M4 style: frequencies in Hz
        // 600 MHz = 600_000_000 Hz; 912 MHz = 912_000_000 Hz
        let data = make_data(&[(600_000_000, 850), (912_000_000, 900)]);
        let freqs = parse_voltage_states(&data);
        assert_eq!(freqs, vec![600, 912]);
    }

    #[test]
    fn khz_values_m5_style() {
        // M5 style: frequencies in kHz
        // 972 MHz = 972_000 kHz; 1308 MHz = 1_308_000 kHz
        let data = make_data(&[(972_000, 850), (1_308_000, 900)]);
        let freqs = parse_voltage_states(&data);
        assert_eq!(freqs, vec![972, 1308]);
    }

    #[test]
    fn empty_data_returns_empty() {
        assert!(parse_voltage_states(&[]).is_empty());
    }

    #[test]
    fn sub_100mhz_entries_filtered_out() {
        // Zero-frequency entries (power-off state) should be excluded
        let data = make_data(&[(0, 0), (600_000_000, 850)]);
        let freqs = parse_voltage_states(&data);
        assert_eq!(freqs, vec![600]);
    }

    #[test]
    fn partial_entry_at_end_ignored() {
        // 8 valid bytes + 4 trailing bytes → only 1 valid entry
        let mut data = make_data(&[(600_000_000, 850)]);
        data.extend_from_slice(&[0u8; 4]);
        let freqs = parse_voltage_states(&data);
        assert_eq!(freqs, vec![600]);
    }
}
