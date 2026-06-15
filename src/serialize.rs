//! Wire-format serialization for [`Metrics`] — JSON line output and the
//! per-agent context that surrounds it.
//!
//! Used by `pipe`, `serve`, and `collect` subcommands and re-exposed for
//! library consumers. The writer-based variants ([`write_metrics_json`],
//! [`write_utc_now`]) are alloc-free after a one-time buffer allocation,
//! making them safe to call in tight per-tick loops.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::Metrics;

// ── Hostname ─────────────────────────────────────────────────────────────────

/// Return the machine's short hostname. Falls back to `"localhost"` on error.
///
/// Strips a trailing `.local` suffix (common on Bonjour-configured Macs)
/// so dashboard tiles show `macbook-pro` instead of `macbook-pro.local`.
pub fn hostname() -> String {
    unsafe extern "C" {
        fn gethostname(name: *mut u8, namelen: usize) -> i32;
    }
    let mut buf = [0u8; 256];
    // SAFETY: writing up to buf.len() bytes into our own stack buffer.
    let rc = unsafe { gethostname(buf.as_mut_ptr(), buf.len()) };
    if rc != 0 {
        return "localhost".to_string();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let mut host = String::from_utf8_lossy(&buf[..end]).into_owned();
    if let Some(stripped) = host.strip_suffix(".local") {
        host = stripped.to_string();
    }
    host
}

// ── UTC timestamp ─────────────────────────────────────────────────────────────

/// Convert a count of days since the Unix epoch (1970-01-01) to (year, month, day).
///
/// Howard Hinnant's `civil_from_days` algorithm — constant-time using
/// 400-year era arithmetic so the leap-century corrections collapse to a
/// handful of integer divisions. No loops, no per-year/per-month walking.
///
/// Reference: <http://howardhinnant.github.io/date_algorithms.html#civil_from_days>
/// (the same algorithm C++20 `<chrono>` adopted for `year_month_day`).
pub fn days_to_ymd(days: u64) -> (u32, u32, u32) {
    // Shift origin from 1970-01-01 to 0000-03-01 so the year boundary aligns
    // with March, putting the leap-day at the *end* of the year and removing
    // the special case from the month math.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097); // 146_097 = days in 400 yrs
    let doe = (z - era * 146_097) as u64; // day of era,  [0, 146_096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // year of era, [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of (shifted) year, [0, 365]
    let mp = (5 * doy + 2) / 153; // shifted month, [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // day of month, [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // calendar month, [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y as u32, m as u32, d as u32)
}

/// Returns the current UTC time formatted as RFC 3339 (`2026-04-09T12:34:56Z`).
pub fn utc_now() -> String {
    let mut s = String::with_capacity(20);
    let _ = write_utc_now(&mut s);
    s
}

/// Write the current UTC RFC 3339 timestamp directly into a writer — no
/// intermediate `String`. Used by alloc-free hot loops (e.g. `pipe_cmd`).
pub fn write_utc_now<W: std::fmt::Write>(out: &mut W) -> std::fmt::Result {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let (year, month, day) = days_to_ymd(secs / 86400);
    let rem = secs % 86400;
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;
    let ss = rem % 60;

    write!(
        out,
        "{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z"
    )
}

// ── JSON serialisation ────────────────────────────────────────────────────────

/// Per-agent static context — chip identity, core counts, hostname, version.
/// Read once at agent startup and passed into [`metrics_to_json`] on every
/// request.
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub version: &'static str,
    pub hostname: String,
    pub chip: String,
    pub pcpu_cores: u32,
    pub ecpu_cores: u32,
    pub gpu_cores: u32,
    pub interval_ms: u64,
}

impl AgentInfo {
    /// Populate from a live [`crate::Sampler`].
    pub fn from_sampler(sampler: &crate::Sampler, interval_ms: u64) -> Self {
        let soc = &sampler.soc;
        AgentInfo {
            version: env!("CARGO_PKG_VERSION"),
            hostname: hostname(),
            chip: soc.chip_name.clone(),
            pcpu_cores: soc.pcpu_level().map_or(0, |l| l.cores),
            ecpu_cores: soc.ecpu_level().map_or(0, |l| l.cores),
            gpu_cores: soc.gpu_cores,
            interval_ms,
        }
    }
}

/// Serialize `m` to a single-line JSON object. Reused by `serve_cmd` and
/// `collect_cmd` (the latter stores raw payloads verbatim and wraps them).
///
/// For alloc-free hot loops, prefer [`write_metrics_json`] which writes
/// directly into a borrowed writer.
pub fn metrics_to_json(m: &Metrics, info: &AgentInfo) -> String {
    // Pre-size for typical 540-byte payloads to avoid mid-write resizes.
    let mut s = String::with_capacity(640);
    let _ = write_metrics_json(&mut s, m, info);
    s
}

