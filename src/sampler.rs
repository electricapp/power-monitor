//! High-level [`Sampler`] that aggregates SMC, IOReport, and system metrics
//! into a single [`Metrics`] snapshot over a configurable time window.
//!
//! Takes `N_SAMPLES` sub-samples for IOReport data (to smooth transient P-state
//! residency spikes) and averages them. Slow-changing data (thermals, memory,
//! sys power) is read **once per window** — no point reading SMC four times
//! for values that drift over tens of seconds.
//!
//! After warm-up, the sample path is allocation-free.

use std::thread;
use std::time::Duration;

use crate::Smc;
use crate::ioreport::{ChannelGroup, ChannelKind, EnergyUnit, MultiGroupSampler, StateResidency};
use crate::memory::{self, MemoryInfo, SwapInfo};
use crate::soc::SocInfo;

/// Default sub-sample count — a window is split this many times for
/// IOReport smoothing. Callers that need higher-resolution sampling
/// (e.g. a telemetry UI that wants ≥10 Hz frames) can override this
/// with [`Sampler::with_samples_per_window`].
pub const N_SAMPLES: usize = 4;

/// Minimum dwell per sub-sample in milliseconds. IOReport subscriptions
/// need at least this long for the kernel to emit a delta on every
/// registered channel; going below produces noisy or zero readings.
const MIN_SUB_MS: u64 = 50;

// ── Public metric types ───────────────────────────────────────────────────────

/// CPU or GPU cluster utilisation derived from IOReport P-state residency.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[non_exhaustive]
pub struct ClusterMetrics {
    /// Weighted average active frequency in MHz.
    pub freq_mhz: u32,
    /// Active fraction of total time: `0.0` = idle, `1.0` = full utilisation.
    pub utilization: f32,
}

/// Complete system snapshot averaged over a sampling window.
///
/// All power values are in **watts**, temperatures in **°C**, memory in **bytes**.
///
/// Note: `all_power = cpu_power + gpu_power + ane_power` (compute only).
/// `sys_power` is the full-system SMC rail reading and will be higher than
/// `all_power` due to DRAM, display, bus, and other subsystems.
///
/// `Metrics` is `Copy` (all fields are primitive or `Copy` structs). This lets
/// downstream code publish snapshots through lock-free single-writer cells
/// without heap traffic — see `serve_cmd`'s seqlock usage.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct Metrics {
    // ── Utilisation ──────────────────────────────────────────────────────────
    /// Efficiency CPU cluster (ECPU, and MCPU on M5 Pro/Max).
    pub ecpu: ClusterMetrics,
    /// Performance CPU cluster (PCPU / "Super" on M5).
    pub pcpu: ClusterMetrics,
    /// GPU.
    pub gpu_util: f32,
    pub gpu_freq_mhz: u32,

    // ── Power (watts, from IOReport Energy Model) ─────────────────────────
    /// CPU energy model total (all "*CPU Energy" channels).
    pub cpu_power: f32,
    /// GPU energy model.
    pub gpu_power: f32,
    /// Apple Neural Engine.
    pub ane_power: f32,
    /// DRAM / memory subsystem.
    pub dram_power: f32,
    /// Total SoC compute power (cpu + gpu + ane).
    pub all_power: f32,
    /// Full system draw from SMC `PSTR`.
    pub sys_power: f32,

    // ── Memory ───────────────────────────────────────────────────────────────
    pub memory: MemoryInfo,
    pub swap: SwapInfo,

    // ── Temperature (from SMC) ────────────────────────────────────────────
    pub cpu_temp: f32,
    pub gpu_temp: f32,

    // ── Fan (from SMC) ────────────────────────────────────────────────────
    /// Higher-duty fan's current RPM. Zero on fanless hardware.
    pub fan_rpm: u32,
    /// Higher-duty fan's max RPM. Zero on fanless hardware. The TUI / dashboard
    /// use `fan_rpm / fan_max_rpm` for the duty cycle.
    pub fan_max_rpm: u32,

    /// Wall-clock duration of the full sample window in milliseconds
    /// (sum of all sub-sample IOReport intervals).
    pub interval_ms: f32,
}

