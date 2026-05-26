# Changelog

All notable changes to this project will be documented in this file.

This project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0] — 2026-04-20

### Added

- **`Smc::read_per_core_cpu_temps`** — returns individual core temperatures
  (`Tp01`..`Tp0g`) as a `Vec<f32>`, trimmed to the number of cores present
  on this SKU. Backed by the new key-resolution cache, so repeated calls
  are cheap.
- **`EnergyAccumulator`** — folds a stream of `EnergyReading` samples into
  cumulative joules and time-weighted average watts. Saves callers from
  rolling their own multiply-and-sum bookkeeping for long-running totals.
- `Smc::read_f32` now caches key metadata internally — first call per key
  costs two kernel calls (resolve + read), subsequent calls cost one.
  Absent keys are cached as `None` so retry is also cheap.

### Changed

- `power-monitor serve` now spawns a thread per HTTP connection. A slow
  client can no longer stall the accept loop for other requests. (Matches
  the pattern `collect` already used.)
- `MultiGroupSampler::sample` triggers a full schema rebuild (and emits
  `SamplerEvent::SchemaChanged`) when a CPU/GPU Stats channel's P-state
  count drifts at runtime, not just when the channel count changes. Cluster
  P-state additions are picked up on the next sample instead of being
  silently truncated.
- `sampler.rs` cluster-name routing is now prefix-anchored (after stripping
  the Ultra / multi-die `DIE_N_` prefix). Channels like `"APCPU"` or
  `"XECPU"` can no longer accidentally route into a cluster accumulator.
- `Smc` no longer holds a reused `SmcKeyData` request buffer; each SMC
  read builds one on the stack. The micro-opt was saving a ~64 B memset
  that's unmeasurable against the kernel-call cost.

- **Fleet dashboard** (`power-monitor collect`) — new subcommand aggregates
  many `power-monitor serve` agents into a single HTML dashboard. Polls
  agents in parallel, serves an embedded dashboard page, and pushes updates
  to browsers via Server-Sent Events on `/stream`. Also exposes `/snapshot`
  for one-shot JSON and supports `--install` / `--uninstall` as a launchd
  user agent. Zero dependencies; raw TCP + hand-rolled HTTP.
- **`collect --tailnet`** — auto-discover agent hosts by shelling out to
  `tailscale status` and parsing the online peers. Mutually exclusive with
  `--host`. Adding a 26th Mac to the fleet is just `tailscale up` on the
  new machine plus a collector restart.
- **`collect /metrics`** — the collector now serves its own **aggregated**
  Prometheus text. A single scrape of `http://collector:8080/metrics`
  returns every host in the fleet, each line labeled with `chip="..."` and
  `host="..."`. Also emits a `power_monitor_host_up{target="..."}` liveness
  gauge for dead-agent alerting.
- **Ring buffer + sparklines** — the collector now keeps the last 40
  samples of SYS/CPU/GPU power per host and renders Unicode sparklines
  server-side (`▁▂▃▄▅▆▇█`). The dashboard tile grows three sparkline rows
  showing the trend. Zero extra agent load; sparklines are pre-rendered to
  avoid shipping raw history over the wire.
- `power-monitor serve --bind <addr>` — bind the agent to a non-localhost
  address (e.g. `0.0.0.0` for LAN fleet deployments). Default remains
  `127.0.0.1` to preserve existing behaviour.
- `power-monitor serve --auth <token>` — require
  `Authorization: Bearer <token>` on all agent requests. Collector mirrors
  this with its own `--auth` flag, forwarding the token to every poll.
- `/json` payload now includes `hostname` (from `gethostname`) and
  `interval_ms` (the sampler window). The dashboard uses `hostname` to
  label tiles and `interval_ms` to tune the stale-tile threshold.
- Prometheus metrics now carry a `host="..."` label alongside the existing
  `chip="..."` label.
- Shared `src/http.rs` module for the no-deps HTTP helpers
  (`http_response`, `extract_path`, `extract_bearer`).
- Shared `src/sys.rs` module for the `hostname()` helper (was duplicated
  in `main.rs`).
- Integration test `tests/collector_http.rs` spins up a mock agent, runs
  the real `collect` binary against it, and exercises `/snapshot`, `/` and
  auth enforcement end-to-end.
