//! `power-monitor collect` — aggregate many `power-monitor serve` agents into
//! one fleet dashboard.
//!
//! Architecture: one thread per host does blocking HTTP polls, updating a
//! shared `Vec<HostState>` behind a `Mutex`. Each poll parses the agent's
//! `/json` payload into a typed [`ParsedSample`] and pushes the delta into a
//! bounded ring-buffer [`History`] used for sparkline rendering.
//!
//! The HTTP server serves:
//!   GET /            — embedded dashboard HTML
//!   GET /stream      — Server-Sent Events stream (fleet snapshot)
//!   GET /snapshot    — one-shot JSON snapshot
//!   GET /metrics     — **aggregated** Prometheus text (all hosts, one scrape)
//!
//! No third-party dependencies; follows the raw-TCP HTTP pattern from
//! `serve_cmd.rs`.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::process::Command;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::http::{
    MAX_INFLIGHT, extract_bearer, extract_path, http_response, read_request_head, try_acquire,
};
use power_monitor::Metrics;

const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// Sparkline width — matches `BAR_W` in `src/main.rs` so the dashboard's
/// history view can swap the bar for a sparkline without perturbing the
/// surrounding layout (TUI parity preserved).
const SPARK_WIDTH: usize = 24;

/// Number of historical samples kept per metric per host. Matches
/// [`SPARK_WIDTH`]: exactly enough to fill one sparkline.
const HISTORY_LEN: usize = SPARK_WIDTH;

/// Unicode block characters used to rasterize sparklines (low → high).
const SPARK_CHARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Per-metric sparkline scaling caps — match the TUI `power_row` gauges in
/// `src/main.rs`. Values above the cap saturate to the top bucket.
const SPARK_SYS_MAX: f32 = 40.0;
const SPARK_CPU_MAX: f32 = 20.0;
const SPARK_GPU_MAX: f32 = 16.0;
const SPARK_ANE_MAX: f32 = 8.0;
const SPARK_DRAM_MAX: f32 = 5.0;

// ── Field parsing ─────────────────────────────────────────────────────────────

/// Visit each `"key":value` pair in a flat JSON object exactly once.
///
/// `value` is the raw slice — quoted string contents (without the surrounding
/// quotes) for string fields, the literal `"null"` for null, or the digit
/// sequence for numbers. No escape decoding; agent payloads are ASCII.
fn walk_flat_object<'a>(json: &'a str, mut visit: impl FnMut(&'a str, &'a str)) {
    let bytes = json.as_bytes();
    let mut i = 0;
    let n = bytes.len();
    while i < n {
        // Find next `"key"`
        while i < n && bytes[i] != b'"' {
            i += 1;
        }
        if i >= n {
            break;
        }
        let key_start = i + 1;
        i += 1;
        while i < n && bytes[i] != b'"' {
            i += 1;
        }
        if i >= n {
            break;
        }
        let key_end = i;
        i += 1;
        // Expect `:` (after optional whitespace).
        while i < n && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= n || bytes[i] != b':' {
            continue;
        }
        i += 1;
        while i < n && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= n {
            break;
        }
        let key = &json[key_start..key_end];
        let value: &str = if bytes[i] == b'"' {
            let s = i + 1;
            i += 1;
            while i < n && bytes[i] != b'"' {
                i += 1;
            }
            let e = i;
            if i < n {
                i += 1;
            }
            &json[s..e]
        } else {
            let s = i;
            while i < n && !matches!(bytes[i], b',' | b'}' | b' ' | b'\t' | b'\r' | b'\n' | b']') {
                i += 1;
            }
            &json[s..i]
        };
        visit(key, value);
    }
}

// ── ParsedSample ──────────────────────────────────────────────────────────────

/// Typed view of one agent `/json` payload. Fed into the ring buffer, used
/// to render sparklines, and exported to Prometheus.
#[derive(Debug, Clone, Default)]
struct ParsedSample {
    chip: String,
    hostname: String,
    sys_power: f32,
    cpu_power: f32,
    gpu_power: f32,
    ane_power: f32,
    dram_power: f32,
    ecpu_util: f32,
    pcpu_util: f32,
    gpu_util: f32,
    ecpu_freq_mhz: u32,
    pcpu_freq_mhz: u32,
    gpu_freq_mhz: u32,
    cpu_temp_c: f32,
    gpu_temp_c: f32,
    fan_rpm: u32,
    fan_max_rpm: u32,
    memory_used_bytes: u64,
    memory_total_bytes: u64,
    swap_used_bytes: u64,
    swap_total_bytes: u64,
}

fn parse_sample(json: &str) -> ParsedSample {
    let mut s = ParsedSample {
        cpu_temp_c: f32::NAN,
        gpu_temp_c: f32::NAN,
        ..Default::default()
    };
    walk_flat_object(json, |key, value| match key {
        "chip" => s.chip = value.to_string(),
        "hostname" => s.hostname = value.to_string(),
        "sys_power" => s.sys_power = value.parse().unwrap_or(0.0),
        "cpu_power" => s.cpu_power = value.parse().unwrap_or(0.0),
        "gpu_power" => s.gpu_power = value.parse().unwrap_or(0.0),
        "ane_power" => s.ane_power = value.parse().unwrap_or(0.0),
        "dram_power" => s.dram_power = value.parse().unwrap_or(0.0),
        "ecpu_util" => s.ecpu_util = value.parse().unwrap_or(0.0),
        "pcpu_util" => s.pcpu_util = value.parse().unwrap_or(0.0),
        "gpu_util" => s.gpu_util = value.parse().unwrap_or(0.0),
        "ecpu_freq_mhz" => s.ecpu_freq_mhz = value.parse().unwrap_or(0),
        "pcpu_freq_mhz" => s.pcpu_freq_mhz = value.parse().unwrap_or(0),
        "gpu_freq_mhz" => s.gpu_freq_mhz = value.parse().unwrap_or(0),
        "cpu_temp_c" => s.cpu_temp_c = value.parse().unwrap_or(f32::NAN),
        "gpu_temp_c" => s.gpu_temp_c = value.parse().unwrap_or(f32::NAN),
        "fan_rpm" => s.fan_rpm = value.parse().unwrap_or(0),
        "fan_max_rpm" => s.fan_max_rpm = value.parse().unwrap_or(0),
        "memory_used_bytes" => s.memory_used_bytes = value.parse().unwrap_or(0),
        "memory_total_bytes" => s.memory_total_bytes = value.parse().unwrap_or(0),
        "swap_used_bytes" => s.swap_used_bytes = value.parse().unwrap_or(0),
        "swap_total_bytes" => s.swap_total_bytes = value.parse().unwrap_or(0),
        _ => {}
    });
    s
}