// ── Sampler ───────────────────────────────────────────────────────────────────

/// Aggregated Apple Silicon performance sampler.
///
/// Wraps [`MultiGroupSampler`] (IOReport), [`Smc`], and system memory reads.
///
/// All scratch buffers (per-cluster state accumulators, channel sample cache)
/// are owned by the sampler. After the first [`Sampler::get_metrics`] call,
/// the sample path performs no heap allocations.
pub struct Sampler {
    multi: MultiGroupSampler,
    smc: Smc,
    pub soc: SocInfo,
    // Freq tables populated at construction time from `SocInfo`.
    ecpu_freqs: Vec<u32>,
    pcpu_freqs: Vec<u32>,
    gpu_freqs: Vec<u32>,
    // Per-cluster state accumulator scratch — persistent so names are cached
    // across calls and ticks are simply overwritten each sub-sample.
    ecpu_scratch: Vec<StateResidency>,
    pcpu_scratch: Vec<StateResidency>,
    gpu_scratch: Vec<StateResidency>,
    // Pre-classified per-channel routing tags, parallel to `multi.channels()`.
    // Rebuilt on schema change (channel count drift); after that, the inner
    // sub-sample loop is a tag dispatch — no per-channel string compares.
    routing: Vec<RouteTag>,
    /// Number of sub-samples to average per `get_metrics` call. Default
    /// [`N_SAMPLES`]; a telemetry UI that wants high-rate frames can
    /// drop this to 1 or 2 (see [`Sampler::with_samples_per_window`]).
    samples_per_window: usize,
}

/// Per-channel routing tag, computed once at schema build and reused on the
/// hot path. Eliminates per-channel string matching in the sub-sample loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteTag {
    /// Channel is irrelevant or has no destination.
    Skip,
    /// EnergyModel: route watts to `acc.gpu_power` (instantaneous "GPU", not "GPU Energy").
    EnergyGpu,
    /// EnergyModel: route watts to `acc.cpu_power` (any "*CPU Energy" channel).
    EnergyCpu,
    /// EnergyModel: route watts to `acc.ane_power`.
    EnergyAne,
    /// EnergyModel: route watts to `acc.dram_power`.
    EnergyDram,
    /// CpuStats P-state residency for the performance cluster (PCPU*).
    ClusterP,
    /// CpuStats P-state residency for the efficiency / mid cluster (ECPU*/MCPU*).
    ClusterEorM,
    /// GpuStats: GPUPH P-state residency.
    GpuStates,
}

impl Sampler {
    /// Open all subsystems. Returns `None` if any critical subsystem fails.
    pub fn new() -> Option<Self> {
        Self::with_samples_per_window(N_SAMPLES)
    }

    /// Open all subsystems with a custom sub-sample count. Lower values
    /// reduce the latency of the first frame (and the cadence of every
    /// frame thereafter) at the cost of less averaging. Values below 1
    /// are clamped to 1.
    pub fn with_samples_per_window(samples: usize) -> Option<Self> {
        let multi = MultiGroupSampler::new()?;
        let smc = Smc::open().ok()?;
        let soc = SocInfo::from_system();

        let pcpu_freqs = soc
            .pcpu_level()
            .filter(|l| !l.freqs_mhz.is_empty())
            .map(|l| l.freqs_mhz.clone())
            .unwrap_or_default();
        let ecpu_freqs = soc
            .ecpu_level()
            .filter(|l| !l.freqs_mhz.is_empty())
            .map(|l| l.freqs_mhz.clone())
            .unwrap_or_default();
        let gpu_freqs = soc.gpu_freqs_mhz.clone();

        Some(Sampler {
            multi,
            smc,
            soc,
            ecpu_freqs,
            pcpu_freqs,
            gpu_freqs,
            ecpu_scratch: Vec::new(),
            pcpu_scratch: Vec::new(),
            gpu_scratch: Vec::new(),
            routing: Vec::new(),
            samples_per_window: samples.max(1),
        })
    }

