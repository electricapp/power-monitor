//! macOS Apple Silicon power and performance monitoring via IOKit SMC and IOReport FFI.
//!
//! No subprocess, no parsing, no `sudo`. All reads go directly to kernel interfaces.
//!
//! # Quick start
//!
//! ```no_run
//! // Averaged metrics over a 1-second window
//! let mut sampler = power_monitor::Sampler::new().unwrap();
//! let m = sampler.get_metrics(1000);
//! println!("CPU {:.2} W  GPU {:.2} W  sys {:.2} W", m.cpu_power, m.gpu_power, m.sys_power);
//! println!("PCPU {:.0}% @ {} MHz", m.pcpu.utilization * 100.0, m.pcpu.freq_mhz);
//! ```
//!
//! # Individual subsystems
//!
//! ```no_run
//! // SMC snapshot (power, temp, fans, voltage, current, battery)
//! let mut smc = power_monitor::Smc::open().unwrap();
//! let snap = smc.snapshot();
//! println!("System power: {:.1} W", snap.power.system);
//! println!("GPU temp: {:.1} °C", snap.thermal.gpu);
//! if snap.battery.is_present() {
//!     println!("Battery: {:.0}%", snap.battery.state_of_charge());
//! }
//!
//! // Arbitrary SMC key (see Apple SMC reverse-engineering docs)
//! if let Some(t) = smc.read_f32(b"Tp0a") {
//!     println!("P-cluster temp: {:.1} °C", t);
//! }
//! ```
//!
//! ```no_run
//! // Raw IOReport multi-group sampling — full control over channel routing
//! let mut ior = power_monitor::MultiGroupSampler::new().unwrap();
//! std::thread::sleep(std::time::Duration::from_millis(500));
//! let _interval_ms = ior.sample();
//! for ch in ior.channels() {
//!     println!("{}/{}: {:?}", ch.group.label(), ch.channel, ch.kind);
//! }
//! ```
//!
//! # Platform
//!
//! This crate compiles and runs **only on macOS** (Apple Silicon or Intel for SMC reads;
//! IOReport state channels require Apple Silicon for CPU/GPU utilisation data).

// TODO: publish to crates.io, Homebrew formula, MacPorts port
#![cfg(target_os = "macos")]

pub mod event;
pub mod ffi;
pub mod ioreport;
pub mod memory;
pub mod sampler;
pub mod serialize;
pub mod soc;

pub use event::{EventHook, SamplerEvent, set_event_hook};

pub use ioreport::{
    ChannelGroup,
    ChannelKind,
    ChannelSample,
    EnergyAccumulator,
    EnergyModel, // EnergyModel is a type alias for EnergySampler
    EnergyReading,
    EnergySampler,
    EnergyUnit,
    MultiGroupSampler,
    RawChannel,
    StateResidency,
};
pub use memory::{MemoryInfo, SwapInfo, read_memory, read_swap};
pub use sampler::{ClusterMetrics, Metrics, Sampler};
pub use soc::{PerfLevel, SocInfo};

/// Convenience alias for `Result<T, SmcError>`.
pub type SmcResult<T> = Result<T, SmcError>;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem;

// ── IOKit / Mach FFI ──────────────────────────────────────────────────────────

type IOReturn = i32;
type IOConnectT = u32;
type MachPortT = u32;

const KERNEL_INDEX_SMC: u32 = 2;
const SMC_CMD_WRITE_BYTES: u8 = 6;
const SMC_CMD_READ_BYTES: u8 = 5;
const SMC_CMD_GET_KEY_FROM_INDEX: u8 = 8;
const SMC_CMD_READ_KEYINFO: u8 = 9;

#[repr(C)]
#[derive(Clone, Copy)]
struct SmcKeyData {
    key: u32,
    vers: [u8; 6],
    p_limit_data: [u8; 16],
    key_info: SmcKeyInfoData,
    result: u8,
    status: u8,
    data8: u8,
    data32: u32,
    bytes: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SmcKeyInfoData {
    data_size: u32,
    data_type: u32,
    data_attributes: u8,
}

unsafe extern "C" {
    fn mach_task_self() -> MachPortT;
    fn IOServiceGetMatchingService(main_port: MachPortT, matching: *const c_void) -> u32;
    fn IOServiceMatching(name: *const u8) -> *const c_void;
    fn IOServiceOpen(
        service: u32,
        task: MachPortT,
        conn_type: u32,
        connect: *mut IOConnectT,
    ) -> IOReturn;
    fn IOServiceClose(connect: IOConnectT) -> IOReturn;
    fn IOConnectCallStructMethod(
        connection: IOConnectT,
        selector: u32,
        input: *const SmcKeyData,
        input_size: usize,
        output: *mut SmcKeyData,
        output_size: *mut usize,
    ) -> IOReturn;
    fn IOObjectRelease(object: u32) -> IOReturn;
}

const fn four_cc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

// ── Decoders ──────────────────────────────────────────────────────────────────

/// How to interpret raw SMC bytes as an `f32`.
///
/// Endianness on Apple Silicon SMC: float / fixed-point types (`flt `, `sp78`,
/// `ioft`) are little-endian; integer types (`ui16`, `si16`, `ui32`) are
/// big-endian. This is the inverse of the Intel-era macOS SMC convention and
/// is verified against known SMC readings (see crate-level tests).
#[derive(Clone, Copy, Debug)]
enum Decoder {
    /// IEEE 754 single-precision, little-endian (`flt `).
    Flt,
    /// Signed 8.8 fixed-point, little-endian (`sp78`).
    Sp78,
    /// Unsigned 16.16 fixed-point, little-endian (`ioft`).
    Ioft,
    /// Unsigned 8-bit integer (`ui8 `).
    Ui8,
    /// Unsigned 16-bit integer, big-endian (`ui16`).
    Ui16,
    /// Signed 16-bit integer, big-endian (`si16`).
    Si16,
    /// Unsigned 32-bit integer, big-endian (`ui32`).
    Ui32,
}

impl Decoder {
    fn decode(self, bytes: &[u8; 32]) -> f32 {
        match self {
            Decoder::Flt => f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            Decoder::Sp78 => i16::from_le_bytes([bytes[0], bytes[1]]) as f32 / 256.0,
            Decoder::Ioft => {
                u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f32 / 65536.0
            }
            Decoder::Ui8 => bytes[0] as f32,
            Decoder::Ui16 => u16::from_be_bytes([bytes[0], bytes[1]]) as f32,
            Decoder::Si16 => i16::from_be_bytes([bytes[0], bytes[1]]) as f32,
            Decoder::Ui32 => u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f32,
        }
    }

