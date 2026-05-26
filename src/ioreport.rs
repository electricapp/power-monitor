//! IOReport FFI for per-component energy sampling on Apple Silicon.
//!
//! Power = delta_energy_mJ / delta_time_ms  (mJ/ms = W).
//! Channel names confirmed by `cargo run --bin probe` on M5.
//!
//! Library: /usr/lib/libIOReport.dylib  (confirmed via powermetrics otool -L)
//!
//! # Allocation profile
//!
//! After a warm-up [`MultiGroupSampler::sample`] call, subsequent calls perform
//! **zero heap allocations**. Channel names, state names, and units are cached
//! in a persistent scratch buffer owned by the sampler. Only channel tick /
//! energy values are overwritten per-sample.

use std::ffi::c_void;
use std::time::Instant;

// ── CoreFoundation types ─────────────────────────────────────────────────────

type CFTypeRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFMutableDictRef = *mut c_void;
type CFArrayRef = *const c_void;
type CFStringRef = *const c_void;
type CFIndex = isize;

const CF_UTF8: u32 = 0x0800_0100;

unsafe extern "C" {
    /// Increment the retain count of a CF object. Pairs with [`CFRelease`].
    pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    /// Decrement the retain count; frees the object when it reaches zero.
    pub fn CFRelease(cf: CFTypeRef);
    fn CFStringCreateWithCString(alloc: CFTypeRef, s: *const u8, enc: u32) -> CFStringRef;
    fn CFStringGetCString(s: CFStringRef, buf: *mut u8, cap: CFIndex, enc: u32) -> bool;
    /// Look up a value in a `CFDictionary` by key.
    pub fn CFDictionaryGetValue(dict: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;
    /// Number of key-value pairs in a `CFDictionary`.
    pub fn CFDictionaryGetCount(dict: CFDictionaryRef) -> CFIndex;
    /// Create a mutable copy of a `CFDictionary`. Caller must CFRelease.
    pub fn CFDictionaryCreateMutableCopy(
        alloc: CFTypeRef,
        capacity: CFIndex,
        the_dict: CFDictionaryRef,
    ) -> CFMutableDictRef;
    /// Number of elements in a `CFArray`.
    pub fn CFArrayGetCount(arr: CFArrayRef) -> CFIndex;
    /// Element at `idx` in a `CFArray` (interior pointer — do NOT CFRelease).
    pub fn CFArrayGetValueAtIndex(arr: CFArrayRef, idx: CFIndex) -> CFTypeRef;
    /// Raw byte pointer into a `CFData` object. Valid until the CFData is released.
    pub fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
    /// Length in bytes of a `CFData` object.
    pub fn CFDataGetLength(data: CFTypeRef) -> CFIndex;
}

// ── IOReport symbols (/usr/lib/libIOReport.dylib) ───────────────────────────

unsafe extern "C" {
    /// Copy channels matching `group`/`subgroup` from `/usr/lib/libIOReport.dylib`.
    /// Pass `null` for `subgroup` to get all subgroups within a group.
    pub fn IOReportCopyChannelsInGroup(
        group: CFStringRef,
        subgroup: CFStringRef,
        a: u64,
        b: u64,
        c: u64,
    ) -> CFMutableDictRef;

    /// Subscribe to a set of channels. Returns an opaque subscription handle.
    pub fn IOReportCreateSubscription(
        alloc: CFTypeRef,
        desired: CFMutableDictRef,
        subbed: *mut CFMutableDictRef,
        channel_id: u64,
        options: CFTypeRef,
    ) -> *mut c_void;

    /// Take a point-in-time sample of all subscribed channels.
    pub fn IOReportCreateSamples(
        sub: *mut c_void,
        subbed: CFMutableDictRef,
        opts: CFTypeRef,
    ) -> CFDictionaryRef;

    /// Compute the delta between two samples. Use for power/utilisation calculations.
    pub fn IOReportCreateSamplesDelta(
        prev: CFDictionaryRef,
        current: CFDictionaryRef,
        opts: CFTypeRef,
    ) -> CFDictionaryRef;

    /// Channel name (e.g. `"ECPU"`, `"GPU"`) — interior pointer, do NOT CFRelease.
    pub fn IOReportChannelGetChannelName(channel: CFDictionaryRef) -> CFStringRef;
    /// Raw integer value for a simple (format = 0) channel; writes format code to `format`.
    pub fn IOReportSimpleGetIntegerValue(channel: CFDictionaryRef, format: *mut i32) -> i64;

    /// Merge `src` channels into `dst` in-place. Caller must CFRelease both.
    pub fn IOReportMergeChannels(dst: CFMutableDictRef, src: CFMutableDictRef, opts: CFTypeRef);

    /// Group name for a channel (e.g. `"Energy Model"`) — interior pointer, do NOT CFRelease.
    pub fn IOReportChannelGetGroup(channel: CFDictionaryRef) -> CFStringRef;
    /// Subgroup name — interior pointer, do NOT CFRelease.
    pub fn IOReportChannelGetSubGroup(channel: CFDictionaryRef) -> CFStringRef;

    /// Number of P-states in a state-format channel (format = 2).
    pub fn IOReportStateGetCount(channel: CFDictionaryRef) -> i32;
    /// State name at index `idx` — interior pointer, do NOT CFRelease.
    pub fn IOReportStateGetNameForIndex(channel: CFDictionaryRef, idx: i32) -> CFStringRef;
    /// Residency ticks accumulated in state `idx` over the sample window.
    pub fn IOReportStateGetResidency(channel: CFDictionaryRef, idx: i32) -> u64;
}

unsafe extern "C" {
    /// Returns a `u64` unit bitmask (NOT a string pointer).
    pub fn IOReportChannelGetUnit(channel: CFDictionaryRef) -> u64;

    /// Scale a raw integer value from a channel to SI units using the channel's
    /// embedded unit metadata.
    pub fn IOReportScaleValue(channel: CFDictionaryRef, value: i64) -> f64;

    /// Duty cycle for state `idx` as a fraction in `[0.0, 1.0]`.
    pub fn IOReportStateGetDutyCycle(channel: CFDictionaryRef, idx: i32) -> f64;
}

// ── CFString helpers ─────────────────────────────────────────────────────────

/// RAII wrapper for an owned Core Foundation object.
/// Calls `CFRelease` on drop — safe even across early returns and panics.
struct OwnedCf(CFTypeRef);

impl OwnedCf {
    /// Wrap a CF pointer; returns `None` if null.
    fn new(cf: CFTypeRef) -> Option<Self> {
        if cf.is_null() {
            None
        } else {
            Some(OwnedCf(cf))
        }
    }
    fn as_ptr(&self) -> CFTypeRef {
        self.0
    }
}

impl Drop for OwnedCf {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) }
    }
}