// ── History ring buffer ───────────────────────────────────────────────────────

/// Per-host ring buffer of recent metric values. Capacity = [`HISTORY_LEN`].
///
/// Feeds the history-view dashboard: one ring buffer per metric that
/// appears in the live tile bar column (power, utilisation, and memory).
#[derive(Debug, Clone, Default)]
struct History {
    sys_power: VecDeque<f32>,
    cpu_power: VecDeque<f32>,
    gpu_power: VecDeque<f32>,
    ane_power: VecDeque<f32>,
    dram_power: VecDeque<f32>,
    pcpu_util: VecDeque<f32>,
    ecpu_util: VecDeque<f32>,
    gpu_util: VecDeque<f32>,
    /// Fan duty fraction in `0.0..=1.0`.
    fan_duty: VecDeque<f32>,
    /// Memory used fraction in `0.0..=1.0`.
    mem_frac: VecDeque<f32>,
    /// Swap used fraction in `0.0..=1.0`.
    swap_frac: VecDeque<f32>,
}

impl History {
    fn push(&mut self, s: &ParsedSample) {
        push_bounded(&mut self.sys_power, s.sys_power);
        push_bounded(&mut self.cpu_power, s.cpu_power);
        push_bounded(&mut self.gpu_power, s.gpu_power);
        push_bounded(&mut self.ane_power, s.ane_power);
        push_bounded(&mut self.dram_power, s.dram_power);
        push_bounded(&mut self.pcpu_util, s.pcpu_util);
        push_bounded(&mut self.ecpu_util, s.ecpu_util);
        push_bounded(&mut self.gpu_util, s.gpu_util);
        let fan_duty = if s.fan_max_rpm > 0 {
            (s.fan_rpm as f32 / s.fan_max_rpm as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        push_bounded(&mut self.fan_duty, fan_duty);
        let mem_frac = if s.memory_total_bytes > 0 {
            s.memory_used_bytes as f32 / s.memory_total_bytes as f32
        } else {
            0.0
        };
        let swap_frac = if s.swap_total_bytes > 0 {
            s.swap_used_bytes as f32 / s.swap_total_bytes as f32
        } else {
            0.0
        };
        push_bounded(&mut self.mem_frac, mem_frac);
        push_bounded(&mut self.swap_frac, swap_frac);
    }
}

#[inline]
fn push_bounded(v: &mut VecDeque<f32>, val: f32) {
    if v.len() >= HISTORY_LEN {
        v.pop_front();
    }
    v.push_back(val);
}

// ── Sparkline rasterizer ──────────────────────────────────────────────────────

/// Render a bounded history buffer as a fixed-width Unicode sparkline.
///
/// Shows the **most recent** `width` samples. Values are normalised against
/// `scale_max` (`0.0..=scale_max` → `▁..█`). When the buffer has fewer
/// entries than `width`, the line is left-padded with spaces so it grows
/// from the right as samples accumulate.
/// Test-only convenience wrapper around [`write_sparkline`] — the production
/// code paths all stream into a reused buffer.
#[cfg(test)]
fn sparkline(values: &VecDeque<f32>, width: usize, scale_max: f32) -> String {
    let mut out = String::with_capacity(width * 3);
    write_sparkline(&mut out, values, width, scale_max);
    out
}

/// Write a sparkline into a borrowed buffer — no intermediate `String`.
/// Used by `build_snapshot_into` to keep per-snapshot allocations at 0.
fn write_sparkline(out: &mut String, values: &VecDeque<f32>, width: usize, scale_max: f32) {
    let n = values.len();
    let start = n.saturating_sub(width);
    let padding = width.saturating_sub(n);
    for _ in 0..padding {
        out.push(' ');
    }
    for &val in values.iter().skip(start) {
        let frac = if scale_max > 0.0 {
            (val / scale_max).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let bucket = ((frac * 8.0) as usize).min(7);
        out.push(SPARK_CHARS[bucket]);
    }
}

// ── Host state ────────────────────────────────────────────────────────────────

/// Cached state for one remote agent.
#[derive(Debug, Clone)]
struct HostState {
    /// `host[:port]` target passed on the command line or discovered via Tailscale.
    target: String,
    /// Short label derived from `target` (host without port).
    label: String,
    /// Most recent `/json` response body, verbatim. Empty until first success.
    last_json: String,
    /// Parsed numeric view of `last_json`, or `None` before first success.
    parsed: Option<ParsedSample>,
    /// Ring buffer of recent samples for sparkline rendering.
    history: History,
    /// Wall time of last successful poll. `None` until the first success.
    last_ok_at: Option<Instant>,
    /// Last error message from a failed poll, if any.
    last_err: Option<String>,
}

impl HostState {
    fn new(target: String) -> Self {
        let label = target.split(':').next().unwrap_or(&target).to_string();
        HostState {
            target,
            label,
            last_json: String::new(),
            parsed: None,
            history: History::default(),
            last_ok_at: None,
            last_err: None,
        }
    }
}

/// Per-slot fine-grained locking. Each poller owns its slot's `Mutex` and only
/// ever contends with readers of that slot. The outer `Arc<[...]>` is shared
/// immutably; no global lock guards iteration.
///
/// Not strictly lock-free — `HostState` carries a `String` (last_json) and
/// `VecDeque`s (history) which can't be `Copy`-published through a seqlock.
/// Per-slot mutexes are the right shape: one writer + one or two readers per
/// slot, held for ~10s of microseconds.
type Fleet = Arc<[Mutex<HostState>]>;

// ── Host spec parsing ────────────────────────────────────────────────────────

/// Parse a comma-separated host list into `host[:port]` targets.
pub(crate) fn parse_hosts(list: &str, default_port: u16) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|raw| {
            let s = raw
                .strip_prefix("https://")
                .or_else(|| raw.strip_prefix("http://"))
                .unwrap_or(raw);
            let s = s.split('/').next().unwrap_or(s);
            if s.contains(':') {
                s.to_string()
            } else {
                format!("{s}:{default_port}")
            }
        })
        .collect()
}

// ── Tailnet discovery ────────────────────────────────────────────────────────

/// Shell out to `tailscale status` and return the online peer hostnames.
///
/// Uses the plain tabular output so we don't need a JSON parser. Skips any
/// line marked `offline` and any line whose first column isn't a Tailscale
/// CGNAT IP (100.64.0.0/10) or ULA (fd7a:115c:a1e0::/48).
fn discover_tailnet() -> Result<Vec<String>, String> {
    let output = Command::new("tailscale")
        .arg("status")
        .output()
        .map_err(|e| {
            format!("failed to exec 'tailscale status': {e}. Is Tailscale installed and in PATH?")
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "tailscale status exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|e| format!("tailscale output not UTF-8: {e}"))?;
    Ok(parse_tailnet_status(stdout))
}

fn parse_tailnet_status(stdout: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let ip = parts[0];
        if !ip.starts_with("100.") && !ip.starts_with("fd7a:") {
            continue;
        }
        if trimmed.to_lowercase().contains("offline") {
            continue;
        }
        let hostname = parts[1];
        if hostname.is_empty() || hostname == "-" {
            continue;
        }
        hosts.push(hostname.to_string());
    }
    hosts
}

// ── Polling ──────────────────────────────────────────────────────────────────

fn resolve_target(target: &str) -> Result<SocketAddr, String> {
    target
        .to_socket_addrs()
        .map_err(|e| format!("resolve {target}: {e}"))?
        .next()
        .ok_or_else(|| format!("resolve {target}: no addresses"))
}

fn fetch_json(
    target: &str,
    addr: SocketAddr,
    auth: Option<&str>,
    timeout: Duration,
) -> Result<String, String> {
    let mut stream =
        TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("connect {target}: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| format!("set_write_timeout: {e}"))?;

    let mut req = format!(
        "GET /json HTTP/1.1\r\n\
         Host: {target}\r\n\
         User-Agent: power-monitor-collect/0.1\r\n\
         Connection: close\r\n"
    );
    if let Some(token) = auth {
        req.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    req.push_str("\r\n");

    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write {target}: {e}"))?;

    let mut raw = Vec::with_capacity(4096);
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("read {target}: {e}"))?;

    let text = std::str::from_utf8(&raw).map_err(|e| format!("utf8 {target}: {e}"))?;
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("malformed response from {target}"))?;

    let status_line = head.lines().next().unwrap_or("");
    if status_line.split_whitespace().nth(1) != Some("200") {
        return Err(format!("{target}: {status_line}"));
    }

    Ok(body.to_string())
}

fn spawn_pollers(fleet: Fleet, interval: Duration, auth: Option<String>) {
    for idx in 0..fleet.len() {
        let fleet = Arc::clone(&fleet);
        let auth = auth.clone();
        thread::spawn(move || {
            // `target` is set at creation and never mutated — cache it once
            // and keep the per-iteration lock window to just the update.
            let target = match fleet[idx].lock() {
                Ok(h) => h.target.clone(),
                Err(_) => return,
            };
            let timeout = interval.saturating_mul(2);
            // Cache the resolved address; re-resolve only on failure.
            let mut addr_cache: Option<SocketAddr> = None;
            loop {
                let result = match addr_cache {
                    Some(addr) => fetch_json(&target, addr, auth.as_deref(), timeout),
                    None => match resolve_target(&target) {
                        Ok(addr) => {
                            addr_cache = Some(addr);
                            fetch_json(&target, addr, auth.as_deref(), timeout)
                        }
                        Err(e) => Err(e),
                    },
                };
                if let Ok(mut h) = fleet[idx].lock() {
                    match result {
                        Ok(body) => {
                            let parsed = parse_sample(&body);
                            h.history.push(&parsed);
                            h.parsed = Some(parsed);
                            h.last_json = body;
                            h.last_ok_at = Some(Instant::now());
                            h.last_err = None;
                        }
                        Err(e) => {
                            addr_cache = None;
                            h.last_err = Some(e);
                        }
                    }
                }
                thread::sleep(interval);
            }
        });
    }
}

// ── Snapshot assembly ────────────────────────────────────────────────────────

/// Test-only convenience wrapper around [`build_snapshot_into`].
#[cfg(test)]
fn build_snapshot(fleet: &[Mutex<HostState>], poll_interval_ms: u64) -> String {
    let mut out = String::with_capacity(8192);
    build_snapshot_into(&mut out, fleet, poll_interval_ms);
    out
}

/// Build the fleet snapshot directly into a borrowed buffer.
///
/// Used by the SSE push loop to reuse one buffer across iterations — the
/// inner host blocks, sparklines, and escape sequences all stream into the
/// same allocation, dropping per-snapshot allocations from ~10×N hosts to 0.
fn build_snapshot_into(out: &mut String, fleet: &[Mutex<HostState>], poll_interval_ms: u64) {
    out.push_str("{\"generated_at\":\"");
    let _ = power_monitor::serialize::write_utc_now(out);
    let _ = write!(
        out,
        "\",\"poll_interval_ms\":{poll_interval_ms},\"history_len\":{HISTORY_LEN},\"hosts\":["
    );

    for (i, slot) in fleet.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        // Hold the per-slot lock for the duration of this host's section.
        // The poller for this slot only blocks for as long as one host's
        // formatting takes, not the whole fleet.
        let Ok(h) = slot.lock() else { continue };
        write_host_block(out, &h);
    }

    out.push_str("]}");
}

/// Format one host's JSON section into `out`. Caller holds the slot lock.
fn write_host_block(out: &mut String, h: &HostState) {
    let stale_s = h
        .last_ok_at
        .map(|t| t.elapsed().as_secs_f32())
        .unwrap_or(-1.0);
    let ok = h.last_ok_at.is_some() && !h.last_json.is_empty();

    out.push_str("{\"label\":\"");
    write_escape_json(out, &h.label);
    out.push_str("\",\"target\":\"");
    write_escape_json(out, &h.target);
    out.push_str("\",\"ok\":");
    out.push_str(if ok { "true" } else { "false" });
    out.push_str(",\"stale_s\":");
    if stale_s < 0.0 {
        out.push_str("null");
    } else {
        let _ = write!(out, "{stale_s:.2}");
    }
    out.push_str(",\"error\":");
    match &h.last_err {
        Some(e) => {
            out.push('"');
            write_escape_json(out, e);
            out.push('"');
        }
        None => out.push_str("null"),
    }
    out.push_str(",\"metrics\":");
    if ok {
        out.push_str(h.last_json.trim());
    } else {
        out.push_str("null");
    }
    // Sparklines: pre-rendered Unicode strings, one per metric that has a
    // bar in the live tile. The dashboard's history mode swaps each bar
    // for the matching sparkline without touching the rest of the layout.
    out.push_str(",\"spark\":{\"sys\":\"");
    write_sparkline(out, &h.history.sys_power, SPARK_WIDTH, SPARK_SYS_MAX);
    out.push_str("\",\"cpu\":\"");
    write_sparkline(out, &h.history.cpu_power, SPARK_WIDTH, SPARK_CPU_MAX);
    out.push_str("\",\"gpu\":\"");
    write_sparkline(out, &h.history.gpu_power, SPARK_WIDTH, SPARK_GPU_MAX);
    out.push_str("\",\"ane\":\"");
    write_sparkline(out, &h.history.ane_power, SPARK_WIDTH, SPARK_ANE_MAX);
    out.push_str("\",\"dram\":\"");
    write_sparkline(out, &h.history.dram_power, SPARK_WIDTH, SPARK_DRAM_MAX);
    out.push_str("\",\"pcpu_util\":\"");
    write_sparkline(out, &h.history.pcpu_util, SPARK_WIDTH, 1.0);
    out.push_str("\",\"ecpu_util\":\"");
    write_sparkline(out, &h.history.ecpu_util, SPARK_WIDTH, 1.0);
    out.push_str("\",\"gpu_util\":\"");
    write_sparkline(out, &h.history.gpu_util, SPARK_WIDTH, 1.0);
    out.push_str("\",\"fan\":\"");
    write_sparkline(out, &h.history.fan_duty, SPARK_WIDTH, 1.0);
    out.push_str("\",\"mem\":\"");
    write_sparkline(out, &h.history.mem_frac, SPARK_WIDTH, 1.0);
    out.push_str("\",\"swap\":\"");
    write_sparkline(out, &h.history.swap_frac, SPARK_WIDTH, 1.0);
    out.push_str("\"}");
    out.push('}');
}

/// Test-only convenience wrapper around [`write_escape_json`].
#[cfg(test)]
fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let _ = power_monitor::serialize::write_escape_json(&mut out, s);
    out
}