    fn encode(self, value: f32, bytes: &mut [u8; 32]) {
        match self {
            Decoder::Flt => bytes[..4].copy_from_slice(&value.to_le_bytes()),
            Decoder::Sp78 => bytes[..2].copy_from_slice(&((value * 256.0) as i16).to_le_bytes()),
            Decoder::Ioft => {
                bytes[..4].copy_from_slice(&((value * 65536.0) as u32).to_le_bytes())
            }
            Decoder::Ui8 => bytes[0] = value as u8,
            Decoder::Ui16 => bytes[..2].copy_from_slice(&(value as u16).to_be_bytes()),
            Decoder::Si16 => bytes[..2].copy_from_slice(&(value as i16).to_be_bytes()),
            Decoder::Ui32 => bytes[..4].copy_from_slice(&(value as u32).to_be_bytes()),
        }
    }
}

// ── Key table ─────────────────────────────────────────────────────────────────

/// Pre-resolved SMC key: encoded four-cc, cached data size/type, decoder.
#[derive(Clone, Copy)]
struct ResolvedKey {
    key: u32,
    data_size: u32,
    data_type: u32,
    decoder: Decoder,
}

// Key indices into the pre-resolved array.
// Power
const K_PSTR: usize = 0; // system total rail
const K_PPBR: usize = 1; // SoC package base rail
const K_PHPC: usize = 2; // P-core (HPC) cluster
const K_PDTR: usize = 3; // intermediate DC rail (NOT DRAM — see VD0R)
const K_PCPT: usize = 4; // CPU package total
const K_PGTR: usize = 5; // GPU rail (absent on some SKUs)
const K_PANT: usize = 6; // Apple Neural Engine
const K_PSDC: usize = 7; // SoC domain
const K_PBUS: usize = 8; // bus power
// Temperature
const K_TP04: usize = 9; // E-cluster aggregate
const K_TP0A: usize = 10; // P-cluster aggregate
const K_TG0D: usize = 11; // GPU die D
const K_TW0P: usize = 12; // wireless module
const K_TA0P: usize = 13; // ambient air (present on Pro/Max; absent on base)
const K_TS0S: usize = 14; // SSD
const K_TB0P: usize = 15; // battery proximity (absent on desktops)
const K_TC0P: usize = 16; // CPU proximity
const K_TG0A: usize = 17; // GPU die A
const K_TG0P: usize = 18; // GPU proximity
const K_TM0A: usize = 19; // memory controller
// Fan
const K_FNUM: usize = 20; // number of fans (ui8)
const K_F0AC: usize = 21; // fan 0 actual RPM
const K_F0TG: usize = 22; // fan 0 target RPM
const K_F0MN: usize = 23; // fan 0 min RPM
const K_F0MX: usize = 24; // fan 0 max RPM
const K_F1AC: usize = 25; // fan 1 actual RPM
const K_F1TG: usize = 26; // fan 1 target RPM
const K_F1MN: usize = 27; // fan 1 min RPM
const K_F1MX: usize = 28; // fan 1 max RPM
// Voltage
const K_VP0R: usize = 29; // 12 V supply rail
const K_VD0R: usize = 30; // ~20 V intermediate DC rail (VD0R × ID0R ≈ PDTR)
const K_VC0C: usize = 31; // CPU core VDD
const K_VG0C: usize = 32; // GPU core VDD
// Current
const K_ID0R: usize = 33; // current on VD0R rail
const K_IC0C: usize = 34; // CPU core current
const K_IG0C: usize = 35; // GPU core current
// Battery (all absent / zero on non-portable hardware)
const K_B0AV: usize = 36; // voltage, mV
const K_B0DC: usize = 37; // current, mA  (negative = charging)
const K_B0AP: usize = 38; // average power, mW (negative = discharging)
const K_B0RM: usize = 39; // remaining capacity, mAh
const K_B0FC: usize = 40; // full-charge capacity, mAh

const KEY_COUNT: usize = 41;

/// Per-core CPU die temp sensors (`Tp01`..`Tp0f`).
const CPU_CORE_TEMP_KEYS: [&[u8; 4]; 15] = [
    b"Tp01", b"Tp02", b"Tp03", b"Tp04", b"Tp05", b"Tp06", b"Tp07", b"Tp08", b"Tp09", b"Tp0a",
    b"Tp0b", b"Tp0c", b"Tp0d", b"Tp0e", b"Tp0f",
];

/// GPU die temp sensors. Population varies by SKU.
// TODO(model-aware): widen this per-SKU using a calibrated map (M5 Pro/Max
// populate more Tg* keys; some chips also expose Te0T as a useful proxy).
const GPU_DIE_TEMP_KEYS: [&[u8; 4]; 5] = [b"Tg0a", b"Tg0b", b"Tg0c", b"Tg0d", b"Tg0f"];

/// Max of present readings, `f32::NAN` when all inputs are `None`.
fn max_present(reads: impl IntoIterator<Item = Option<f32>>) -> f32 {
    reads.into_iter().flatten().fold(f32::NAN, f32::max)
}

const ALL_KEYS: [&[u8; 4]; KEY_COUNT] = [
    // power
    b"PSTR", b"PPBR", b"PHPC", b"PDTR", b"PCPT", b"PGTR", b"PANT", b"PSDC", b"PBUS",
    // temperature
    b"Tp04", b"Tp0a", b"Tg0d", b"TW0P", b"TA0P", b"Ts0S", b"Tb0P", b"TC0P", b"Tg0a", b"TG0P",
    b"Tm0a", // fan
    b"FNum", b"F0Ac", b"F0Tg", b"F0Mn", b"F0Mx", b"F1Ac", b"F1Tg", b"F1Mn", b"F1Mx",
    // voltage
    b"VP0R", b"VD0R", b"VC0C", b"VG0C", // current
    b"ID0R", b"IC0C", b"IG0C", // battery
    b"B0AV", b"B0DC", b"B0AP", b"B0RM", b"B0FC",
];

// ── Error ─────────────────────────────────────────────────────────────────────

/// Errors returned by [`Smc::open`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SmcError {
    /// `IOServiceGetMatchingService` found no `AppleSMC` service.
    /// Usually means the process is running in an environment without SMC access
    /// (VM, Docker, or non-macOS).
    ServiceNotFound,
    /// `IOServiceOpen` failed with the given `kern_return_t` code.
    OpenFailed(i32),
}