// SAFETY: CoreFoundation objects created on the main thread are thread-safe to
// release once no other references exist. The sampler owns them exclusively.
unsafe impl Send for OwnedCf {}

/// Allocate a CFString from a nul-terminated byte slice, wrapped in [`OwnedCf`].
/// Input must contain a trailing `\0` (use `c"..."` literals).
fn cfstr_from_cstr(bytes: &[u8]) -> Option<OwnedCf> {
    debug_assert!(
        bytes.last() == Some(&0),
        "cfstr_from_cstr needs NUL terminator"
    );
    let cf = unsafe { CFStringCreateWithCString(std::ptr::null(), bytes.as_ptr(), CF_UTF8) };
    OwnedCf::new(cf as CFTypeRef)
}

/// Copy a CFString's UTF-8 bytes into `buf`, returning the used slice.
/// Returns an empty slice on null input or encoding failure.
#[inline]
fn cf_to_buf(s: CFStringRef, buf: &mut [u8]) -> &[u8] {
    if s.is_null() || buf.is_empty() {
        return &[];
    }
    unsafe {
        buf[0] = 0;
        if !CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as CFIndex, CF_UTF8) {
            return &[];
        }
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    &buf[..end]
}

/// Decode a CFString into an owned `String`, copying once.
#[inline]
fn cfstr_to_string(s: CFStringRef) -> String {
    let mut buf = [0u8; 128];
    let bytes = cf_to_buf(s, &mut buf);
    std::str::from_utf8(bytes).unwrap_or("").to_owned()
}

// ── Public enums ─────────────────────────────────────────────────────────────

/// Unit label for an Energy Model channel. Always one of three fixed values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum EnergyUnit {
    /// Millijoules — `"mJ"`. Default when unspecified.
    #[default]
    MilliJoule,
    /// Microjoules — `"uJ"`.
    MicroJoule,
    /// Nanojoules — `"nJ"`.
    NanoJoule,
}

impl EnergyUnit {
    /// Convert a raw integer delta reported by IOReport to joules.
    #[inline]
    pub fn to_joules(self, raw: i64) -> f64 {
        let r = raw as f64;
        match self {
            EnergyUnit::MilliJoule => r / 1e3,
            EnergyUnit::MicroJoule => r / 1e6,
            EnergyUnit::NanoJoule => r / 1e9,
        }
    }

    /// Parse a unit label, returning `None` on unknown input.
    ///
    /// Use this when you want to distinguish "unit was absent / corrupted"
    /// from "unit was valid mJ". For internal parsing that silently defaults
    /// to `MilliJoule`, use [`EnergyUnit::from_bytes`].
    #[inline]
    pub fn parse(b: &[u8]) -> Option<Self> {
        match b {
            b"mJ" => Some(EnergyUnit::MilliJoule),
            b"uJ" => Some(EnergyUnit::MicroJoule),
            b"nJ" => Some(EnergyUnit::NanoJoule),
            _ => None,
        }
    }

    /// Lenient parser: unknown input silently defaults to [`EnergyUnit::MilliJoule`].
    ///
    /// Used internally on the hot path where a sane default is preferable
    /// to propagating parse errors.
    #[inline]
    pub fn from_bytes(b: &[u8]) -> Self {
        Self::parse(b).unwrap_or(EnergyUnit::MilliJoule)
    }