/// Write the JSON payload directly into a writer — no intermediate `String`.
/// Used by `pipe_cmd`'s tight loop to keep per-frame allocations at zero.
pub fn write_metrics_json<W: std::fmt::Write>(
    out: &mut W,
    m: &Metrics,
    info: &AgentInfo,
) -> std::fmt::Result {
    out.write_str("{\"timestamp\":\"")?;
    write_utc_now(out)?;
    out.write_str("\",\"version\":\"")?;
    write_escape_json(out, info.version)?;
    out.write_str("\",\"hostname\":\"")?;
    write_escape_json(out, &info.hostname)?;
    out.write_str("\",\"chip\":\"")?;
    write_escape_json(out, &info.chip)?;
    write!(
        out,
        "\",\"pcpu_cores\":{pc},\"ecpu_cores\":{ec},\"gpu_cores\":{gc},\"interval_ms\":{iv},\
\"sys_power\":{sys:.1},\
\"cpu_power\":{cpu:.1},\
\"gpu_power\":{gpu:.1},\
\"ane_power\":{ane:.1},\
\"dram_power\":{dram:.1},\
\"all_power\":{all:.1},\
\"ecpu_util\":{eu:.3},\
\"ecpu_freq_mhz\":{ef},\
\"pcpu_util\":{pu:.3},\
\"pcpu_freq_mhz\":{pf},\
\"gpu_util\":{gu:.3},\
\"gpu_freq_mhz\":{gf},",
        pc = info.pcpu_cores,
        ec = info.ecpu_cores,
        gc = info.gpu_cores,
        iv = info.interval_ms,
        sys = m.sys_power,
        cpu = m.cpu_power,
        gpu = m.gpu_power,
        ane = m.ane_power,
        dram = m.dram_power,
        all = m.all_power,
        eu = m.ecpu.utilization,
        ef = m.ecpu.freq_mhz,
        pu = m.pcpu.utilization,
        pf = m.pcpu.freq_mhz,
        gu = m.gpu_util,
        gf = m.gpu_freq_mhz,
    )?;
    // Temps are the only fields that legitimately go NaN (rail-gated sensors).
    write_temp_field(out, "cpu_temp_c", m.cpu_temp)?;
    out.write_str(",")?;
    write_temp_field(out, "gpu_temp_c", m.gpu_temp)?;
    write!(
        out,
        ",\"fan_rpm\":{fr},\"fan_max_rpm\":{fm},\
\"memory_used_bytes\":{mu},\
\"memory_total_bytes\":{mt},\
\"swap_used_bytes\":{su},\
\"swap_total_bytes\":{st}}}",
        fr = m.fan_rpm,
        fm = m.fan_max_rpm,
        mu = m.memory.used,
        mt = m.memory.total,
        su = m.swap.used,
        st = m.swap.total,
    )
}

fn write_temp_field<W: std::fmt::Write>(out: &mut W, name: &str, t: f32) -> std::fmt::Result {
    if t.is_nan() {
        write!(out, "\"{name}\":null")
    } else {
        write!(out, "\"{name}\":{t:.1}")
    }
}

// ── Prometheus gauge catalog ──────────────────────────────────────────────────

/// One Prometheus gauge: `(metric suffix, help text, value extractor)`.
pub type PromGauge = (&'static str, &'static str, fn(&Metrics) -> f64);

/// Canonical Prometheus gauge catalog.
///
/// Single source of truth shared by the single-host (`serve`) and fleet
/// (`collect`) exporters so their metric names, help strings, and units can't
/// drift apart. The `power_monitor_` prefix is added by the writer; NaN values
/// (rail-gated temps) are skipped at the sample line, never the `# HELP`/`# TYPE`
/// header. `all_power` is intentionally absent — it's a JSON convenience field,
/// not a gauge.
pub const PROM_GAUGES: &[PromGauge] = &[
    ("sys_power_watts", "Total system power draw in watts", |m| {
        m.sys_power as f64
    }),
    ("cpu_power_watts", "CPU power draw in watts", |m| {
        m.cpu_power as f64
    }),
    ("gpu_power_watts", "GPU power draw in watts", |m| {
        m.gpu_power as f64
    }),
    (
        "ane_power_watts",
        "Apple Neural Engine power draw in watts",
        |m| m.ane_power as f64,
    ),
    ("dram_power_watts", "DRAM power draw in watts", |m| {
        m.dram_power as f64
    }),
    (
        "ecpu_utilization",
        "Efficiency CPU cluster utilization (0-1)",
        |m| m.ecpu.utilization as f64,
    ),
    (
        "pcpu_utilization",
        "Performance CPU cluster utilization (0-1)",
        |m| m.pcpu.utilization as f64,
    ),
    ("gpu_utilization", "GPU utilization (0-1)", |m| {
        m.gpu_util as f64
    }),
    (
        "ecpu_freq_mhz",
        "Efficiency CPU cluster weighted average frequency in MHz",
        |m| m.ecpu.freq_mhz as f64,
    ),
    (
        "pcpu_freq_mhz",
        "Performance CPU cluster weighted average frequency in MHz",
        |m| m.pcpu.freq_mhz as f64,
    ),
    (
        "gpu_freq_mhz",
        "GPU weighted average frequency in MHz",
        |m| m.gpu_freq_mhz as f64,
    ),
    (
        "cpu_temp_celsius",
        "CPU temperature in degrees Celsius",
        |m| m.cpu_temp as f64,
    ),
    (
        "gpu_temp_celsius",
        "GPU temperature in degrees Celsius",
        |m| m.gpu_temp as f64,
    ),
    (
        "fan_rpm",
        "Highest-duty fan current RPM (0 if fanless)",
        |m| m.fan_rpm as f64,
    ),
    (
        "fan_max_rpm",
        "Highest-duty fan max RPM (0 if fanless)",
        |m| m.fan_max_rpm as f64,
    ),
    ("memory_used_bytes", "Physical RAM used in bytes", |m| {
        m.memory.used as f64
    }),
    ("memory_total_bytes", "Physical RAM total in bytes", |m| {
        m.memory.total as f64
    }),
    ("swap_used_bytes", "Swap used in bytes", |m| {
        m.swap.used as f64
    }),
    ("swap_total_bytes", "Swap total in bytes", |m| {
        m.swap.total as f64
    }),
];