impl std::fmt::Display for SmcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SmcError::ServiceNotFound => write!(f, "AppleSMC service not found"),
            SmcError::OpenFailed(kr) => write!(f, "IOServiceOpen failed: kern_return_t {kr}"),
        }
    }
}

impl std::error::Error for SmcError {}

/// Errors returned by [`Smc::write_f32`] and fan control methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SmcWriteError {
    /// The SMC key does not exist on this hardware or uses an unsupported type.
    KeyNotFound,
    /// `IOConnectCallStructMethod` failed with the given `kern_return_t` code.
    /// Common cause: writing without root privileges.
    IoError(i32),
}

impl std::fmt::Display for SmcWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SmcWriteError::KeyNotFound => write!(f, "SMC key not found on this hardware"),
            SmcWriteError::IoError(kr) => write!(f, "SMC write failed: kern_return_t {kr}"),
        }
    }
}

impl std::error::Error for SmcWriteError {}

// ── Smc ───────────────────────────────────────────────────────────────────────

/// Connection to the Apple SMC with pre-resolved key metadata.
///
/// Open once with [`Smc::open`]. Each [`Smc::snapshot`] issues one kernel
/// call per key (41 calls total). Reads of individual sub-groups are cheaper.
///
/// `Smc` is `Send` but not `Sync` — do not share across threads without
/// external synchronisation.
pub struct Smc {
    conn: IOConnectT,
    keys: [Option<ResolvedKey>; KEY_COUNT],
    // Arbitrary-key resolution cache for `read_f32`. Populated lazily; absent
    // keys are stored as `None` so the two-call resolve path isn't repeated.
    cache: RefCell<HashMap<u32, Option<ResolvedKey>>>,
    // Per-core CPU + GPU die temp sensors that responded at probe time. Skips
    // absent keys on the hot path — M4 base populates ~10 of 15 Tp* and 1 of 5 Tg*.
    cpu_temp_keys: Vec<ResolvedKey>,
    gpu_temp_keys: Vec<ResolvedKey>,
}

// SAFETY: IOConnectT is a Mach port that is safe to move across threads.
// RefCell<HashMap> is Send (but not Sync, which is fine — Smc is !Sync).
unsafe impl Send for Smc {}

impl Smc {
    /// Open the SMC and pre-resolve all keys. One-time cost (~82 kernel calls).
    pub fn open() -> Result<Self, SmcError> {
        let conn = unsafe {
            let matching = IOServiceMatching(c"AppleSMC".as_ptr() as *const u8);
            if matching.is_null() {
                return Err(SmcError::ServiceNotFound);
            }

            let service = IOServiceGetMatchingService(0, matching);
            if service == 0 {
                return Err(SmcError::ServiceNotFound);
            }

            let mut conn: IOConnectT = 0;
            let kr = IOServiceOpen(service, mach_task_self(), 0, &mut conn);
            IOObjectRelease(service);
            if kr != 0 {
                return Err(SmcError::OpenFailed(kr));
            }
            conn
        };

        let mut smc = Smc {
            conn,
            keys: [None; KEY_COUNT],
            cache: RefCell::new(HashMap::new()),
            cpu_temp_keys: Vec::new(),
            gpu_temp_keys: Vec::new(),
        };

        for (i, key_bytes) in ALL_KEYS.iter().enumerate() {
            smc.keys[i] = smc.resolve_key(key_bytes);
        }

        smc.cpu_temp_keys = CPU_CORE_TEMP_KEYS
            .iter()
            .filter_map(|k| smc.resolve_key(k))
            .collect();
        smc.gpu_temp_keys = GPU_DIE_TEMP_KEYS
            .iter()
            .filter_map(|k| smc.resolve_key(k))
            .collect();

        Ok(smc)
    }