    /// Short label (`"mJ"`, `"uJ"`, `"nJ"`).
    pub fn label(self) -> &'static str {
        match self {
            EnergyUnit::MilliJoule => "mJ",
            EnergyUnit::MicroJoule => "uJ",
            EnergyUnit::NanoJoule => "nJ",
        }
    }
}

/// IOReport channel group. Matches the three groups [`MultiGroupSampler`] subscribes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ChannelGroup {
    /// `"Energy Model"` — per-component energy counters.
    EnergyModel,
    /// `"CPU Stats"` — CPU core P-state residency.
    CpuStats,
    /// `"GPU Stats"` — GPU P-state residency.
    GpuStats,
    /// Any group not explicitly handled.
    #[default]
    Other,
}

impl ChannelGroup {
    #[inline]
    fn from_bytes(b: &[u8]) -> Self {
        match b {
            b"Energy Model" => ChannelGroup::EnergyModel,
            b"CPU Stats" => ChannelGroup::CpuStats,
            b"GPU Stats" => ChannelGroup::GpuStats,
            _ => ChannelGroup::Other,
        }
    }

    /// Static string label — useful for logging without allocation.
    pub fn label(self) -> &'static str {
        match self {
            ChannelGroup::EnergyModel => "Energy Model",
            ChannelGroup::CpuStats => "CPU Stats",
            ChannelGroup::GpuStats => "GPU Stats",
            ChannelGroup::Other => "Other",
        }
    }
}

const CHANNELS_KEY: &[u8] = b"IOReportChannels\0";
const UNIT_KEY: &[u8] = b"IOReportChannelUnit\0";

// ── EnergyReading ─────────────────────────────────────────────────────────────

/// Per-component power in watts from IOReport Energy Model delta.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnergyReading {
    pub ecpu: f32,
    pub pcpu: f32,
    pub gpu: f32,
    pub ane: f32,
    pub dram: f32,
    pub amcc: f32,
    pub dcs: f32,
    pub isp: f32,
    pub ave: f32,
    pub disp: f32,
    pub interval_ms: f32,
}

impl EnergyReading {
    pub fn cpu_total(&self) -> f32 {
        self.ecpu + self.pcpu
    }
}

// ── EnergyAccumulator ─────────────────────────────────────────────────────────

/// Integrates a stream of [`EnergyReading`]s into cumulative energy (joules)
/// and sample-weighted average power (watts).
///
/// Each `EnergyReading` reports instantaneous watts over its `interval_ms`
/// window; multiplying by the interval gives joules for that window. This
/// accumulator does the multiply-and-sum so you can track total energy spent
/// over a longer run without rolling your own bookkeeping.
///
/// All fields are `f64` — summing thousands of `f32` samples in single
/// precision loses meaningful bits.
///
/// # Example
///
/// ```no_run
/// let mut sampler = power_monitor::EnergySampler::new().unwrap();
/// let mut acc = power_monitor::EnergyAccumulator::default();
/// for _ in 0..60 {
///     std::thread::sleep(std::time::Duration::from_millis(1000));
///     acc.add(&sampler.sample());
/// }
/// println!("total CPU energy: {:.2} J", acc.cpu_joules());
/// println!("average CPU power: {:.2} W", acc.average_cpu_watts());
/// ```
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct EnergyAccumulator {
    pub ecpu_joules: f64,
    pub pcpu_joules: f64,
    pub gpu_joules: f64,
    pub ane_joules: f64,
    pub dram_joules: f64,
    /// Sum of every observed `interval_ms`. Divide to recover averages.
    pub total_ms: f64,
}

impl EnergyAccumulator {
    /// Empty accumulator. Equivalent to [`EnergyAccumulator::default`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one reading's watts × interval into the running totals.
    ///
    /// Readings with `interval_ms <= 0.0` are skipped — the first call from
    /// a fresh [`EnergySampler`] can return a zero interval.
    pub fn add(&mut self, r: &EnergyReading) {
        if r.interval_ms <= 0.0 {
            return;
        }
        let s = f64::from(r.interval_ms) / 1000.0;
        self.ecpu_joules += f64::from(r.ecpu) * s;
        self.pcpu_joules += f64::from(r.pcpu) * s;
        self.gpu_joules += f64::from(r.gpu) * s;
        self.ane_joules += f64::from(r.ane) * s;
        self.dram_joules += f64::from(r.dram) * s;
        self.total_ms += f64::from(r.interval_ms);
    }

    /// Reset all totals to zero.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Elapsed seconds across all accumulated samples.
    pub fn total_seconds(&self) -> f64 {
        self.total_ms / 1000.0
    }

    /// ECPU + PCPU total energy in joules.
    pub fn cpu_joules(&self) -> f64 {
        self.ecpu_joules + self.pcpu_joules
    }

    /// Sum of CPU + GPU + ANE + DRAM energy in joules.
    pub fn total_joules(&self) -> f64 {
        self.cpu_joules() + self.gpu_joules + self.ane_joules + self.dram_joules
    }