fn write_escape_json(out: &mut String, s: &str) {
    let _ = power_monitor::serialize::write_escape_json(out, s);
}

// ── Prometheus aggregation ───────────────────────────────────────────────────

/// Inflate a flat [`ParsedSample`] back into a [`Metrics`] so the shared
/// [`power_monitor::serialize::PROM_GAUGES`] catalog can read it. `Metrics` is
/// `#[non_exhaustive]`, so it's built field-by-field from `Default` rather than
/// a struct literal. Fields not carried over the wire (`all_power`,
/// `interval_ms`) stay at their defaults — they aren't exported gauges.
fn parsed_to_metrics(p: &ParsedSample) -> Metrics {
    let mut m = Metrics::default();
    m.sys_power = p.sys_power;
    m.cpu_power = p.cpu_power;
    m.gpu_power = p.gpu_power;
    m.ane_power = p.ane_power;
    m.dram_power = p.dram_power;
    m.ecpu.utilization = p.ecpu_util;
    m.ecpu.freq_mhz = p.ecpu_freq_mhz;
    m.pcpu.utilization = p.pcpu_util;
    m.pcpu.freq_mhz = p.pcpu_freq_mhz;
    m.gpu_util = p.gpu_util;
    m.gpu_freq_mhz = p.gpu_freq_mhz;
    m.cpu_temp = p.cpu_temp_c;
    m.gpu_temp = p.gpu_temp_c;
    m.fan_rpm = p.fan_rpm;
    m.fan_max_rpm = p.fan_max_rpm;
    m.memory.used = p.memory_used_bytes;
    m.memory.total = p.memory_total_bytes;
    m.swap.used = p.swap_used_bytes;
    m.swap.total = p.swap_total_bytes;
    m
}