    /// Read a pre-resolved key by index. One kernel call. Returns `0.0` if the
    /// key was absent at `open()` or the kernel call fails.
    #[inline]
    fn read(&self, idx: usize) -> f32 {
        let Some(rk) = self.keys[idx] else { return 0.0 };
        self.read_resolved(rk).unwrap_or(0.0)
    }

    /// Execute a single SMC read for an already-resolved key. Returns `None`
    /// when the kernel call fails (distinct from a key legitimately reading 0).
    #[inline]
    fn read_resolved(&self, rk: ResolvedKey) -> Option<f32> {
        unsafe {
            let mut req = mem::zeroed::<SmcKeyData>();
            req.key = rk.key;
            req.key_info.data_size = rk.data_size;
            req.data8 = SMC_CMD_READ_BYTES;

            let mut output = mem::MaybeUninit::<SmcKeyData>::uninit();
            let mut output_size = mem::size_of::<SmcKeyData>();
            let kr = IOConnectCallStructMethod(
                self.conn,
                KERNEL_INDEX_SMC,
                &req,
                mem::size_of::<SmcKeyData>(),
                output.as_mut_ptr(),
                &mut output_size,
            );
            if kr != 0 {
                return None;
            }
            Some(rk.decoder.decode(&output.assume_init_ref().bytes))
        }
    }

    /// Read an arbitrary SMC key by its four-CC code.
    ///
    /// The first call for a given key costs two kernel calls (resolve + read);
    /// subsequent calls for the same key hit an in-memory cache and cost one.
    /// Absent keys are cached too — they don't re-trigger resolve on retry.
    ///
    /// Supported SMC data types: `flt ` (IEEE 754 float), `sp78` (signed 8.8
    /// fixed-point), `ioft` (unsigned 16.16 fixed-point), `ui8`, `ui16`, `si16`,
    /// `ui32`. Keys using other types return `None`.
    ///
    /// Returns `None` if the key is absent on this hardware or uses an unsupported type.
    pub fn read_f32(&self, key: &[u8; 4]) -> Option<f32> {
        let key_u32 = four_cc(key);
        // Hot path: cache hit takes only an immutable borrow.
        if let Some(&entry) = self.cache.borrow().get(&key_u32) {
            return entry.and_then(|rk| self.read_resolved(rk));
        }
        let resolved = self.resolve_key(key);
        self.cache.borrow_mut().insert(key_u32, resolved);
        resolved.and_then(|rk| self.read_resolved(rk))
    }

    /// Write a resolved key. One kernel call. Returns `Err(kern_return_t)` on failure.
    fn write_resolved(&self, rk: ResolvedKey, value: f32) -> Result<(), i32> {
        unsafe {
            let mut req = mem::zeroed::<SmcKeyData>();
            req.key = rk.key;
            req.key_info.data_size = rk.data_size;
            req.key_info.data_type = rk.data_type;
            req.data8 = SMC_CMD_WRITE_BYTES;
            rk.decoder.encode(value, &mut req.bytes);

            let mut output = mem::MaybeUninit::<SmcKeyData>::uninit();
            let mut output_size = mem::size_of::<SmcKeyData>();
            let kr = IOConnectCallStructMethod(
                self.conn,
                KERNEL_INDEX_SMC,
                &req,
                mem::size_of::<SmcKeyData>(),
                output.as_mut_ptr(),
                &mut output_size,
            );
            if kr != 0 { Err(kr) } else { Ok(()) }
        }
    }

    /// Write a value to an arbitrary SMC key by its four-CC code.
    ///
    /// The value is encoded according to the key's data type (discovered
    /// automatically). Requires root privileges for most keys.
    ///
    /// Returns `Err` if the key is absent or the kernel rejects the write.
    pub fn write_f32(&self, key: &[u8; 4], value: f32) -> Result<(), SmcWriteError> {
        let key_u32 = four_cc(key);
        if let Some(&entry) = self.cache.borrow().get(&key_u32) {
            return match entry {
                Some(rk) => self.write_resolved(rk, value).map_err(SmcWriteError::IoError),
                None => Err(SmcWriteError::KeyNotFound),
            };
        }
        let resolved = self.resolve_key(key);
        self.cache.borrow_mut().insert(key_u32, resolved);
        match resolved {
            Some(rk) => self.write_resolved(rk, value).map_err(SmcWriteError::IoError),
            None => Err(SmcWriteError::KeyNotFound),
        }
    }

    // ── Key enumeration ──────────────────────────────────────────────────────

    /// Total number of SMC keys on this machine (reads the `#KEY` meta-key).
    pub fn key_count(&self) -> u32 {
        // #KEY is a ui32 key that stores the total key count.
        self.read_f32(b"#KEY").unwrap_or(0.0) as u32
    }

    /// Return the four-CC key name at the given index, or `None` on error.
    ///
    /// Iterate from `0..key_count()` to enumerate every available SMC key.
    pub fn key_at_index(&self, index: u32) -> Option<[u8; 4]> {
        unsafe {
            let mut req = mem::zeroed::<SmcKeyData>();
            req.data32 = index;
            req.data8 = SMC_CMD_GET_KEY_FROM_INDEX;

            let mut output = mem::MaybeUninit::<SmcKeyData>::uninit();
            let mut output_size = mem::size_of::<SmcKeyData>();
            let kr = IOConnectCallStructMethod(
                self.conn,
                KERNEL_INDEX_SMC,
                &req,
                mem::size_of::<SmcKeyData>(),
                output.as_mut_ptr(),
                &mut output_size,
            );
            if kr != 0 {
                return None;
            }
            let key_u32 = output.assume_init_ref().key;
            if key_u32 == 0 {
                return None;
            }
            Some(key_u32.to_be_bytes())
        }
    }