    /// Time-weighted average CPU power in watts.
    pub fn average_cpu_watts(&self) -> f64 {
        self.average(self.cpu_joules())
    }

    /// Time-weighted average GPU power in watts.
    pub fn average_gpu_watts(&self) -> f64 {
        self.average(self.gpu_joules)
    }

    /// Time-weighted average ANE power in watts.
    pub fn average_ane_watts(&self) -> f64 {
        self.average(self.ane_joules)
    }

    /// Time-weighted average DRAM power in watts.
    pub fn average_dram_watts(&self) -> f64 {
        self.average(self.dram_joules)
    }

    #[inline]
    fn average(&self, joules: f64) -> f64 {
        if self.total_ms <= 0.0 {
            0.0
        } else {
            joules / self.total_seconds()
        }
    }
}

// ── EnergySampler ─────────────────────────────────────────────────────────────

/// IOReport subscription to the `Energy Model` channel group.
///
/// Allocation-free after the first call: CFString keys for
/// `"IOReportChannels"` / `"IOReportChannelUnit"` are cached in the sampler.
pub struct EnergySampler {
    sub: *mut c_void,
    subbed: CFMutableDictRef,
    prev: CFDictionaryRef,
    prev_time: Instant,
    k_channels: OwnedCf,
    k_unit: OwnedCf,
}

/// Backward-compatible alias.
pub type EnergyModel = EnergySampler;

unsafe impl Send for EnergySampler {}

impl EnergySampler {
    /// Subscribe to the IOReport `Energy Model` channel group.
    ///
    /// Returns `None` if the IOReport service is unavailable (e.g., inside a VM).
    pub fn new() -> Option<Self> {
        unsafe {
            let group = cfstr_from_cstr(b"Energy Model\0")?;
            let channels = OwnedCf::new(IOReportCopyChannelsInGroup(
                group.as_ptr() as CFStringRef,
                std::ptr::null(),
                0,
                0,
                0,
            ) as CFTypeRef)?;

            let ch_ptr = channels.as_ptr() as CFDictionaryRef;
            let ch_copy = OwnedCf::new(CFDictionaryCreateMutableCopy(
                std::ptr::null(),
                CFDictionaryGetCount(ch_ptr),
                ch_ptr,
            ) as CFTypeRef)?;

            let mut subbed: CFMutableDictRef = std::ptr::null_mut();
            let sub = IOReportCreateSubscription(
                std::ptr::null(),
                ch_copy.as_ptr() as CFMutableDictRef,
                &mut subbed,
                0,
                std::ptr::null(),
            );
            if sub.is_null() {
                return None;
            }

            let prev = IOReportCreateSamples(sub, subbed, std::ptr::null());
            if prev.is_null() {
                CFRelease(sub as CFTypeRef);
                if !subbed.is_null() {
                    CFRelease(subbed as CFTypeRef);
                }
                return None;
            }

            let k_channels = cfstr_from_cstr(CHANNELS_KEY)?;
            let k_unit = cfstr_from_cstr(UNIT_KEY)?;

            Some(EnergySampler {
                sub,
                subbed,
                prev,
                prev_time: Instant::now(),
                k_channels,
                k_unit,
            })
        }
    }

    /// Snapshot energy, compute watts since the last call, return reading.
    pub fn sample(&mut self) -> EnergyReading {
        unsafe {
            let now = Instant::now();
            let current = IOReportCreateSamples(self.sub, self.subbed, std::ptr::null());
            let interval_ms = now.duration_since(self.prev_time).as_secs_f32() * 1000.0;
            let delta = IOReportCreateSamplesDelta(self.prev, current, std::ptr::null());

            CFRelease(self.prev as CFTypeRef);
            self.prev = current;
            self.prev_time = now;

            let mut reading = EnergyReading {
                interval_ms,
                ..Default::default()
            };
            if !delta.is_null() {
                self.parse_delta(delta, interval_ms, &mut reading);
                CFRelease(delta as CFTypeRef);
            }
            reading
        }
    }

    /// Parse a delta dictionary into `out`. Zero-allocation.
    unsafe fn parse_delta(
        &self,
        delta: CFDictionaryRef,
        interval_ms: f32,
        out: &mut EnergyReading,
    ) {
        if interval_ms <= 0.0 {
            return;
        }
        let arr = unsafe { CFDictionaryGetValue(delta, self.k_channels.as_ptr()) } as CFArrayRef;
        if arr.is_null() {
            return;
        }

        let count = unsafe { CFArrayGetCount(arr) };
        let interval_s = interval_ms as f64 / 1000.0;
        let mut name_buf = [0u8; 64];
        let mut unit_buf = [0u8; 8];

        for i in 0..count {
            let ch = unsafe { CFArrayGetValueAtIndex(arr, i) } as CFDictionaryRef;
            if ch.is_null() {
                continue;
            }

            let name_bytes = cf_to_buf(unsafe { IOReportChannelGetChannelName(ch) }, &mut name_buf);

            let raw = unsafe { IOReportSimpleGetIntegerValue(ch, std::ptr::null_mut()) };
            let unit_ref = unsafe { CFDictionaryGetValue(ch, self.k_unit.as_ptr()) } as CFStringRef;
            let unit_bytes = cf_to_buf(unit_ref, &mut unit_buf);
            let unit = EnergyUnit::from_bytes(unit_bytes);
            let watts = (unit.to_joules(raw) / interval_s) as f32;

            match name_bytes {
                b"ECPU" => out.ecpu += watts,
                b"PCPU" => out.pcpu += watts,
                b"GPU" => out.gpu += watts,
                b"ANE" => out.ane += watts,
                b"DRAM" => out.dram += watts,
                b"AMCC" => out.amcc += watts,
                b"DCS" => out.dcs += watts,
                b"ISP" => out.isp += watts,
                b"AVE" => out.ave += watts,
                b"DISP" => out.disp += watts,
                _ => {}
            }
        }
    }
}