    /// Sample metrics over `duration_ms` milliseconds.
    ///
    /// IOReport channels are sub-sampled `samples_per_window` times for
    /// smoothing; SMC thermals, power, and memory are read **once** at
    /// the end of the window (they change slowly compared to the sampling
    /// rate). Each sub-sample dwells at least [`MIN_SUB_MS`] milliseconds
    /// regardless of `duration_ms`, so the actual call duration is
    /// `max(duration_ms, samples_per_window * MIN_SUB_MS)`.
    ///
    /// # Blocking
    ///
    /// This call blocks the calling thread for the full sample duration.
    pub fn get_metrics(&mut self, duration_ms: u64) -> Metrics {
        let n = self.samples_per_window;
        let sub_ms = (duration_ms / n as u64).max(MIN_SUB_MS);
        let mut acc = Accum::default();

        for _ in 0..n {
            thread::sleep(Duration::from_millis(sub_ms));
            let interval_ms = self.multi.sample();
            acc.interval_ms += interval_ms;

            let channels = self.multi.channels();
            if self.routing.len() != channels.len() {
                self.routing.clear();
                self.routing.reserve(channels.len());
                self.routing.extend(channels.iter().map(classify_channel));
            }

            process_sub_sample(
                channels,
                &self.routing,
                interval_ms,
                &mut self.ecpu_scratch,
                &mut self.pcpu_scratch,
                &mut self.gpu_scratch,
                &self.ecpu_freqs,
                &self.pcpu_freqs,
                &self.gpu_freqs,
                &mut acc,
            );
        }

        let (cpu_t, gpu_t) = self.smc.read_cpu_gpu_temps();
        let sys_power = self.smc.read_system_power();
        let mem = memory::read_memory().unwrap_or_default();
        let swap = memory::read_swap().unwrap_or_default();
        let fans = self.smc.read_fans();

        // 0°C physical floor: a powered SoC sits above freezing. Anything
        // at or below 0°C is a rail-off sentinel, not a real temperature.
        let cpu_temp = if cpu_t > 0.0 { cpu_t } else { f32::NAN };
        let gpu_temp = if gpu_t > 0.0 { gpu_t } else { f32::NAN };

        // Pick the higher-duty fan when two are present; fans with no max RPM
        // are skipped so the dashboard duty calc can't divide by zero.
        let (fan_rpm, fan_max_rpm) = fans
            .fans
            .iter()
            .flatten()
            .filter(|f| f.max_rpm > 0.0)
            .max_by(|a, b| {
                a.duty_cycle()
                    .partial_cmp(&b.duty_cycle())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|f| (f.rpm as u32, f.max_rpm as u32))
            .unwrap_or((0, 0));

        acc.finish(
            n,
            cpu_temp,
            gpu_temp,
            sys_power,
            mem,
            swap,
            fan_rpm,
            fan_max_rpm,
        )
    }
}

/// Classify one channel into a [`RouteTag`]. Called once per channel at
/// schema-build time; the hot sub-sample loop only reads tags by index.
fn classify_channel(ch: &crate::ioreport::ChannelSample) -> RouteTag {
    match ch.group {
        ChannelGroup::EnergyModel => match ch.channel.as_str() {
            // "GPU" = per-interval delta in mJ (correct).
            // "GPU Energy" = absolute lifetime accumulator — do NOT use.
            "GPU" => RouteTag::EnergyGpu,
            n if n.ends_with("CPU Energy") => RouteTag::EnergyCpu,
            n if n.starts_with("ANE") => RouteTag::EnergyAne,
            n if n.starts_with("DRAM") => RouteTag::EnergyDram,
            _ => RouteTag::Skip,
        },
        ChannelGroup::CpuStats => match classify_cpu_cluster(ch.channel.as_str()) {
            Some(CpuCluster::P) => RouteTag::ClusterP,
            Some(CpuCluster::EorM) => RouteTag::ClusterEorM,
            None => RouteTag::Skip,
        },
        ChannelGroup::GpuStats if ch.channel == "GPUPH" => RouteTag::GpuStates,
        _ => RouteTag::Skip,
    }
}

/// Per-unit "watts per raw count" scale factor for one sub-sample.
///
/// Computing `raw * scale[unit]` replaces `to_joules(raw) / interval_s` —
/// one multiply per channel instead of one division per channel.
#[inline]
fn unit_scale(interval_s: f64) -> [f64; 3] {
    let inv = 1.0 / interval_s;
    [
        1e-3 * inv, // mJ → J/s
        1e-6 * inv, // µJ → J/s
        1e-9 * inv, // nJ → J/s
    ]
}

#[inline]
fn unit_index(u: EnergyUnit) -> usize {
    match u {
        EnergyUnit::MilliJoule => 0,
        EnergyUnit::MicroJoule => 1,
        EnergyUnit::NanoJoule => 2,
    }
}

/// Process one IOReport sub-sample: route energy channels to the accumulator
/// and merge per-core P-state residency into the cluster scratch buffers.
#[allow(clippy::too_many_arguments)]
fn process_sub_sample(
    channels: &[crate::ioreport::ChannelSample],
    routing: &[RouteTag],
    interval_ms: f32,
    ecpu_scratch: &mut Vec<StateResidency>,
    pcpu_scratch: &mut Vec<StateResidency>,
    gpu_scratch: &mut Vec<StateResidency>,
    ecpu_freqs: &[u32],
    pcpu_freqs: &[u32],
    gpu_freqs: &[u32],
    acc: &mut Accum,
) {
    // Zero cluster tick accumulators for this sub-sample (names preserved).
    reset_ticks(ecpu_scratch);
    reset_ticks(pcpu_scratch);
    reset_ticks(gpu_scratch);

    if interval_ms <= 0.0 {
        return;
    }
    let scale = unit_scale(f64::from(interval_ms) / 1000.0);

    for (ch, &tag) in channels.iter().zip(routing) {
        match tag {
            RouteTag::Skip => continue,
            RouteTag::EnergyGpu
            | RouteTag::EnergyCpu
            | RouteTag::EnergyAne
            | RouteTag::EnergyDram => {
                let ChannelKind::Energy(raw) = ch.kind else {
                    continue;
                };
                let watts = (raw as f64 * scale[unit_index(ch.unit)]) as f32;
                match tag {
                    RouteTag::EnergyGpu => acc.gpu_power += watts,
                    RouteTag::EnergyCpu => acc.cpu_power += watts,
                    RouteTag::EnergyAne => acc.ane_power += watts,
                    RouteTag::EnergyDram => acc.dram_power += watts,
                    _ => unreachable!(),
                }
            }
            RouteTag::ClusterP => {
                if let ChannelKind::States(ref states) = ch.kind {
                    add_ticks(pcpu_scratch, states);
                }
            }
            RouteTag::ClusterEorM => {
                if let ChannelKind::States(ref states) = ch.kind {
                    add_ticks(ecpu_scratch, states);
                }
            }
            RouteTag::GpuStates => {
                if let ChannelKind::States(ref states) = ch.kind {
                    add_ticks(gpu_scratch, states);
                }
            }
        }
    }

    // Cluster utilisation for this sub-sample.
    if !ecpu_scratch.is_empty() {
        let (freq, util) = calc_utilization(ecpu_scratch, ecpu_freqs);
        acc.ecpu_freq += freq as f32;
        acc.ecpu_util += util;
    }
    if !pcpu_scratch.is_empty() {
        let (freq, util) = calc_utilization(pcpu_scratch, pcpu_freqs);
        acc.pcpu_freq += freq as f32;
        acc.pcpu_util += util;
    }
    if !gpu_scratch.is_empty() {
        let (freq, util) = calc_utilization(gpu_scratch, gpu_freqs);
        acc.gpu_freq += freq as f32;
        acc.gpu_util += util;
    }
}

// ── CPU cluster classifier ────────────────────────────────────────────────────

/// Which cluster tier a `CPU Stats` channel belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CpuCluster {
    /// Performance cluster (`PCPU*`).
    P,
    /// Efficiency (`ECPU*`) or M5 mid-tier (`MCPU*`) cluster.
    EorM,
}