    /// Return all SMC key names present on this machine.
    ///
    /// Iterates every key by index — costs `key_count()` kernel calls.
    pub fn all_keys(&self) -> Vec<[u8; 4]> {
        let count = self.key_count();
        (0..count).filter_map(|i| self.key_at_index(i)).collect()
    }

    /// Return raw key metadata: `(data_size, data_type_fourcc, data_attributes)`.
    pub fn key_info(&self, key: &[u8; 4]) -> Option<(u32, [u8; 4], u8)> {
        let key_u32 = four_cc(key);
        unsafe {
            let mut req = mem::zeroed::<SmcKeyData>();
            req.key = key_u32;
            req.data8 = SMC_CMD_READ_KEYINFO;

            let mut output = mem::MaybeUninit::<SmcKeyData>::uninit();
            let mut output_size = mem::size_of::<SmcKeyData>();
            let kr = IOConnectCallStructMethod(
                self.conn,
                KERNEL_INDEX_SMC,
                &req,
                mem::size_of::<SmcKeyData>(),
                output.as_mut_ptr(),
                &mut output_size,
            );
            if kr != 0 {
                return None;
            }
            let info = output.assume_init_ref().key_info;
            if info.data_size == 0 {
                return None;
            }
            Some((info.data_size, info.data_type.to_be_bytes(), info.data_attributes))
        }
    }

    // ── Grouped reads ────────────────────────────────────────────────────────

    /// Read just the full-system power rail (`PSTR`). One kernel call —
    /// prefer this over [`Self::read_power`] when only the system total is needed.
    pub fn read_system_power(&self) -> f32 {
        self.read(K_PSTR)
    }

    /// Returns `(cpu_c, gpu_c)`, each the max of populated per-core / per-die
    /// sensors, or `f32::NAN` when no key in the group resolves on this SKU.
    /// Rail-off sentinels (negative on Apple Silicon) pass through verbatim
    /// — caller is responsible for filtering implausible values.
    pub fn read_cpu_gpu_temps(&self) -> (f32, f32) {
        let cpu = max_present(self.cpu_temp_keys.iter().map(|k| self.read_resolved(*k)));
        let gpu = max_present(self.gpu_temp_keys.iter().map(|k| self.read_resolved(*k)));
        (cpu, gpu)
    }

    /// Read all power-rail sensors (9 keys).
    pub fn read_power(&mut self) -> PowerReading {
        PowerReading {
            system: self.read(K_PSTR),
            package: self.read(K_PPBR),
            hpc_cluster: self.read(K_PHPC),
            dc_rail: self.read(K_PDTR),
            cpu: self.read(K_PCPT),
            gpu_rail: self.read(K_PGTR),
            ane: self.read(K_PANT),
            soc: self.read(K_PSDC),
            bus: self.read(K_PBUS),
        }
    }

    /// Read all temperature sensors (11 keys).
    pub fn read_thermals(&mut self) -> ThermalReading {
        ThermalReading {
            cpu_efficiency: self.read(K_TP04),
            cpu_performance: self.read(K_TP0A),
            gpu: self.read(K_TG0D),
            wireless: self.read(K_TW0P),
            ambient: self.read(K_TA0P),
            ssd: self.read(K_TS0S),
            battery: self.read(K_TB0P),
            cpu_proximity: self.read(K_TC0P),
            gpu_a: self.read(K_TG0A),
            gpu_proximity: self.read(K_TG0P),
            memory: self.read(K_TM0A),
        }
    }

    /// Read fan state for all fans present (up to 2). Uses `FNum` to determine count.
    pub fn read_fans(&mut self) -> FanReading {
        let raw = self.read(K_FNUM);
        let count = if raw.is_finite() {
            raw.clamp(0.0, 2.0) as u8
        } else {
            0
        };
        let fan0 = if count >= 1 {
            Some(FanState {
                rpm: self.read(K_F0AC),
                target_rpm: self.read(K_F0TG),
                min_rpm: self.read(K_F0MN),
                max_rpm: self.read(K_F0MX),
            })
        } else {
            None
        };
        let fan1 = if count >= 2 {
            Some(FanState {
                rpm: self.read(K_F1AC),
                target_rpm: self.read(K_F1TG),
                min_rpm: self.read(K_F1MN),
                max_rpm: self.read(K_F1MX),
            })
        } else {
            None
        };
        FanReading {
            count,
            fans: [fan0, fan1],
        }
    }

    /// Read voltage rails (4 keys).
    pub fn read_voltages(&mut self) -> VoltageReading {
        VoltageReading {
            rail_12v: self.read(K_VP0R),
            dc_rail: self.read(K_VD0R),
            cpu_core: self.read(K_VC0C),
            gpu_core: self.read(K_VG0C),
        }
    }

    /// Read current sensors (3 keys).
    pub fn read_currents(&mut self) -> CurrentReading {
        CurrentReading {
            dc_rail: self.read(K_ID0R),
            cpu_core: self.read(K_IC0C),
            gpu_core: self.read(K_IG0C),
        }
    }