impl Drop for EnergySampler {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.prev as CFTypeRef);
            if !self.subbed.is_null() {
                CFRelease(self.subbed as CFTypeRef);
            }
            if !self.sub.is_null() {
                CFRelease(self.sub as CFTypeRef);
            }
        }
    }
}

// ── Raw channels (probe helper) ──────────────────────────────────────────────

/// A single raw IOReport channel entry — intended for hardware discovery and
/// debugging via the probe binary.
#[derive(Debug, Clone)]
pub struct RawChannel {
    pub name: String,
    pub fmt: i32,
    pub raw: i64,
    pub watts: f64,
}

impl EnergySampler {
    /// Return every Energy Model channel from the next delta as raw data.
    ///
    /// Intended for hardware discovery. Allocates — not for hot-path use.
    #[doc(hidden)]
    pub fn raw_channels(&mut self) -> Vec<RawChannel> {
        unsafe {
            let now = Instant::now();
            let current = IOReportCreateSamples(self.sub, self.subbed, std::ptr::null());
            let elapsed = now.duration_since(self.prev_time).as_secs_f32() * 1000.0;
            let delta = IOReportCreateSamplesDelta(self.prev, current, std::ptr::null());
            CFRelease(self.prev as CFTypeRef);
            self.prev = current;
            self.prev_time = now;

            if delta.is_null() {
                return Vec::new();
            }
            let delta = match OwnedCf::new(delta as CFTypeRef) {
                Some(d) => d,
                None => return Vec::new(),
            };

            let arr =
                CFDictionaryGetValue(delta.as_ptr() as CFDictionaryRef, self.k_channels.as_ptr())
                    as CFArrayRef;
            if arr.is_null() {
                return Vec::new();
            }

            let count = CFArrayGetCount(arr);
            let mut out = Vec::with_capacity(count as usize);
            for i in 0..count {
                let ch = CFArrayGetValueAtIndex(arr, i) as CFDictionaryRef;
                if ch.is_null() {
                    continue;
                }
                let name = cfstr_to_string(IOReportChannelGetChannelName(ch));
                let mut fmt = 0i32;
                let raw = IOReportSimpleGetIntegerValue(ch, &mut fmt);
                let watts = if elapsed > 0.0 {
                    raw as f64 / elapsed as f64
                } else {
                    0.0
                };
                out.push(RawChannel {
                    name,
                    fmt,
                    raw,
                    watts,
                });
            }
            out
        }
    }
}

// ── Multi-group sampler ───────────────────────────────────────────────────────

/// Performance-state residency for one P-state within an IOReport state channel.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StateResidency {
    /// State name (e.g., `"IDLE"`, `"972 MHz"`, `"V0S0"`). Cached — set once on
    /// the first sample and never reallocated.
    pub name: String,
    /// Hardware counter ticks accumulated in this state over the sample window.
    pub ticks: u64,
}

/// Payload of one IOReport channel in a delta sample.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ChannelKind {
    /// Simple scalar energy counter (IOReport format 0). Raw integer in the
    /// units given by [`ChannelSample::unit`].
    Energy(i64),
    /// Performance-state residency histogram (IOReport format 2).
    States(Vec<StateResidency>),
}

impl Default for ChannelKind {
    fn default() -> Self {
        ChannelKind::Energy(0)
    }
}

/// A single parsed IOReport channel from a delta sample.
///
/// Owned by a [`MultiGroupSampler`] scratch buffer. After the first
/// [`MultiGroupSampler::sample`] call, `group`, `channel`, `unit`, and state
/// names are all stable across calls — only `Energy` values and state `ticks`
/// are rewritten.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ChannelSample {
    /// Group discriminant — no allocation.
    pub group: ChannelGroup,
    /// Channel name (e.g., `"ECPU"`, `"PCPU"`, `"GPU"`, `"GPUPH"`). Cached — set
    /// once on the first sample and never reallocated.
    pub channel: String,
    /// Unit for `ChannelKind::Energy` values. Irrelevant for state channels.
    pub unit: EnergyUnit,
    /// Parsed channel data.
    pub kind: ChannelKind,
}