/// Strip the `DIE_N_` prefix IOReport prepends on Ultra / multi-die SoCs.
#[inline]
fn strip_die_prefix(name: &str) -> &str {
    let Some(rest) = name.strip_prefix("DIE_") else {
        return name;
    };
    // After `DIE_` expect one or more digits then `_`.
    match rest.bytes().position(|b| b == b'_') {
        Some(idx) if rest.as_bytes()[..idx].iter().all(u8::is_ascii_digit) => &rest[idx + 1..],
        _ => name,
    }
}

/// Classify an IOReport `CPU Stats` channel name into a cluster tier.
///
/// Prefix-anchored (post-`DIE_N_` strip) so a stray substring like `"APCPU"`
/// can't accidentally route to the P-cluster.
#[inline]
fn classify_cpu_cluster(name: &str) -> Option<CpuCluster> {
    let stem = strip_die_prefix(name);
    if stem.starts_with("PCPU") {
        Some(CpuCluster::P)
    } else if stem.starts_with("ECPU") || stem.starts_with("MCPU") {
        Some(CpuCluster::EorM)
    } else {
        None
    }
}

// ── State buffer helpers ──────────────────────────────────────────────────────

/// Zero every tick counter while preserving state names (allocation-free).
#[inline]
fn reset_ticks(accum: &mut [StateResidency]) {
    for s in accum {
        s.ticks = 0;
    }
}

