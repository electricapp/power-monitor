//! Proves that [`power_monitor::MultiGroupSampler::sample`] performs **zero
//! heap allocations** after the first (warm-up) call.
//!
//! Mechanism: install a custom global allocator that increments an atomic
//! counter while a flag is set, warm up the sampler, then measure a single
//! steady-state sample.
//!
//! Skipped gracefully when IOReport is unavailable (CI VMs, containers).

#![cfg(target_os = "macos")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);

/// `cargo test` runs `#[test]` functions concurrently by default, but the
/// COUNTING / ALLOCS atomics are process-global. Serialize the two tests
/// here to keep their measurement windows from interleaving.
static MEASURE_LOCK: Mutex<()> = Mutex::new(());

struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[test]
fn full_sampler_get_metrics_is_zero_alloc_after_warmup() {
    use power_monitor::Sampler;
    let _guard = MEASURE_LOCK.lock().unwrap();

    let Some(mut sampler) = Sampler::with_samples_per_window(1) else {
        eprintln!("Sampler unavailable — skipping zero-alloc assertion");
        return;
    };

    // Warm-up: builds IOReport schema, classifies channels, resolves SMC keys,
    // initializes OnceLocks for page_size / total_ram. Two passes for the
    // refresh-in-place path.
    let _ = sampler.get_metrics(50);
    let _ = sampler.get_metrics(50);

    ALLOCS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    let m = sampler.get_metrics(50);
    std::hint::black_box(&m);
    COUNTING.store(false, Ordering::Relaxed);

    let count = ALLOCS.load(Ordering::Relaxed);
    assert_eq!(
        count, 0,
        "Sampler::get_metrics() allocated {count} times after warm-up"
    );
}

#[test]
fn write_metrics_json_is_zero_alloc_into_reused_buffer() {
    // The `pipe`/`serve` per-tick loops claim to serialize with zero per-frame
    // allocations into a reused buffer. The sampler-level tests don't cover the
    // serializer, so verify it directly — no hardware needed.
    use power_monitor::Metrics;
    use power_monitor::serialize::{AgentInfo, write_metrics_json};

    let _guard = MEASURE_LOCK.lock().unwrap();

    let info = AgentInfo {
        version: "0.1.0",
        hostname: "testhost".to_string(),
        chip: "Apple M-test".to_string(),
        pcpu_cores: 4,
        ecpu_cores: 4,
        gpu_cores: 10,
        interval_ms: 1000,
    };
    let m = Metrics::default();

    // Warm-up writes grow the buffer to its steady-state capacity; `clear()`
    // keeps that capacity so the measured write reuses the same allocation.
    let mut buf = String::with_capacity(1024);
    write_metrics_json(&mut buf, &m, &info).unwrap();
    buf.clear();

    ALLOCS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    write_metrics_json(&mut buf, &m, &info).unwrap();
    std::hint::black_box(&buf);
    COUNTING.store(false, Ordering::Relaxed);

    let count = ALLOCS.load(Ordering::Relaxed);
    assert_eq!(
        count, 0,
        "write_metrics_json() allocated {count} times into a pre-grown buffer"
    );
}

#[test]
fn multigroup_sampler_is_zero_alloc_after_warmup() {
    let _guard = MEASURE_LOCK.lock().unwrap();
    // Skip gracefully when IOReport is unavailable (e.g. inside a VM).
    let Some(mut sampler) = power_monitor::MultiGroupSampler::new() else {
        eprintln!("IOReport unavailable — skipping zero-alloc assertion");
        return;
    };

    // Warm-up 1 — populates the channel schema + scratch buffer (this call
    // DOES allocate, and is deliberately excluded from the measurement).
    thread::sleep(Duration::from_millis(50));
    let _ = sampler.sample();
    let _ = sampler.channels();

    // Warm-up 2 — exercises the refresh-in-place path once, to trip any
    // lazy allocation hiding behind a once-cell or similar.
    thread::sleep(Duration::from_millis(50));
    let _ = sampler.sample();
    let _ = sampler.channels();

    // Measured sample.
    thread::sleep(Duration::from_millis(50));
    ALLOCS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    let _interval = sampler.sample();
    let channels = sampler.channels();
    // Force use of the return value so the optimiser can't elide the call.
    std::hint::black_box(channels.len());
    COUNTING.store(false, Ordering::Relaxed);

    let count = ALLOCS.load(Ordering::Relaxed);
    assert_eq!(
        count, 0,
        "MultiGroupSampler::sample() allocated {count} times after warm-up"
    );
}