/// IOReport subscription to multiple channel groups simultaneously.
///
/// Subscribes to `Energy Model` + `CPU Stats` + `GPU Stats` in one handle.
/// After warm-up, [`MultiGroupSampler::sample`] performs **zero allocations**:
/// the channel schema (names, groups, units, state names) is cached on the
/// first call and only numeric values are updated on subsequent calls.
///
/// Access samples via [`MultiGroupSampler::sample`] (returns `interval_ms`)
/// followed by [`MultiGroupSampler::channels`].
///
/// `MultiGroupSampler` is `Send` — move it to a background thread for sampling.
///
/// # Example
///
/// ```no_run
/// let mut ior = power_monitor::MultiGroupSampler::new().unwrap();
/// loop {
///     std::thread::sleep(std::time::Duration::from_millis(1000));
///     let interval_ms = ior.sample();
///     for ch in ior.channels() {
///         if ch.group == power_monitor::ChannelGroup::EnergyModel {
///             if let power_monitor::ChannelKind::Energy(raw) = ch.kind {
///                 let watts = (ch.unit.to_joules(raw) / (interval_ms as f64 / 1000.0)) as f32;
///                 println!("{}: {:.2} W", ch.channel, watts);
///             }
///         }
///     }
/// }
/// ```
pub struct MultiGroupSampler {
    sub: *mut c_void,
    subbed: CFMutableDictRef,
    prev: CFDictionaryRef,
    prev_time: Instant,
    k_channels: OwnedCf,
    k_unit: OwnedCf,
    /// Reusable channel buffer — populated on first sample, updated in place
    /// on subsequent samples.
    scratch: Vec<ChannelSample>,
}

unsafe impl Send for MultiGroupSampler {}

impl MultiGroupSampler {
    /// Subscribe to `Energy Model`, `CPU Stats`, and `GPU Stats` channel groups.
    ///
    /// Returns `None` if the IOReport service is unavailable.
    pub fn new() -> Option<Self> {
        unsafe {
            // Energy Model — power channels (format=0 simple).
            let g1 = cfstr_from_cstr(b"Energy Model\0")?;
            let merged = OwnedCf::new(IOReportCopyChannelsInGroup(
                g1.as_ptr() as CFStringRef,
                std::ptr::null(),
                0,
                0,
                0,
            ) as CFTypeRef)?;

            // CPU Stats — "CPU Core Performance States" subgroup only.
            let g2 = cfstr_from_cstr(b"CPU Stats\0")?;
            let sg2 = cfstr_from_cstr(b"CPU Core Performance States\0")?;
            if let Some(cpu_ch) = OwnedCf::new(IOReportCopyChannelsInGroup(
                g2.as_ptr() as CFStringRef,
                sg2.as_ptr() as CFStringRef,
                0,
                0,
                0,
            ) as CFTypeRef)
            {
                IOReportMergeChannels(
                    merged.as_ptr() as CFMutableDictRef,
                    cpu_ch.as_ptr() as CFMutableDictRef,
                    std::ptr::null(),
                );
            }

            // GPU Stats — GPU P-state residency (format=2 state).
            let g3 = cfstr_from_cstr(b"GPU Stats\0")?;
            let sg3 = cfstr_from_cstr(b"GPU Performance States\0")?;
            if let Some(gpu_ch) = OwnedCf::new(IOReportCopyChannelsInGroup(
                g3.as_ptr() as CFStringRef,
                sg3.as_ptr() as CFStringRef,
                0,
                0,
                0,
            ) as CFTypeRef)
            {
                IOReportMergeChannels(
                    merged.as_ptr() as CFMutableDictRef,
                    gpu_ch.as_ptr() as CFMutableDictRef,
                    std::ptr::null(),
                );
            }

            // IOReportCreateSubscription requires a mutable copy of the merged dict.
            let mg_ptr = merged.as_ptr() as CFDictionaryRef;
            let mg_copy = OwnedCf::new(CFDictionaryCreateMutableCopy(
                std::ptr::null(),
                CFDictionaryGetCount(mg_ptr),
                mg_ptr,
            ) as CFTypeRef)?;

            let mut subbed: CFMutableDictRef = std::ptr::null_mut();
            let sub = IOReportCreateSubscription(
                std::ptr::null(),
                mg_copy.as_ptr() as CFMutableDictRef,
                &mut subbed,
                0,
                std::ptr::null(),
            );
            if sub.is_null() {
                return None;
            }

            let prev = IOReportCreateSamples(sub, subbed, std::ptr::null());
            if prev.is_null() {
                CFRelease(sub as CFTypeRef);
                if !subbed.is_null() {
                    CFRelease(subbed as CFTypeRef);
                }
                return None;
            }

            let k_channels = cfstr_from_cstr(CHANNELS_KEY)?;
            let k_unit = cfstr_from_cstr(UNIT_KEY)?;

            Some(MultiGroupSampler {
                sub,
                subbed,
                prev,
                prev_time: Instant::now(),
                k_channels,
                k_unit,
                scratch: Vec::new(),
            })
        }
    }