    /// Read battery sensors (5 keys). All fields are `0.0` on desktop hardware.
    ///
    /// Check [`BatteryReading::is_present`] before using the values.
    pub fn read_battery(&mut self) -> BatteryReading {
        BatteryReading {
            voltage_mv: self.read(K_B0AV),
            current_ma: self.read(K_B0DC),
            power_mw: self.read(K_B0AP),
            remaining_mah: self.read(K_B0RM),
            full_charge_mah: self.read(K_B0FC),
        }
    }

    /// Full snapshot: 41 kernel calls.
    pub fn snapshot(&mut self) -> SystemSnapshot {
        SystemSnapshot {
            power: self.read_power(),
            thermal: self.read_thermals(),
            fans: self.read_fans(),
            voltage: self.read_voltages(),
            current: self.read_currents(),
            battery: self.read_battery(),
        }
    }

    /// Read per-core CPU temperatures from the `Tp01..Tp0f` sensors that
    /// resolved at `Smc::open`. Returns exactly one entry per resolved key,
    /// in ascending key order. Length depends on SKU.
    pub fn read_per_core_cpu_temps(&self) -> Vec<f32> {
        self.cpu_temp_keys
            .iter()
            .map(|k| self.read_resolved(*k).unwrap_or(0.0))
            .collect()
    }

    // ── Fan control ─────────────────────────────────────────────────────────

    /// Number of fans present (0–2). Zero on fanless hardware.
    pub fn fan_count(&self) -> u8 {
        let raw = self.read(K_FNUM);
        if raw.is_finite() { raw.clamp(0.0, 2.0) as u8 } else { 0 }
    }

    /// Hardware maximum RPM for the given fan index (0 or 1).
    pub fn fan_max_rpm(&self, fan: u8) -> f32 {
        match fan {
            0 => self.read(K_F0MX),
            1 => self.read(K_F1MX),
            _ => 0.0,
        }
    }

    /// Force all fans to their hardware maximum RPM.
    ///
    /// Sets each fan to forced mode (`F0md`/`F1md` = 1) then writes the
    /// max RPM as the target. **Requires root privileges.**
    ///
    /// On any failure mid-sequence, best-effort restores automatic control so
    /// a fan is never left stranded in forced mode. (A `SIGKILL` or crash while
    /// forced cannot be cleaned up from userspace — the fan holds its forced
    /// state until the OS thermal manager reclaims it.)
    pub fn set_fans_max(&self) -> Result<(), SmcWriteError> {
        const MODE: [&[u8; 4]; 2] = [b"F0md", b"F1md"];
        const TARGET: [&[u8; 4]; 2] = [b"F0Tg", b"F1Tg"];
        let count = self.fan_count() as usize;
        for i in 0..count {
            let max_rpm = self.fan_max_rpm(i as u8);
            if let Err(e) = self
                .write_f32(MODE[i], 1.0)
                .and_then(|()| self.write_f32(TARGET[i], max_rpm))
            {
                // Don't leave a half-forced fan behind on partial failure.
                let _ = self.set_fans_auto();
                return Err(e);
            }
        }
        Ok(())
    }

    /// Restore all fans to automatic (OS-controlled) mode.
    ///
    /// Clears forced mode on each fan. **Requires root privileges.**
    pub fn set_fans_auto(&self) -> Result<(), SmcWriteError> {
        const MODE: [&[u8; 4]; 2] = [b"F0md", b"F1md"];
        let count = self.fan_count() as usize;
        for &mode in &MODE[..count] {
            self.write_f32(mode, 0.0)?;
        }
        Ok(())
    }

    // ── Internal ─────────────────────────────────────────────────────────────

    fn resolve_key(&self, key: &[u8; 4]) -> Option<ResolvedKey> {
        let key_u32 = four_cc(key);
        unsafe {
            let mut req = mem::zeroed::<SmcKeyData>();
            req.key = key_u32;
            req.data8 = SMC_CMD_READ_KEYINFO;

            let mut output = mem::MaybeUninit::<SmcKeyData>::uninit();
            let mut output_size = mem::size_of::<SmcKeyData>();
            let kr = IOConnectCallStructMethod(
                self.conn,
                KERNEL_INDEX_SMC,
                &req,
                mem::size_of::<SmcKeyData>(),
                output.as_mut_ptr(),
                &mut output_size,
            );
            if kr != 0 {
                return None;
            }

            let info = output.assume_init_ref().key_info;
            if info.data_size == 0 {
                return None;
            }

            let decoder = match &info.data_type.to_be_bytes() {
                b"flt " => Decoder::Flt,
                b"sp78" => Decoder::Sp78,
                b"ioft" => Decoder::Ioft,
                b"ui8 " => Decoder::Ui8,
                b"ui16" => Decoder::Ui16,
                b"si16" => Decoder::Si16,
                b"ui32" => Decoder::Ui32,
                _ => return None,
            };

            Some(ResolvedKey {
                key: key_u32,
                data_size: info.data_size,
                data_type: info.data_type,
                decoder,
            })
        }
    }
}

impl Drop for Smc {
    fn drop(&mut self) {
        unsafe {
            IOServiceClose(self.conn);
        }
    }
}

// ── Reading structs ───────────────────────────────────────────────────────────