/// Render the full fleet state as Prometheus exposition text.
///
/// A single scrape of `/metrics` on the collector replaces scraping every
/// agent individually — each host gets `chip="..."` and `host="..."` labels
/// on every gauge. A `power_monitor_host_up` gauge is emitted for every
/// configured target (ok or not) so Prometheus alerts can catch dead agents.
fn fleet_to_prometheus(fleet: &[Mutex<HostState>]) -> String {
    let mut out = String::with_capacity(16_384);

    /// Liveness fact pulled out of `HostState` while the slot lock is held —
    /// `Instant` doesn't survive across the lock release for re-evaluation.
    struct Live {
        target: String,
        label: String,
        up: bool,
        parsed: Option<ParsedSample>,
    }

    // Take one short lock per slot, copy out exactly what we need, then
    // format with no locks held. Cheaper than holding 17 gauges' worth of
    // formatting inside the lock; correctness-equivalent because Prometheus
    // doesn't require cross-host atomicity.
    let snapshot: Vec<Live> = fleet
        .iter()
        .filter_map(|m| {
            let h = m.lock().ok()?;
            Some(Live {
                target: h.target.clone(),
                label: h.label.clone(),
                up: matches!(h.last_ok_at, Some(t) if t.elapsed().as_secs() < 10),
                parsed: h.parsed.clone(),
            })
        })
        .collect();

    // Render each labelled host's metrics once (ParsedSample → Metrics) so the
    // shared PROM_GAUGES catalog drives both the single-host (`serve`) and fleet
    // exporters from one source of truth. Hosts whose first poll hasn't yet
    // populated chip/host labels are skipped (the label race the old per-gauge
    // guard handled).
    let rendered: Vec<(&str, &str, Metrics)> = snapshot
        .iter()
        .filter_map(|l| {
            let p = l.parsed.as_ref()?;
            if p.chip.is_empty() || p.hostname.is_empty() {
                return None;
            }
            Some((p.chip.as_str(), p.hostname.as_str(), parsed_to_metrics(p)))
        })
        .collect();

    for &(name, help, value) in power_monitor::serialize::PROM_GAUGES {
        let _ = write!(
            out,
            "# HELP power_monitor_{name} {help}\n# TYPE power_monitor_{name} gauge\n"
        );
        for (chip, host, m) in &rendered {
            let v = value(m);
            if v.is_nan() {
                continue;
            }
            let _ = writeln!(
                out,
                "power_monitor_{name}{{chip=\"{chip}\",host=\"{host}\"}} {v:.3}"
            );
        }
    }

    // Liveness: 1 if the agent responded within ~10 intervals, 0 otherwise.
    // Emitted for every configured target, including ones that have never
    // responded, so Prometheus alerts can catch dead agents.
    out.push_str("# HELP power_monitor_host_up 1 if the agent responded to the most recent poll within 10 seconds\n");
    out.push_str("# TYPE power_monitor_host_up gauge\n");
    for live in &snapshot {
        let chip = live.parsed.as_ref().map(|p| p.chip.as_str()).unwrap_or("");
        let host = live
            .parsed
            .as_ref()
            .map(|p| p.hostname.as_str())
            .unwrap_or(&live.label);
        let _ = writeln!(
            out,
            "power_monitor_host_up{{chip=\"{}\",host=\"{}\",target=\"{}\"}} {}",
            chip,
            host,
            live.target,
            u8::from(live.up)
        );
    }

    out
}

// ── HTTP / SSE server ────────────────────────────────────────────────────────