    /// Take a new snapshot, update the internal scratch buffer, and return
    /// the elapsed interval in milliseconds since the previous call.
    ///
    /// Use [`MultiGroupSampler::channels`] to read the updated samples.
    pub fn sample(&mut self) -> f32 {
        unsafe {
            let now = Instant::now();
            let current = IOReportCreateSamples(self.sub, self.subbed, std::ptr::null());
            let interval_ms = now.duration_since(self.prev_time).as_secs_f32() * 1000.0;
            let delta = IOReportCreateSamplesDelta(self.prev, current, std::ptr::null());
            CFRelease(self.prev as CFTypeRef);
            self.prev = current;
            self.prev_time = now;

            if delta.is_null() {
                crate::event::emit(crate::event::SamplerEvent::NullDelta);
            } else {
                self.refresh_scratch(delta);
                CFRelease(delta as CFTypeRef);
            }
            interval_ms
        }
    }

    /// Samples collected by the most recent [`MultiGroupSampler::sample`] call.
    #[inline]
    pub fn channels(&self) -> &[ChannelSample] {
        &self.scratch
    }

    /// Walk the delta dictionary and update the scratch buffer in place.
    ///
    /// On the first call the scratch buffer is empty and we populate names,
    /// groups, units, and state names. On subsequent calls we only overwrite
    /// `ChannelKind::Energy` values and `StateResidency::ticks`.
    ///
    /// A full rebuild is triggered when the channel count changes **or** when
    /// any state-format channel's P-state count drifts (e.g. a cluster gains a
    /// new P-state at runtime). Either case emits `SchemaChanged`.
    unsafe fn refresh_scratch(&mut self, delta: CFDictionaryRef) {
        let arr = unsafe { CFDictionaryGetValue(delta, self.k_channels.as_ptr()) } as CFArrayRef;
        if arr.is_null() {
            return;
        }
        let count = unsafe { CFArrayGetCount(arr) } as usize;

        // Detect schema change: channel count mismatch, or per-channel state
        // count drift (a cluster added/removed a P-state since the last sample).
        let mut needs_rebuild = self.scratch.len() != count;
        if !needs_rebuild {
            for (i, slot) in self.scratch.iter().enumerate() {
                let ChannelKind::States(states) = &slot.kind else {
                    continue;
                };
                let ch = unsafe { CFArrayGetValueAtIndex(arr, i as CFIndex) } as CFDictionaryRef;
                if ch.is_null() {
                    continue;
                }
                let n = unsafe { IOReportStateGetCount(ch) } as usize;
                if n != states.len() {
                    needs_rebuild = true;
                    break;
                }
            }
        }

        if needs_rebuild {
            let previous = self.scratch.len();
            self.scratch.clear();
            self.scratch.reserve(count);
            for i in 0..count {
                let ch = unsafe { CFArrayGetValueAtIndex(arr, i as CFIndex) } as CFDictionaryRef;
                if ch.is_null() {
                    self.scratch.push(ChannelSample::default());
                    continue;
                }
                self.scratch
                    .push(unsafe { build_sample(ch, self.k_unit.as_ptr()) });
            }
            crate::event::emit(crate::event::SamplerEvent::SchemaChanged {
                previous,
                current: count,
            });
            return;
        }

        // Fast path — same schema, just refresh numeric values.
        for (i, slot) in self.scratch.iter_mut().enumerate() {
            let ch = unsafe { CFArrayGetValueAtIndex(arr, i as CFIndex) } as CFDictionaryRef;
            if ch.is_null() {
                continue;
            }
            match &mut slot.kind {
                ChannelKind::Energy(raw) => {
                    *raw = unsafe { IOReportSimpleGetIntegerValue(ch, std::ptr::null_mut()) };
                }
                ChannelKind::States(states) => {
                    for (j, st) in states.iter_mut().enumerate() {
                        st.ticks = unsafe { IOReportStateGetResidency(ch, j as i32) };
                    }
                }
            }
        }
    }
}

/// Build a fresh [`ChannelSample`] from a channel dict. Called during schema
/// init; subsequent samples mutate the returned slot in place.
unsafe fn build_sample(ch: CFDictionaryRef, k_unit: CFTypeRef) -> ChannelSample {
    let mut name_buf = [0u8; 128];
    let mut unit_buf = [0u8; 8];

    let group = ChannelGroup::from_bytes(cf_to_buf(
        unsafe { IOReportChannelGetGroup(ch) },
        &mut name_buf,
    ));
    let channel = {
        let bytes = cf_to_buf(unsafe { IOReportChannelGetChannelName(ch) }, &mut name_buf);
        std::str::from_utf8(bytes).unwrap_or("").to_owned()
    };

    let n_states = unsafe { IOReportStateGetCount(ch) };
    if n_states > 0 {
        let mut states = Vec::with_capacity(n_states as usize);
        for j in 0..n_states {
            let name_bytes = cf_to_buf(
                unsafe { IOReportStateGetNameForIndex(ch, j) },
                &mut name_buf,
            );
            let name = std::str::from_utf8(name_bytes).unwrap_or("").to_owned();
            let ticks = unsafe { IOReportStateGetResidency(ch, j) };
            states.push(StateResidency { name, ticks });
        }
        ChannelSample {
            group,
            channel,
            unit: EnergyUnit::MilliJoule,
            kind: ChannelKind::States(states),
        }
    } else {
        let raw = unsafe { IOReportSimpleGetIntegerValue(ch, std::ptr::null_mut()) };
        let unit_ref = unsafe { CFDictionaryGetValue(ch, k_unit) } as CFStringRef;
        let unit = EnergyUnit::from_bytes(cf_to_buf(unit_ref, &mut unit_buf));
        ChannelSample {
            group,
            channel,
            unit,
            kind: ChannelKind::Energy(raw),
        }
    }
}