/// JSON-escape `s` into `out`. Handles the standard escapes plus control chars.
pub fn write_escape_json<W: std::fmt::Write>(out: &mut W, s: &str) -> std::fmt::Result {
    for ch in s.chars() {
        match ch {
            '"' => out.write_str("\\\"")?,
            '\\' => out.write_str("\\\\")?,
            '\n' => out.write_str("\\n")?,
            '\r' => out.write_str("\\r")?,
            '\t' => out.write_str("\\t")?,
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32)?,
            c => out.write_char(c)?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inverse of [`days_to_ymd`] — Hinnant's `days_from_civil`. Used as a
    /// round-trip helper so tests can express dates as `(y, m, d)` instead of
    /// hand-computed day counts.
    fn days_from_civil(y: i64, m: u32, d: u32) -> u64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = (y - era * 400) as u64;
        let m = m as u64;
        let mp = if m > 2 { m - 3 } else { m + 9 };
        let doy = (153 * mp + 2) / 5 + (d - 1) as u64;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        (era * 146_097 + doe as i64 - 719_468) as u64
    }

    #[test]
    fn epoch_is_1970_01_01() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn day_1_is_1970_01_02() {
        assert_eq!(days_to_ymd(1), (1970, 1, 2));
    }

    #[test]
    fn year_boundary_1970_1971() {
        assert_eq!(days_to_ymd(365), (1971, 1, 1));
    }

    #[test]
    fn leap_year_1972_has_feb_29() {
        assert_eq!(days_to_ymd(789), (1972, 2, 29));
    }

    #[test]
    fn known_date_2026_04_09() {
        assert_eq!(days_to_ymd(20552), (2026, 4, 9));
    }

    /// Round-trip nine landmark dates spanning normal/leap/century/four-century
    /// edge cases. Lets the test express dates as `(y, m, d)` instead of
    /// hand-computed day counts.
    #[test]
    fn landmark_dates_round_trip() {
        for (y, m, d) in [
            (1970, 1, 1),
            (1970, 12, 31),
            (1972, 2, 29),  // leap by /4
            (1999, 12, 31), // pre-Y2K
            (2000, 1, 1),
            (2000, 2, 29), // leap by /400
            (2024, 6, 15),
            (2100, 3, 1),  // day after non-leap Feb 28
            (2400, 2, 29), // leap by /400 again
        ] {
            let days = days_from_civil(y as i64, m, d);
            assert_eq!(
                days_to_ymd(days),
                (y as u32, m, d),
                "round-trip failed for {y}-{m:02}-{d:02}"
            );
        }
    }

    #[test]
    fn leap_century_2000_is_leap() {
        // 2000 is divisible by 400 → leap year.
        let d = days_from_civil(2000, 2, 29);
        assert_eq!(days_to_ymd(d), (2000, 2, 29));
    }

    #[test]
    fn century_2100_is_not_leap() {
        // 2100 is divisible by 100 but not 400 → NOT leap.
        // 2100-02-28 → 2100-03-01 must be next day.
        let feb28 = days_from_civil(2100, 2, 28);
        assert_eq!(days_to_ymd(feb28), (2100, 2, 28));
        assert_eq!(days_to_ymd(feb28 + 1), (2100, 3, 1));
    }

    #[test]
    fn leap_year_2400_is_leap() {
        let d = days_from_civil(2400, 2, 29);
        assert_eq!(days_to_ymd(d), (2400, 2, 29));
    }

    #[test]
    fn utc_now_looks_like_rfc3339() {
        let s = utc_now();
        assert_eq!(s.len(), 20, "expected 20-char timestamp, got: {s}");
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[13..14], ":");
        assert_eq!(&s[16..17], ":");
    }
}