/// Accumulate residency ticks from `src` into `accum` element-wise.
///
/// On the very first call (length mismatch), the schema is copied from `src`
/// — that's the only path that allocates. Subsequent calls only touch `ticks`.
fn add_ticks(accum: &mut Vec<StateResidency>, src: &[StateResidency]) {
    if accum.len() != src.len() {
        accum.clear();
        accum.extend_from_slice(src);
    } else {
        for (a, s) in accum.iter_mut().zip(src) {
            a.ticks += s.ticks;
        }
    }
}

// ── Utilisation math ──────────────────────────────────────────────────────────

/// Returns `true` for P-state names that represent idle/off conditions.
#[inline]
fn is_idle_state(name: &str) -> bool {
    matches!(name, "IDLE" | "DOWN" | "OFF" | "SWOFF")
}

/// Compute weighted average frequency and utilisation from P-state residency data.
///
/// # Arguments
///
/// - `states`: P-state residency from [`ChannelKind::States`], in P-state order.
/// - `freqs`: frequency table in MHz from [`SocInfo`], aligned to active (non-idle)
///   P-states. Pass an empty slice when frequencies are unavailable.
///
/// # Returns
///
/// `(weighted_avg_freq_mhz, utilization_fraction)` where utilization ∈ `[0.0, 1.0]`.
pub fn calc_utilization(states: &[StateResidency], freqs: &[u32]) -> (u32, f32) {
    // Single pass: total ticks, active ticks, and freq-weighted accumulators.
    // Idle states are skipped wherever they appear (prefix today, but a future
    // schema may interleave them — checking inside the loop is robust).
    let mut total_ticks: u64 = 0;
    let mut active_ticks: u64 = 0;
    let mut weighted: f64 = 0.0;
    let mut covered: u64 = 0;
    let mut active_idx: usize = 0;

    for s in states {
        total_ticks += s.ticks;
        if is_idle_state(&s.name) {
            continue;
        }
        active_ticks += s.ticks;
        // Active P-states beyond the freq table contribute to utilisation but
        // not the weighted-frequency average (no MHz known for them).
        if let Some(&f) = freqs.get(active_idx) {
            weighted += f64::from(f) * s.ticks as f64;
            covered += s.ticks;
        }
        active_idx += 1;
    }

    if total_ticks == 0 || active_ticks == 0 {
        return (0, 0.0);
    }

    let utilization = active_ticks as f32 / total_ticks as f32;
    let avg_freq = if covered > 0 {
        (weighted / covered as f64) as u32
    } else {
        0
    };
    (avg_freq, utilization)
}