impl Drop for MultiGroupSampler {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.prev as CFTypeRef);
            if !self.subbed.is_null() {
                CFRelease(self.subbed as CFTypeRef);
            }
            if !self.sub.is_null() {
                CFRelease(self.sub as CFTypeRef);
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn energy_unit_bytes_roundtrip() {
        assert_eq!(EnergyUnit::from_bytes(b"mJ"), EnergyUnit::MilliJoule);
        assert_eq!(EnergyUnit::from_bytes(b"uJ"), EnergyUnit::MicroJoule);
        assert_eq!(EnergyUnit::from_bytes(b"nJ"), EnergyUnit::NanoJoule);
        assert_eq!(EnergyUnit::from_bytes(b"???"), EnergyUnit::MilliJoule);
    }

    #[test]
    fn energy_unit_to_joules() {
        assert!((EnergyUnit::MilliJoule.to_joules(1000) - 1.0).abs() < 1e-9);
        assert!((EnergyUnit::MicroJoule.to_joules(1_000_000) - 1.0).abs() < 1e-9);
        assert!((EnergyUnit::NanoJoule.to_joules(1_000_000_000) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn channel_group_from_bytes() {
        assert_eq!(
            ChannelGroup::from_bytes(b"Energy Model"),
            ChannelGroup::EnergyModel
        );
        assert_eq!(
            ChannelGroup::from_bytes(b"CPU Stats"),
            ChannelGroup::CpuStats
        );
        assert_eq!(
            ChannelGroup::from_bytes(b"GPU Stats"),
            ChannelGroup::GpuStats
        );
        assert_eq!(ChannelGroup::from_bytes(b"Unknown"), ChannelGroup::Other);
    }

    // ── EnergyAccumulator ─────────────────────────────────────────────────────

    fn reading(cpu: f32, gpu: f32, interval_ms: f32) -> EnergyReading {
        EnergyReading {
            pcpu: cpu,
            gpu,
            interval_ms,
            ..Default::default()
        }
    }

    #[test]
    fn accumulator_defaults_to_empty() {
        let a = EnergyAccumulator::new();
        assert_eq!(a.total_ms, 0.0);
        assert_eq!(a.cpu_joules(), 0.0);
        assert_eq!(a.total_joules(), 0.0);
        assert_eq!(a.average_cpu_watts(), 0.0);
    }

    #[test]
    fn accumulator_integrates_watts_over_interval() {
        // 5 W for 2 s = 10 J; 10 W for 1 s = 10 J → 20 J total, avg 20/3 W.
        let mut a = EnergyAccumulator::new();
        a.add(&reading(5.0, 0.0, 2000.0));
        a.add(&reading(10.0, 0.0, 1000.0));
        assert!((a.cpu_joules() - 20.0).abs() < 1e-6);
        assert!((a.total_seconds() - 3.0).abs() < 1e-6);
        assert!((a.average_cpu_watts() - 20.0 / 3.0).abs() < 1e-6);
    }

    #[test]
    fn accumulator_skips_zero_interval_readings() {
        let mut a = EnergyAccumulator::new();
        a.add(&reading(5.0, 0.0, 0.0));
        a.add(&reading(5.0, 0.0, -1.0));
        assert_eq!(a.total_ms, 0.0);
        assert_eq!(a.cpu_joules(), 0.0);
    }

    #[test]
    fn accumulator_sums_all_components() {
        let mut a = EnergyAccumulator::new();
        a.add(&EnergyReading {
            ecpu: 1.0,
            pcpu: 2.0,
            gpu: 3.0,
            ane: 4.0,
            dram: 5.0,
            interval_ms: 1000.0,
            ..Default::default()
        });
        assert!((a.ecpu_joules - 1.0).abs() < 1e-6);
        assert!((a.pcpu_joules - 2.0).abs() < 1e-6);
        assert!((a.cpu_joules() - 3.0).abs() < 1e-6);
        assert!((a.total_joules() - 15.0).abs() < 1e-6);
    }

    #[test]
    fn accumulator_reset_clears_state() {
        let mut a = EnergyAccumulator::new();
        a.add(&reading(5.0, 0.0, 1000.0));
        assert!(a.total_ms > 0.0);
        a.reset();
        assert_eq!(a.total_ms, 0.0);
        assert_eq!(a.cpu_joules(), 0.0);
    }
}