/// Power rail readings in watts. Zero for any rail absent on this SKU.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[non_exhaustive]
pub struct PowerReading {
    /// `PSTR` — total system power draw from the supply rail.
    pub system: f32,
    /// `PPBR` — SoC package base rail.
    pub package: f32,
    /// `PHPC` — P-core (high-performance) cluster. Not GPU power.
    pub hpc_cluster: f32,
    /// `PDTR` — intermediate DC rail power (≈ VD0R × ID0R). Not DRAM-specific.
    pub dc_rail: f32,
    /// `PCPT` — CPU package total.
    pub cpu: f32,
    /// `PGTR` — GPU power rail. Zero if absent (e.g. some base-chip SKUs).
    pub gpu_rail: f32,
    /// `PANT` — Apple Neural Engine.
    pub ane: f32,
    /// `PSDC` — SoC power domain.
    pub soc: f32,
    /// `PBUS` — bus power.
    pub bus: f32,
}

/// Temperature readings in degrees Celsius. Zero for sensors absent on this hardware.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[non_exhaustive]
pub struct ThermalReading {
    /// `Tp04` — E-cluster aggregate.
    pub cpu_efficiency: f32,
    /// `Tp0a` — P-cluster aggregate.
    pub cpu_performance: f32,
    /// `Tg0d` — GPU die D.
    pub gpu: f32,
    /// `TW0P` — wireless module.
    pub wireless: f32,
    /// `TA0P` — ambient air. Present on Pro/Max; may be zero on base chips.
    pub ambient: f32,
    /// `Ts0S` — SSD controller.
    pub ssd: f32,
    /// `Tb0P` — battery proximity. Zero on desktops.
    pub battery: f32,
    /// `TC0P` — CPU proximity.
    pub cpu_proximity: f32,
    /// `Tg0a` — GPU die A.
    pub gpu_a: f32,
    /// `TG0P` — GPU proximity.
    pub gpu_proximity: f32,
    /// `Tm0a` — memory controller.
    pub memory: f32,
}

/// State of one fan.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[non_exhaustive]
pub struct FanState {
    /// Actual RPM.
    pub rpm: f32,
    /// Target RPM (set by thermal manager).
    pub target_rpm: f32,
    /// Minimum RPM (hardware limit).
    pub min_rpm: f32,
    /// Maximum RPM (hardware limit).
    pub max_rpm: f32,
}