fn serve_stream(
    mut stream: TcpStream,
    fleet: Fleet,
    poll_interval_ms: u64,
    push_interval: Duration,
) {
    let headers = "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache\r\n\
         Connection: keep-alive\r\n\
         Access-Control-Allow-Origin: *\r\n\
         \r\n";
    if stream.write_all(headers.as_bytes()).is_err() {
        return;
    }

    // Reused per push: SSE prefix + snapshot + suffix all stream into the
    // same allocation so the steady-state push loop is heap-traffic-free.
    let mut buf = String::with_capacity(8192);
    loop {
        buf.clear();
        buf.push_str("data: ");
        build_snapshot_into(&mut buf, &fleet, poll_interval_ms);
        buf.push_str("\n\n");
        if stream.write_all(buf.as_bytes()).is_err() {
            return;
        }
        if stream.flush().is_err() {
            return;
        }
        thread::sleep(push_interval);
    }
}

fn handle_connection(
    mut stream: TcpStream,
    fleet: Fleet,
    poll_interval_ms: u64,
    push_interval: Duration,
    auth: Option<&str>,
) {
    let buf = read_request_head(&mut stream);
    if buf.is_empty() {
        return;
    }

    if let Some(required) = auth {
        let presented = extract_bearer(&buf);
        if presented != Some(required) {
            let body = "{\"error\":\"unauthorized\"}";
            let response = http_response("401 Unauthorized", "application/json", body);
            stream.write_all(&response).ok();
            return;
        }
    }

    let path = extract_path(&buf).unwrap_or_default();
    // Strip the query string so `/?mode=history` still matches `/`.
    let route = path.split('?').next().unwrap_or("/");

    match route {
        "/" | "/index.html" => {
            let response = http_response("200 OK", "text/html; charset=utf-8", DASHBOARD_HTML);
            stream.write_all(&response).ok();
        }
        "/snapshot" => {
            let mut body = String::with_capacity(8192);
            build_snapshot_into(&mut body, &fleet, poll_interval_ms);
            let response = http_response("200 OK", "application/json", &body);
            stream.write_all(&response).ok();
        }
        "/metrics" => {
            let body = fleet_to_prometheus(&fleet);
            let response =
                http_response("200 OK", "text/plain; version=0.0.4; charset=utf-8", &body);
            stream.write_all(&response).ok();
        }
        "/stream" => {
            serve_stream(stream, fleet, poll_interval_ms, push_interval);
        }
        _ => {
            let body = "{\"error\":\"not found\"}";
            let response = http_response("404 Not Found", "application/json", body);
            stream.write_all(&response).ok();
        }
    }
}

// ── launchd install/uninstall ─────────────────────────────────────────────────

fn plist_path() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(
        std::path::PathBuf::from(home).join("Library/LaunchAgents/com.power-monitor-collect.plist"),
    )
}

fn do_install(hosts: &str, port: u16, interval_ms: u64, auth: crate::args::AuthArg) {
    use crate::args::AuthArg;

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: could not determine exe path: {e}");
            std::process::exit(1);
        }
    };
    let exe_str = exe.to_string_lossy().into_owned();

    let mut args_xml = String::new();
    let mut push = |s: &str| {
        args_xml.push_str("\t\t<string>");
        args_xml.push_str(s);
        args_xml.push_str("</string>\n");
    };
    push(&exe_str);
    push("collect");
    push("--host");
    push(hosts);
    push("--port");
    push(&port.to_string());
    push("--interval");
    push(&interval_ms.to_string());
    match auth {
        AuthArg::None => {}
        AuthArg::Inline(tok) => {
            eprintln!(
                "warning: storing the auth token in plaintext in the launchd plist; prefer --auth-file"
            );
            push("--auth");
            push(tok);
        }
        AuthArg::File(path) => {
            let abs = std::fs::canonicalize(path).unwrap_or_else(|e| {
                eprintln!("error: --auth-file '{path}': {e}");
                std::process::exit(1);
            });
            push("--auth-file");
            push(&abs.to_string_lossy());
        }
    }

    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>com.power-monitor-collect</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n{args_xml}\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         \t<key>KeepAlive</key>\n\
         \t<true/>\n\
         \t<key>StandardErrorPath</key>\n\
         \t<string>/tmp/power-monitor-collect.log</string>\n\
         </dict>\n\
         </plist>\n",
    );

    let path = match plist_path() {
        Some(p) => p,
        None => {
            eprintln!("error: could not determine HOME");
            std::process::exit(1);
        }
    };

    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("error: could not create LaunchAgents dir: {e}");
        std::process::exit(1);
    }

    if let Err(e) = std::fs::write(&path, &plist) {
        eprintln!("error: could not write plist to {}: {e}", path.display());
        std::process::exit(1);
    }

    let status = Command::new("launchctl")
        .args(["load", &path.to_string_lossy()])
        .status();

    match status {
        Ok(s) if s.success() => println!("installed and started: {}", path.display()),
        Ok(s) => eprintln!(
            "launchctl load exited with status {s}; plist at {}",
            path.display()
        ),
        Err(e) => eprintln!("error: launchctl load failed: {e}"),
    }
}

fn do_uninstall() {
    let path = match plist_path() {
        Some(p) => p,
        None => {
            eprintln!("error: could not determine HOME");
            std::process::exit(1);
        }
    };

    let status = Command::new("launchctl")
        .args(["unload", &path.to_string_lossy()])
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("warning: launchctl unload exited with status {s}"),
        Err(e) => eprintln!("warning: launchctl unload failed: {e}"),
    }

    match std::fs::remove_file(&path) {
        Ok(()) => println!("uninstalled: {}", path.display()),
        Err(e) => eprintln!("error: could not remove {}: {e}", path.display()),
    }
}

// ── CLI ──────────────────────────────────────────────────────────────────────