// ── Averaging accumulator ─────────────────────────────────────────────────────

#[derive(Default)]
struct Accum {
    ecpu_freq: f32,
    ecpu_util: f32,
    pcpu_freq: f32,
    pcpu_util: f32,
    gpu_freq: f32,
    gpu_util: f32,
    cpu_power: f32,
    gpu_power: f32,
    ane_power: f32,
    dram_power: f32,
    interval_ms: f32,
}

impl Accum {
    #[allow(clippy::too_many_arguments)]
    fn finish(
        self,
        n: usize,
        cpu_temp: f32,
        gpu_temp: f32,
        sys_power: f32,
        memory: MemoryInfo,
        swap: SwapInfo,
        fan_rpm: u32,
        fan_max_rpm: u32,
    ) -> Metrics {
        let nf = n as f32;
        let cpu_power = self.cpu_power / nf;
        let gpu_power = self.gpu_power / nf;
        let ane_power = self.ane_power / nf;
        Metrics {
            ecpu: ClusterMetrics {
                freq_mhz: (self.ecpu_freq / nf) as u32,
                utilization: self.ecpu_util / nf,
            },
            pcpu: ClusterMetrics {
                freq_mhz: (self.pcpu_freq / nf) as u32,
                utilization: self.pcpu_util / nf,
            },
            gpu_util: self.gpu_util / nf,
            gpu_freq_mhz: (self.gpu_freq / nf) as u32,
            cpu_power,
            gpu_power,
            ane_power,
            dram_power: self.dram_power / nf,
            all_power: cpu_power + gpu_power + ane_power,
            sys_power,
            cpu_temp,
            gpu_temp,
            memory,
            swap,
            fan_rpm,
            fan_max_rpm,
            interval_ms: self.interval_ms,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sr(name: &str, ticks: u64) -> StateResidency {
        StateResidency {
            name: name.into(),
            ticks,
        }
    }

    // ── is_idle_state ─────────────────────────────────────────────────────────

    #[test]
    fn idle_state_names_recognized() {
        for name in ["IDLE", "DOWN", "OFF", "SWOFF"] {
            assert!(is_idle_state(name), "{name} should be idle");
        }
    }

    #[test]
    fn active_state_names_not_idle() {
        for name in ["V0P7", "V1P18", "P1", "P2", "1308 MHz", "NON_IDLE", ""] {
            assert!(!is_idle_state(name), "{name} should not be idle");
        }
    }

    // ── calc_utilization ──────────────────────────────────────────────────────

    #[test]
    fn all_idle_returns_zero() {
        let states = vec![sr("IDLE", 10_000)];
        let (freq, util) = calc_utilization(&states, &[]);
        assert_eq!(util, 0.0);
        assert_eq!(freq, 0);
    }

    #[test]
    fn all_active_returns_one() {
        let states = vec![sr("V0P7", 10_000)];
        let (freq, util) = calc_utilization(&states, &[1000]);
        assert!((util - 1.0).abs() < 1e-6);
        assert_eq!(freq, 1000);
    }

    #[test]
    fn half_idle_half_active() {
        let states = vec![sr("IDLE", 5_000), sr("V0P5", 5_000)];
        let (freq, util) = calc_utilization(&states, &[2000]);
        assert!((util - 0.5).abs() < 1e-6);
        assert_eq!(freq, 2000);
    }

    #[test]
    fn weighted_freq_two_active_states() {
        // 1000 ticks @ 1000 MHz, 3000 ticks @ 3000 MHz → avg = (1000+9000)/4000 = 2500
        let states = vec![sr("IDLE", 0), sr("V0P1", 1_000), sr("V0P3", 3_000)];
        let (freq, util) = calc_utilization(&states, &[1000, 3000]);
        assert!((util - 1.0).abs() < 1e-6, "util={util}");
        assert_eq!(freq, 2500);
    }

    #[test]
    fn empty_freq_table_gives_zero_mhz_but_correct_util() {
        let states = vec![sr("IDLE", 1_000), sr("V0P7", 1_000)];
        let (freq, util) = calc_utilization(&states, &[]);
        assert!((util - 0.5).abs() < 1e-6);
        assert_eq!(freq, 0);
    }

    #[test]
    fn zero_total_ticks_returns_zero() {
        let states = vec![sr("IDLE", 0), sr("V0P7", 0)];
        let (freq, util) = calc_utilization(&states, &[1000]);
        assert_eq!(util, 0.0);
        assert_eq!(freq, 0);
    }

    #[test]
    fn multiple_idle_states_all_skipped() {
        let states = vec![
            sr("IDLE", 8_000),
            sr("DOWN", 1_000),
            sr("V0P1", 500),
            sr("V0P2", 500),
        ];
        let (_, util) = calc_utilization(&states, &[1000, 2000]);
        assert!((util - 0.1).abs() < 1e-6, "util={util}");
    }

    // ── add_ticks ──────────────────────────────────────────────────────────

    #[test]
    fn add_ticks_into_empty_clones_source() {
        let mut accum = Vec::new();
        let src = vec![sr("IDLE", 100), sr("V0P1", 50)];
        add_ticks(&mut accum, &src);
        assert_eq!(accum.len(), 2);
        assert_eq!(accum[0].ticks, 100);
        assert_eq!(accum[1].ticks, 50);
    }

    #[test]
    fn add_ticks_accumulates_by_index() {
        let mut accum = vec![sr("IDLE", 100), sr("V0P1", 50)];
        let src = vec![sr("IDLE", 200), sr("V0P1", 75)];
        add_ticks(&mut accum, &src);
        assert_eq!(accum[0].ticks, 300);
        assert_eq!(accum[1].ticks, 125);
        assert_eq!(accum[0].name, "IDLE");
    }

    #[test]
    fn reset_ticks_preserves_names() {
        let mut accum = vec![sr("IDLE", 100), sr("V0P1", 50)];
        reset_ticks(&mut accum);
        assert_eq!(accum[0].ticks, 0);
        assert_eq!(accum[1].ticks, 0);
        assert_eq!(accum[0].name, "IDLE");
        assert_eq!(accum[1].name, "V0P1");
    }

    // ── CPU cluster classifier ───────────────────────────────────────────────

    #[test]
    fn classify_bare_cluster_names() {
        assert_eq!(classify_cpu_cluster("PCPU"), Some(CpuCluster::P));
        assert_eq!(classify_cpu_cluster("PCPU0"), Some(CpuCluster::P));
        assert_eq!(classify_cpu_cluster("ECPU"), Some(CpuCluster::EorM));
        assert_eq!(classify_cpu_cluster("ECPU0"), Some(CpuCluster::EorM));
        assert_eq!(classify_cpu_cluster("MCPU"), Some(CpuCluster::EorM));
        assert_eq!(classify_cpu_cluster("MCPU1"), Some(CpuCluster::EorM));
    }

    #[test]
    fn classify_strips_die_prefix() {
        assert_eq!(classify_cpu_cluster("DIE_0_PCPU0"), Some(CpuCluster::P));
        assert_eq!(classify_cpu_cluster("DIE_1_ECPU"), Some(CpuCluster::EorM));
        assert_eq!(classify_cpu_cluster("DIE_12_MCPU"), Some(CpuCluster::EorM));
    }

    #[test]
    fn classify_rejects_substring_matches() {
        // Must be prefix-anchored — no accidental routing for nearby names.
        assert_eq!(classify_cpu_cluster("APCPU"), None);
        assert_eq!(classify_cpu_cluster("XECPU"), None);
        assert_eq!(classify_cpu_cluster("GPU"), None);
        assert_eq!(classify_cpu_cluster(""), None);
    }

    #[test]
    fn classify_preserves_original_on_malformed_die_prefix() {
        // DIE_ without a numeric segment — not a DIE prefix, keep whole name.
        assert_eq!(classify_cpu_cluster("DIE_X_PCPU"), None);
        assert_eq!(classify_cpu_cluster("DIE_PCPU"), None);
    }
}