impl FanState {
    /// Duty cycle as a fraction in `[0, 1]`, derived from actual vs max RPM.
    pub fn duty_cycle(&self) -> f32 {
        if self.max_rpm > 0.0 {
            (self.rpm / self.max_rpm).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

/// All fans on this machine. Supports up to two fans (Mac Studio, Mac Pro).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[non_exhaustive]
pub struct FanReading {
    /// Number of fans reported by `FNum`. Zero on fanless hardware (MacBook Air).
    pub count: u8,
    /// Fan 0 and fan 1 state. Index corresponds to `F0*` / `F1*` SMC keys.
    /// `None` when the fan does not exist.
    pub fans: [Option<FanState>; 2],
}

/// Voltage readings. Zero for rails absent on this hardware.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[non_exhaustive]
pub struct VoltageReading {
    /// `VP0R` — 12 V supply rail.
    pub rail_12v: f32,
    /// `VD0R` — intermediate DC rail (~20 V on Mac Studio). Not DRAM voltage.
    /// This is the supply side of the main SoC power delivery network.
    pub dc_rail: f32,
    /// `VC0C` — CPU core VDD.
    pub cpu_core: f32,
    /// `VG0C` — GPU core VDD.
    pub gpu_core: f32,
}

/// Current readings in amperes. Zero for absent sensors.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[non_exhaustive]
pub struct CurrentReading {
    /// `ID0R` — current on the VD0R rail (pairs with [`VoltageReading::dc_rail`]).
    pub dc_rail: f32,
    /// `IC0C` — CPU core current.
    pub cpu_core: f32,
    /// `IG0C` — GPU core current.
    pub gpu_core: f32,
}

/// Battery readings. **All fields are `0.0` on desktop hardware.**
///
/// Check [`BatteryReading::is_present`] before using values.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[non_exhaustive]
pub struct BatteryReading {
    /// `B0AV` — battery voltage in millivolts.
    pub voltage_mv: f32,
    /// `B0DC` — current in milliamps. Negative = charging.
    pub current_ma: f32,
    /// `B0AP` — average power in milliwatts. Negative = discharging.
    pub power_mw: f32,
    /// `B0RM` — remaining charge in milliamp-hours.
    pub remaining_mah: f32,
    /// `B0FC` — full-charge capacity in milliamp-hours.
    pub full_charge_mah: f32,
}

impl BatteryReading {
    /// `true` if a battery is present (full-charge capacity > 0).
    pub fn is_present(&self) -> bool {
        self.full_charge_mah > 0.0
    }

    /// State of charge as a percentage in `[0, 100]`. Zero on desktops.
    pub fn state_of_charge(&self) -> f32 {
        if self.full_charge_mah > 0.0 {
            (self.remaining_mah / self.full_charge_mah * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        }
    }

    /// `true` if the battery is currently charging (`current_ma < 0`).
    pub fn is_charging(&self) -> bool {
        self.current_ma < 0.0
    }

    /// Instantaneous battery draw in watts (positive = discharging, negative = charging).
    ///
    /// Derived from `B0AP` (mW) divided by 1000. Zero on desktops.
    pub fn power_watts(&self) -> f32 {
        -self.power_mw / 1000.0
    }

    /// Hours until the battery is empty at the current discharge rate.
    ///
    /// Returns `None` if the battery is charging, idle, or absent.
    pub fn time_to_empty_hours(&self) -> Option<f32> {
        if !self.is_present() || self.current_ma <= 0.0 {
            return None;
        }
        Some(self.remaining_mah / self.current_ma)
    }

    /// Hours until the battery is full at the current charge rate.
    ///
    /// Returns `None` if the battery is discharging, idle, or absent.
    pub fn time_to_full_hours(&self) -> Option<f32> {
        if !self.is_present() || self.current_ma >= 0.0 {
            return None;
        }
        let remaining_to_full = self.full_charge_mah - self.remaining_mah;
        if remaining_to_full <= 0.0 {
            return Some(0.0);
        }
        Some(remaining_to_full / (-self.current_ma))
    }
}

/// Complete system snapshot from a single [`Smc::snapshot`] call (41 kernel calls).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[non_exhaustive]
pub struct SystemSnapshot {
    /// Power rail readings in watts.
    pub power: PowerReading,
    /// Temperature sensors in degrees Celsius.
    pub thermal: ThermalReading,
    /// Fan state and RPM.
    pub fans: FanReading,
    /// Voltage rails in volts.
    pub voltage: VoltageReading,
    /// Current sensors in amperes.
    pub current: CurrentReading,
    /// Battery state. All zeros on desktop hardware — check [`BatteryReading::is_present`].
    pub battery: BatteryReading,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── BatteryReading ────────────────────────────────────────────────────────

    #[test]
    fn battery_not_present_when_zero_capacity() {
        let b = BatteryReading::default();
        assert!(!b.is_present());
        assert_eq!(b.state_of_charge(), 0.0);
        assert!(!b.is_charging());
    }

    #[test]
    fn battery_present_when_capacity_nonzero() {
        let b = BatteryReading {
            full_charge_mah: 5000.0,
            remaining_mah: 2500.0,
            ..Default::default()
        };
        assert!(b.is_present());
        assert!((b.state_of_charge() - 50.0).abs() < 0.01);
    }

    #[test]
    fn battery_charging_when_current_negative() {
        let b = BatteryReading {
            current_ma: -500.0,
            full_charge_mah: 5000.0,
            ..Default::default()
        };
        assert!(b.is_charging());
    }

    #[test]
    fn battery_power_watts_from_milliwatts() {
        let b = BatteryReading {
            power_mw: -5000.0, // -5000 mW = 5 W discharging
            full_charge_mah: 5000.0,
            ..Default::default()
        };
        assert!((b.power_watts() - 5.0).abs() < 1e-6);
    }

    #[test]
    fn battery_time_to_empty_only_when_discharging() {
        let discharging = BatteryReading {
            current_ma: 500.0,
            remaining_mah: 1000.0,
            full_charge_mah: 5000.0,
            ..Default::default()
        };
        assert!((discharging.time_to_empty_hours().unwrap() - 2.0).abs() < 1e-6);

        let charging = BatteryReading {
            current_ma: -500.0,
            remaining_mah: 1000.0,
            full_charge_mah: 5000.0,
            ..Default::default()
        };
        assert!(charging.time_to_empty_hours().is_none());

        let absent = BatteryReading::default();
        assert!(absent.time_to_empty_hours().is_none());
    }

    #[test]
    fn battery_time_to_full_only_when_charging() {
        let charging = BatteryReading {
            current_ma: -500.0,
            remaining_mah: 1000.0,
            full_charge_mah: 5000.0,
            ..Default::default()
        };
        assert!((charging.time_to_full_hours().unwrap() - 8.0).abs() < 1e-6);

        let discharging = BatteryReading {
            current_ma: 500.0,
            remaining_mah: 1000.0,
            full_charge_mah: 5000.0,
            ..Default::default()
        };
        assert!(discharging.time_to_full_hours().is_none());
    }

    #[test]
    fn battery_soc_clamped_to_100() {
        let b = BatteryReading {
            full_charge_mah: 100.0,
            remaining_mah: 200.0,
            ..Default::default()
        };
        assert_eq!(b.state_of_charge(), 100.0);
    }

    // ── FanState ─────────────────────────────────────────────────────────────

    #[test]
    fn fan_duty_cycle_proportional() {
        let f = FanState {
            rpm: 3000.0,
            max_rpm: 6000.0,
            ..Default::default()
        };
        assert!((f.duty_cycle() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn fan_duty_cycle_zero_when_max_zero() {
        let f = FanState::default(); // max_rpm = 0
        assert_eq!(f.duty_cycle(), 0.0);
    }

    // ── SmcError ─────────────────────────────────────────────────────────────

    #[test]
    fn smc_error_display() {
        assert_eq!(
            SmcError::ServiceNotFound.to_string(),
            "AppleSMC service not found"
        );
        assert!(
            SmcError::OpenFailed(-536870212)
                .to_string()
                .contains("-536870212")
        );
    }

    #[test]
    fn smc_error_is_copy() {
        let e = SmcError::ServiceNotFound;
        let _e2 = e; // Copy
        let _e3 = e; // still usable
    }

    // ── max_present ──────────────────────────────────────────────────────────

    #[test]
    fn max_present_all_none_is_nan() {
        let v: Vec<Option<f32>> = vec![None, None, None];
        assert!(max_present(v).is_nan());
    }

    #[test]
    fn max_present_picks_max_of_some() {
        let v = vec![Some(10.0_f32), None, Some(42.5), Some(-4.5)];
        assert!((max_present(v) - 42.5).abs() < 1e-6);
    }

    #[test]
    fn max_present_single_some_passes_through() {
        let v = vec![None, Some(-4.5_f32), None];
        assert!((max_present(v) - (-4.5)).abs() < 1e-6);
    }
}
