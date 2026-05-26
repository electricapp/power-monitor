//! Criterion benchmarks for the public hot paths.
//!
//! Run: `cargo bench`.
//!
//! - **Hardware-dependent benches** (`Smc::*`, `MultiGroupSampler::*`) skip
//!   gracefully when the underlying service is unavailable (CI VMs, Docker).
//!   Criterion silently registers no benchmark in that case.
//! - **Pure benches** (`EnergyAccumulator::add`, `calc_utilization`) always
//!   run and are the right things to compare across releases — they're not
//!   affected by SoC variability or thermal state.
//!
//! The companion `tests/no_alloc.rs` integration test asserts the sample
//! path performs zero allocations after warm-up; these benches measure how
//! long that allocation-free path takes.

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use power_monitor::Metrics;
use power_monitor::ioreport::StateResidency;
use power_monitor::memory;
use power_monitor::sampler::calc_utilization;
use power_monitor::serialize::{AgentInfo, metrics_to_json, write_metrics_json};
use power_monitor::{EnergyAccumulator, EnergyReading, Smc};

// ── Hardware-dependent ────────────────────────────────────────────────────────

fn bench_smc(c: &mut Criterion) {
    let Ok(mut smc) = Smc::open() else {
        eprintln!("[bench] SMC unavailable — skipping Smc benches");
        return;
    };

    let mut group = c.benchmark_group("smc");
    // Each kernel call is the unit of work: PSTR, PHPC, ... 41 keys per snapshot.
    group.throughput(Throughput::Elements(41));
    group.bench_function("snapshot_41_keys", |b| b.iter(|| black_box(smc.snapshot())));
    group.throughput(Throughput::Elements(9));
    group.bench_function("read_power_9_rails", |b| {
        b.iter(|| black_box(smc.read_power()))
    });
    group.throughput(Throughput::Elements(11));
    group.bench_function("read_thermals_11_sensors", |b| {
        b.iter(|| black_box(smc.read_thermals()))
    });
    // Narrow paths used by the sampler — fewer syscalls is the only real win
    // here, since SMC kernel calls (~50µs) dominate any in-process compute.
    group.throughput(Throughput::Elements(1));
    group.bench_function("read_system_power_1_key", |b| {
        b.iter(|| black_box(smc.read_system_power()))
    });
    group.throughput(Throughput::Elements(2));
    group.bench_function("read_cpu_gpu_temps_2_keys", |b| {
        b.iter(|| black_box(smc.read_cpu_gpu_temps()))
    });
    group.finish();
}

// ── Pure transforms ───────────────────────────────────────────────────────────

fn bench_energy_accumulator(c: &mut Criterion) {
    let reading = EnergyReading {
        ecpu: 1.5,
        pcpu: 4.7,
        gpu: 0.8,
        ane: 0.1,
        dram: 0.6,
        interval_ms: 1000.0,
        ..Default::default()
    };

    c.bench_function("EnergyAccumulator::add", |b| {
        let mut acc = EnergyAccumulator::default();
        b.iter(|| {
            acc.add(black_box(&reading));
            black_box(&acc);
        });
    });
}

fn bench_memory(c: &mut Criterion) {
    // Steady-state: page_size + total_ram are OnceLock-cached, so this should
    // be one Mach call (host_statistics64) — not three syscalls.
    c.bench_function("memory::read_memory", |b| {
        // Prime the OnceLocks so the cold-path miss isn't measured.
        let _ = memory::read_memory();
        b.iter(|| black_box(memory::read_memory()));
    });
    c.bench_function("memory::read_swap", |b| {
        let _ = memory::read_swap();
        b.iter(|| black_box(memory::read_swap()));
    });
}

fn bench_calc_utilization(c: &mut Criterion) {
    // Realistic 8-state P-cluster residency histogram.
    let states: Vec<StateResidency> = ["IDLE", "DOWN", "P1", "P2", "P3", "P4", "P5", "P6"]
        .iter()
        .enumerate()
        .map(|(i, name)| StateResidency {
            name: (*name).into(),
            ticks: 1_000 * (i as u64 + 1),
        })
        .collect();
    let freqs = vec![972u32, 1308, 1704, 2100, 2496, 2892];

    c.bench_function("calc_utilization_8_states", |b| {
        b.iter(|| black_box(calc_utilization(black_box(&states), black_box(&freqs))));
    });
}

fn bench_serialize(c: &mut Criterion) {
    let m = Metrics::default();
    let info = AgentInfo {
        version: "0.1.0",
        hostname: "bench-host".to_string(),
        chip: "Apple M5 Max".to_string(),
        pcpu_cores: 12,
        ecpu_cores: 4,
        gpu_cores: 40,
        interval_ms: 1000,
    };

    // String-returning version: one alloc per call (~640 bytes).
    c.bench_function("metrics_to_json_alloc", |b| {
        b.iter(|| black_box(metrics_to_json(black_box(&m), black_box(&info))));
    });

    // Writer version into a reused buffer: zero allocs per call after warm-up.
    c.bench_function("write_metrics_json_reused_buf", |b| {
        let mut buf = String::with_capacity(640);
        b.iter(|| {
            buf.clear();
            write_metrics_json(&mut buf, black_box(&m), black_box(&info)).ok();
            black_box(buf.len());
        });
    });
}

// ── Criterion entry ───────────────────────────────────────────────────────────

criterion_group! {
    name = benches;
    // Tighter defaults — 41-call SMC snapshots take ~ms, so 3s of measurement
    // is plenty; pure benches converge in well under that.
    config = Criterion::default()
        .measurement_time(Duration::from_secs(3))
        .warm_up_time(Duration::from_secs(1));
    targets = bench_smc, bench_energy_accumulator, bench_calc_utilization, bench_memory, bench_serialize
}

criterion_main!(benches);