- `ChannelGroup` enum (`EnergyModel`, `CpuStats`, `GpuStats`, `Other`) replaces the
  stringly-typed `group`/`subgroup` fields on `ChannelSample`. Byte-level parsing
  against a stack buffer — no heap allocation.
- `EnergyUnit` enum (`MilliJoule`, `MicroJoule`, `NanoJoule`) with `to_joules`,
  `parse` (strict), and `from_bytes` (lenient) constructors.
- `MultiGroupSampler::channels()` — borrow the cached scratch buffer after `sample()`.
- `set_event_hook` / `SamplerEvent` — diagnostic event hook for schema changes and
  null delta dictionaries. Zero-cost (one relaxed atomic load) when no hook is
  registered.
- `BatteryReading::power_watts` — instantaneous watts (positive = discharging).
- `BatteryReading::time_to_empty_hours` / `time_to_full_hours` — runtime estimates
  based on current draw and remaining capacity.
- Integration test (`tests/no_alloc.rs`) using a counting global allocator to
  assert `MultiGroupSampler::sample()` is zero-allocation after warm-up.

### Changed

- **Breaking:** `MultiGroupSampler::sample()` now returns `f32` (interval ms only).
  Access the channel list via `MultiGroupSampler::channels()`.
- **Breaking:** `ChannelSample` dropped its `subgroup: String` field (never
  consumed internally) and replaced `group: String` with `ChannelGroup`.
- **Breaking:** `ChannelSample.unit` is now `EnergyUnit`, not `String`.
- `Sampler::get_metrics` now reads SMC thermals, SMC system power, memory, and
  swap **once per window** instead of per sub-sample. Sub-sampling is reserved
  for IOReport data (which actually benefits from smoothing).

### Performance

- After a single warm-up call, `MultiGroupSampler::sample()` performs **zero heap
  allocations**. Channel names, state names, units, and group discriminants are
  cached in a persistent scratch buffer; subsequent samples overwrite only
  `Energy` values and `StateResidency::ticks` in place.
- Per-sample sampler allocations dropped from ~800/sec to 0 (measured on M5 at
  4 Hz sub-sampling) via:
  - Cached CFString keys (`"IOReportChannels"`, `"IOReportChannelUnit"`) in both
    `EnergySampler` and `MultiGroupSampler`.
  - Reusable `Vec<ChannelSample>` scratch buffer in `MultiGroupSampler`.
  - State names cached after the first sample; only ticks refresh thereafter.
  - Stack-allocated byte buffers for CFString → `&[u8]` reads (no `String`
    round-trip).
  - Per-cluster state accumulator scratch (`ecpu_scratch` / `pcpu_scratch` /
    `gpu_scratch`) moved into `Sampler` fields and reused across calls.
- `memory.rs`: replaced `CString::new("hw.pagesize").unwrap()` pattern with
  `c"..."` literals, removing three allocations from the sample path.

## [0.1.0] -- 2026-01-01

Initial release.

### Added

- `Smc` -- direct FFI to AppleSMC via IOKit; reads 41 pre-resolved keys covering power rails, 11 temperature sensors, fans (up to 2), voltage rails, current sensors, and battery state.
- `EnergySampler` -- IOReport `Energy Model` subscription; converts cumulative energy counter deltas to per-component watts (ECPU, PCPU, GPU, ANE, DRAM, AMCC, DCS, ISP, AVE, DISP).
- `MultiGroupSampler` -- combined IOReport subscription to Energy Model + CPU Stats + GPU Stats; exposes raw `ChannelSample` / `ChannelKind` for full consumer control.
- `Sampler` -- high-level aggregator; calls `MultiGroupSampler`, `Smc`, and memory reads in a configurable time window, averages `N_SAMPLES` sub-windows, returns a single `Metrics` snapshot.
- `SocInfo` -- reads chip name, E/P-cluster and GPU core counts, and CPU/GPU frequency tables from sysctl and the IOKit registry.
- `read_memory` / `read_swap` -- physical RAM (total, used, available, wired, compressed) and swap file statistics via Mach `host_statistics64` and `vm.swapusage` sysctl.
- `calc_utilization` -- public helper to convert IOReport P-state residency histograms into weighted average frequency and utilisation fraction.
- Zero subprocesses, no `sudo` required, no third-party runtime dependencies.