pub(crate) fn write_usage(w: &mut impl Write) {
    let _ = writeln!(
        w,
        "Usage: power-monitor collect (--host <list> | --tailnet) [options]"
    );
    let _ = writeln!(w);
    let _ = writeln!(w, "Aggregate many 'serve' agents into one fleet dashboard.");
    let _ = writeln!(w);
    let _ = writeln!(w, "Options:");
    let _ = writeln!(
        w,
        "      --host LIST     Comma-separated agent targets (host[:port])"
    );
    let _ = writeln!(
        w,
        "      --tailnet       Auto-discover hosts via 'tailscale status'"
    );
    let _ = writeln!(w, "                      (mutually exclusive with --host)");
    let _ = writeln!(
        w,
        "  -p, --port N        Dashboard HTTP listen port (default 8080)"
    );
    let _ = writeln!(
        w,
        "  -i, --interval N    Poll interval in ms per agent (default 1000)"
    );
    let _ = writeln!(
        w,
        "      --auth TOKEN    Forward 'Authorization: Bearer TOKEN' to every agent"
    );
    let _ = writeln!(
        w,
        "                      (insecure: visible in ps/shell history — prefer --auth-file)"
    );
    let _ = writeln!(
        w,
        "      --auth-file F   Read the bearer token from the first line of file F"
    );
    let _ = writeln!(
        w,
        "      --install       Install and start as a launchd user agent"
    );
    let _ = writeln!(w, "      --uninstall     Stop and remove the launchd agent");
    let _ = writeln!(w, "  -h, --help          Show this help");
    let _ = writeln!(w);
    let _ = writeln!(w, "Endpoints:");
    let _ = writeln!(w, "  GET /          dashboard HTML");
    let _ = writeln!(w, "  GET /stream    Server-Sent Events (fleet snapshot)");
    let _ = writeln!(w, "  GET /snapshot  one-shot JSON snapshot");
    let _ = writeln!(w, "  GET /metrics   aggregated Prometheus text (all hosts)");
    let _ = writeln!(w);
    let _ = writeln!(w, "Examples:");
    let _ = writeln!(w, "  power-monitor collect --host mac01,mac02,mac03");
    let _ = writeln!(w, "  power-monitor collect --tailnet --port 8080 --install");
}

/// Entry point for `power-monitor collect`.
pub fn run(args: &[String]) {
    use crate::args as argp;

    let mut hosts_arg: Option<String> = None;
    let mut tailnet = false;
    let mut port: u16 = 8080;
    let mut interval_ms: u64 = 1000;
    let mut auth_token: Option<String> = None;
    let mut auth_file: Option<String> = None;
    let mut install = false;
    let mut uninstall = false;

    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--host" | "--hosts" => {
                hosts_arg = Some(argp::take_value(args, &mut i, "--host").to_string())
            }
            "--tailnet" => tailnet = true,
            "-p" | "--port" => port = argp::parse_value(args, &mut i, "--port", "port"),
            "-i" | "--interval" => {
                interval_ms = argp::parse_value(args, &mut i, "--interval", "interval")
            }
            "--auth" => auth_token = Some(argp::take_value(args, &mut i, "--auth").to_string()),
            "--auth-file" => {
                auth_file = Some(argp::take_value(args, &mut i, "--auth-file").to_string())
            }
            "--install" => install = true,
            "--uninstall" => uninstall = true,
            "-h" | "--help" => {
                write_usage(&mut std::io::stdout().lock());
                return;
            }
            other => argp::unknown_arg(other),
        }
        i += 1;
    }

    argp::check_auth_exclusive(&auth_token, &auth_file);

    if uninstall {
        do_uninstall();
        return;
    }

    // Resolve host list: --tailnet runs discovery, otherwise use --host.
    if tailnet {
        if hosts_arg.is_some() {
            eprintln!("error: --tailnet and --host are mutually exclusive");
            std::process::exit(1);
        }
        match discover_tailnet() {
            Ok(v) if !v.is_empty() => {
                eprintln!(
                    "Discovered {} host(s) via Tailscale: {}",
                    v.len(),
                    v.join(", ")
                );
                hosts_arg = Some(v.join(","));
            }
            Ok(_) => {
                eprintln!("error: tailscale status returned no online peers");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("error: tailscale discovery failed: {e}");
                std::process::exit(1);
            }
        }
    }

    let hosts_str = match hosts_arg {
        Some(ref s) => s.clone(),
        None => {
            eprintln!("error: --host or --tailnet is required");
            write_usage(&mut std::io::stderr().lock());
            std::process::exit(2);
        }
    };

    if install {
        let auth_arg = match (&auth_token, &auth_file) {
            (Some(t), _) => argp::AuthArg::Inline(t),
            (None, Some(p)) => argp::AuthArg::File(p),
            (None, None) => argp::AuthArg::None,
        };
        do_install(&hosts_str, port, interval_ms, auth_arg);
        return;
    }

    // Serving path: resolve the file token now (install passed the path through).
    let auth = argp::resolve_auth(auth_token, auth_file);

    let targets = parse_hosts(&hosts_str, 9090);
    if targets.is_empty() {
        eprintln!("no valid hosts in --host");
        std::process::exit(1);
    }

    let fleet: Fleet = targets
        .iter()
        .cloned()
        .map(HostState::new)
        .map(Mutex::new)
        .collect::<Vec<_>>()
        .into();

    let interval = Duration::from_millis(interval_ms);
    spawn_pollers(Arc::clone(&fleet), interval, auth.clone());

    let addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        eprintln!("error: could not bind {addr}: {e}");
        std::process::exit(1);
    });

    eprintln!("Fleet dashboard: http://{addr}/");
    eprintln!("  GET /          -- dashboard HTML");
    eprintln!("  GET /snapshot  -- one-shot fleet JSON");
    eprintln!("  GET /stream    -- Server-Sent Events stream");
    eprintln!("  GET /metrics   -- aggregated Prometheus text");
    eprintln!(
        "Polling {} agent(s) every {} ms",
        targets.len(),
        interval_ms
    );
    if auth.is_some() {
        eprintln!("(auth: Authorization: Bearer <token> required on all requests)");
    }

    let push_interval = interval;
    let auth = Arc::new(auth);
    let inflight = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let Some(permit) = try_acquire(&inflight, MAX_INFLIGHT) else {
            let body = "{\"error\":\"server busy\"}";
            let response = http_response("503 Service Unavailable", "application/json", body);
            stream.write_all(&response).ok();
            continue;
        };
        let fleet = Arc::clone(&fleet);
        let auth = Arc::clone(&auth);
        let auth_ref = auth.as_ref().clone();
        thread::spawn(move || {
            let _permit = permit;
            handle_connection(
                stream,
                fleet,
                interval_ms,
                push_interval,
                auth_ref.as_deref(),
            );
        });
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_hosts ──────────────────────────────────────────────────────────

    #[test]
    fn parse_hosts_bare_names_get_default_port() {
        let v = parse_hosts("a,b,c", 9090);
        assert_eq!(v, vec!["a:9090", "b:9090", "c:9090"]);
    }

    #[test]
    fn parse_hosts_explicit_port_preserved() {
        let v = parse_hosts("a:1000,b:2000", 9090);
        assert_eq!(v, vec!["a:1000", "b:2000"]);
    }

    #[test]
    fn parse_hosts_strips_scheme_and_path() {
        let v = parse_hosts("http://mac01/path,https://mac02:9091/x", 9090);
        assert_eq!(v, vec!["mac01:9090", "mac02:9091"]);
    }

    #[test]
    fn parse_hosts_ignores_empty_and_whitespace() {
        let v = parse_hosts(" a , ,b ", 9090);
        assert_eq!(v, vec!["a:9090", "b:9090"]);
    }

    // ── Field parsers ───────────────────────────────────────────────────────

    #[test]
    fn walk_flat_object_yields_keys_and_values() {
        let json = r#"{"a":42,"b":"hi","c":-0.5,"d":null}"#;
        let mut pairs = Vec::new();
        walk_flat_object(json, |k, v| pairs.push((k.to_string(), v.to_string())));
        assert_eq!(
            pairs,
            vec![
                ("a".into(), "42".into()),
                ("b".into(), "hi".into()),
                ("c".into(), "-0.5".into()),
                ("d".into(), "null".into()),
            ]
        );
    }

    #[test]
    fn parse_sample_preserves_u64_precision() {
        // 2^34 + 1: outside f32 mantissa range.
        let n: u64 = 17_179_869_185;
        let json = format!(r#"{{"memory_total_bytes":{n}}}"#);
        let s = parse_sample(&json);
        assert_eq!(s.memory_total_bytes, n);
    }

    #[test]
    fn parse_sample_scientific_notation() {
        let json = r#"{"sys_power":1.5e3}"#;
        let s = parse_sample(json);
        assert!((s.sys_power - 1500.0).abs() < 0.01);
    }

    #[test]
    fn parse_sample_missing_field_defaults() {
        let s = parse_sample(r#"{"chip":"Apple M5"}"#);
        assert_eq!(s.chip, "Apple M5");
        assert_eq!(s.sys_power, 0.0);
        assert_eq!(s.memory_total_bytes, 0);
        assert!(s.cpu_temp_c.is_nan());
    }

    #[test]
    fn parse_sample_null_temp_round_trips_to_nan() {
        let json = r#"{"chip":"Apple M4","hostname":"sub-1","cpu_temp_c":null,"gpu_temp_c":null,"sys_power":1.2}"#;
        let s = parse_sample(json);
        assert!(s.cpu_temp_c.is_nan());
        assert!(s.gpu_temp_c.is_nan());
        assert!((s.sys_power - 1.2).abs() < 0.01);
    }

    #[test]
    fn parse_sample_full_payload() {
        let json = r#"{"timestamp":"2026-04-09T12:00:00Z","version":"0.1.0","hostname":"mac01","chip":"Apple M5","pcpu_cores":4,"ecpu_cores":6,"gpu_cores":10,"interval_ms":1000,"sys_power":12.3,"cpu_power":3.1,"gpu_power":1.2,"ane_power":0.0,"dram_power":0.5,"all_power":4.3,"ecpu_util":0.10,"ecpu_freq_mhz":1200,"pcpu_util":0.15,"pcpu_freq_mhz":2500,"gpu_util":0.05,"gpu_freq_mhz":700,"cpu_temp_c":45.0,"gpu_temp_c":44.0,"memory_used_bytes":8000000000,"memory_total_bytes":16000000000,"swap_used_bytes":0,"swap_total_bytes":0}"#;
        let s = parse_sample(json);
        assert_eq!(s.chip, "Apple M5");
        assert_eq!(s.hostname, "mac01");
        assert!((s.sys_power - 12.3).abs() < 0.01);
        assert!((s.cpu_power - 3.1).abs() < 0.01);
        assert_eq!(s.pcpu_freq_mhz, 2500);
        assert_eq!(s.memory_used_bytes, 8_000_000_000);
        assert_eq!(s.memory_total_bytes, 16_000_000_000);
        assert!((s.cpu_temp_c - 45.0).abs() < 0.01);
    }

    // ── Sparkline / history ─────────────────────────────────────────────────

    #[test]
    fn sparkline_empty_buffer_is_all_spaces() {
        let v = VecDeque::new();
        let s = sparkline(&v, 8, 40.0);
        assert_eq!(s, "        ");
    }

    #[test]
    fn sparkline_single_value_low_and_high() {
        let mut low = VecDeque::new();
        low.push_back(0.0);
        let s = sparkline(&low, 4, 40.0);
        assert_eq!(s, "   ▁");

        let mut high = VecDeque::new();
        high.push_back(40.0);
        let s = sparkline(&high, 4, 40.0);
        assert_eq!(s, "   █");
    }

    #[test]
    fn sparkline_clamps_above_scale_max() {
        let mut v = VecDeque::new();
        v.push_back(100.0);
        let s = sparkline(&v, 1, 40.0);
        assert_eq!(s, "█");
    }

    #[test]
    fn sparkline_pads_left_when_buffer_smaller_than_width() {
        let mut v = VecDeque::new();
        v.push_back(20.0);
        v.push_back(40.0);
        let s = sparkline(&v, 5, 40.0);
        assert_eq!(s.chars().count(), 5);
        assert!(s.starts_with("   "));
    }

    #[test]
    fn sparkline_shows_most_recent_when_buffer_overflows_width() {
        let mut v = VecDeque::new();
        for _ in 0..10 {
            v.push_back(0.0); // older samples, should be dropped
        }
        v.push_back(40.0); // most recent, should survive
        let s = sparkline(&v, 4, 40.0);
        // Buffer has 11 samples, width is 4 — should show last 4.
        // Last 4: [0.0, 0.0, 0.0, 40.0] → ▁▁▁█
        assert_eq!(s, "▁▁▁█");
    }

    #[test]
    fn push_bounded_respects_capacity() {
        let mut v = VecDeque::new();
        for i in 0..(HISTORY_LEN + 10) {
            push_bounded(&mut v, i as f32);
        }
        assert_eq!(v.len(), HISTORY_LEN);
        assert_eq!(*v.front().unwrap(), 10.0);
        assert_eq!(*v.back().unwrap(), (HISTORY_LEN + 9) as f32);
    }

    #[test]
    fn history_push_updates_all_ringbuffers() {
        let mut h = History::default();
        let s = ParsedSample {
            sys_power: 5.0,
            cpu_power: 1.5,
            gpu_power: 0.3,
            ane_power: 0.1,
            dram_power: 0.2,
            pcpu_util: 0.15,
            ecpu_util: 0.25,
            gpu_util: 0.05,
            memory_used_bytes: 8_000_000_000,
            memory_total_bytes: 16_000_000_000,
            swap_used_bytes: 1_500_000_000,
            swap_total_bytes: 3_000_000_000,
            ..Default::default()
        };
        h.push(&s);
        assert_eq!(h.sys_power.back(), Some(&5.0));
        assert_eq!(h.cpu_power.back(), Some(&1.5));
        assert_eq!(h.gpu_power.back(), Some(&0.3));
        assert_eq!(h.ane_power.back(), Some(&0.1));
        assert_eq!(h.dram_power.back(), Some(&0.2));
        assert_eq!(h.pcpu_util.back(), Some(&0.15));
        assert_eq!(h.ecpu_util.back(), Some(&0.25));
        assert_eq!(h.gpu_util.back(), Some(&0.05));
        assert!((h.mem_frac.back().unwrap() - 0.5).abs() < 1e-6);
        assert!((h.swap_frac.back().unwrap() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn history_push_handles_zero_total_memory() {
        let mut h = History::default();
        let s = ParsedSample {
            memory_used_bytes: 100,
            memory_total_bytes: 0,
            swap_used_bytes: 50,
            swap_total_bytes: 0,
            ..Default::default()
        };
        h.push(&s);
        assert_eq!(h.mem_frac.back(), Some(&0.0));
        assert_eq!(h.swap_frac.back(), Some(&0.0));
    }

    // ── Snapshot ─────────────────────────────────────────────────────────────

    #[test]
    fn snapshot_empty_fleet() {
        let s = build_snapshot(&[], 1000);
        assert!(s.starts_with("{\"generated_at\":"));
        assert!(s.contains("\"poll_interval_ms\":1000"));
        assert!(s.contains(&format!("\"history_len\":{HISTORY_LEN}")));
        assert!(s.contains("\"hosts\":[]"));
    }

    /// Wrap a list of host states for the new per-slot Mutex API.
    fn fleet(states: impl IntoIterator<Item = HostState>) -> Vec<Mutex<HostState>> {
        states.into_iter().map(Mutex::new).collect()
    }

    #[test]
    fn snapshot_never_seen_host_produces_null_metrics() {
        let f = fleet([HostState::new("mac01:9090".into())]);
        let s = build_snapshot(&f, 1000);
        assert!(s.contains("\"label\":\"mac01\""));
        assert!(s.contains("\"target\":\"mac01:9090\""));
        assert!(s.contains("\"ok\":false"));
        assert!(s.contains("\"stale_s\":null"));
        assert!(s.contains("\"metrics\":null"));
        // Even with no history, a spark field is emitted (all spaces).
        assert!(s.contains("\"spark\":"));
    }

    #[test]
    fn snapshot_inlines_raw_metrics_body_verbatim() {
        let mut h = HostState::new("mac01:9090".into());
        h.last_json = "{\"chip\":\"M5\",\"sys_power\":12.3}".into();
        h.last_ok_at = Some(Instant::now());
        let s = build_snapshot(&fleet([h]), 1000);
        assert!(s.contains("\"ok\":true"));
        assert!(s.contains("\"metrics\":{\"chip\":\"M5\",\"sys_power\":12.3}"));
    }

    #[test]
    fn snapshot_contains_sparkline_fields() {
        let mut h = HostState::new("mac01:9090".into());
        h.last_json = "{}".into();
        h.last_ok_at = Some(Instant::now());
        h.history.sys_power.push_back(20.0);
        h.history.cpu_power.push_back(10.0);
        h.history.gpu_power.push_back(8.0);
        let s = build_snapshot(&fleet([h]), 1000);
        assert!(s.contains("\"spark\":{"));
        assert!(s.contains("\"sys\":"));
        assert!(s.contains("\"cpu\":"));
        assert!(s.contains("\"gpu\":"));
    }

    // ── Prometheus aggregation ──────────────────────────────────────────────

    #[test]
    fn prometheus_emits_help_and_type_lines() {
        let text = fleet_to_prometheus(&fleet([]));
        assert!(text.contains("# HELP power_monitor_sys_power_watts"));
        assert!(text.contains("# TYPE power_monitor_sys_power_watts gauge"));
        assert!(text.contains("# HELP power_monitor_host_up"));
    }

    #[test]
    fn prometheus_emits_labelled_samples_for_online_hosts() {
        let mut h = HostState::new("mac01:9090".into());
        h.parsed = Some(ParsedSample {
            chip: "Apple M5".into(),
            hostname: "mac01".into(),
            sys_power: 12.5,
            ..Default::default()
        });
        h.last_ok_at = Some(Instant::now());
        let text = fleet_to_prometheus(&fleet([h]));
        assert!(
            text.contains("power_monitor_sys_power_watts{chip=\"Apple M5\",host=\"mac01\"} 12.500")
        );
        assert!(text.contains(
            "power_monitor_host_up{chip=\"Apple M5\",host=\"mac01\",target=\"mac01:9090\"} 1"
        ));
    }

    #[test]
    fn prometheus_host_up_is_zero_for_never_seen() {
        let f = fleet([HostState::new("never-seen:9090".into())]);
        let text = fleet_to_prometheus(&f);
        assert!(text.contains(
            "power_monitor_host_up{chip=\"\",host=\"never-seen\",target=\"never-seen:9090\"} 0"
        ));
    }

    // ── Tailnet discovery parsing ───────────────────────────────────────────

    #[test]
    fn parse_tailnet_status_picks_online_peers() {
        let stdout = "\
100.64.1.1    dashboard-host      me@        macOS   -
100.64.1.5    mac01               me@        macOS   idle; direct 192.168.1.5:41641
100.64.1.6    mac02               me@        macOS   active
100.64.1.7    mac03               me@        macOS   offline
";
        let hosts = parse_tailnet_status(stdout);
        assert_eq!(hosts, vec!["dashboard-host", "mac01", "mac02"]);
    }

    #[test]
    fn parse_tailnet_status_skips_non_tailscale_ips() {
        let stdout = "\
192.168.1.5    bogus               me@        macOS   -
100.64.1.5    mac01               me@        macOS   idle
";
        assert_eq!(parse_tailnet_status(stdout), vec!["mac01"]);
    }

    #[test]
    fn parse_tailnet_status_handles_ipv6_tailnet() {
        let stdout = "fd7a:115c:a1e0::1    mac01   me@   macOS  idle";
        assert_eq!(parse_tailnet_status(stdout), vec!["mac01"]);
    }

    #[test]
    fn parse_tailnet_status_returns_empty_for_blank_input() {
        assert!(parse_tailnet_status("").is_empty());
    }

    // ── Misc ────────────────────────────────────────────────────────────────

    #[test]
    fn escape_json_handles_quotes_and_backslashes() {
        assert_eq!(escape_json("a\"b\\c"), "a\\\"b\\\\c");
    }

    #[test]
    fn host_state_label_is_hostname_only() {
        let h = HostState::new("mac01.lan:9090".into());
        assert_eq!(h.label, "mac01.lan");
    }
}
