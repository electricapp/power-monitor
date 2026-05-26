mod args;
mod collect_cmd;
mod http;
mod pipe_cmd;
mod seqlock;
mod serve_cmd;

use power_monitor::Sampler;
use std::io::Write;

// ── time FFI (used for the bottom-bar clock) ─────────────────────────────────

#[repr(C)]
struct LibcTm {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
    tm_gmtoff: i64,
    tm_zone: *const u8,
}

unsafe extern "C" {
    fn time(tloc: *mut i64) -> i64;
    fn localtime_r(clock: *const i64, result: *mut LibcTm) -> *mut LibcTm;
}

use power_monitor::serialize::hostname;

fn now_str() -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    // SAFETY: time/localtime_r with valid out-params.
    let mut tm = LibcTm {
        tm_sec: 0,
        tm_min: 0,
        tm_hour: 0,
        tm_mday: 0,
        tm_mon: 0,
        tm_year: 0,
        tm_wday: 0,
        tm_yday: 0,
        tm_isdst: 0,
        tm_gmtoff: 0,
        tm_zone: std::ptr::null(),
    };
    unsafe {
        let t = time(std::ptr::null_mut());
        if localtime_r(&t, &mut tm).is_null() {
            return String::new();
        }
    }
    let mon = MONTHS.get(tm.tm_mon as usize).copied().unwrap_or("???");
    format!(
        "{:02}:{:02} {:02}-{}",
        tm.tm_hour, tm.tm_min, tm.tm_mday, mon
    )
}

// ── ANSI ─────────────────────────────────────────────────────────────────────

const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const HOME: &str = "\x1b[H";
const ERASE_LINE: &str = "\x1b[2K";
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const CYAN: &str = "\x1b[36m";

const BAR_W: usize = 24;

fn heat(frac: f32) -> &'static str {
    if frac < 0.40 {
        GREEN
    } else if frac < 0.75 {
        YELLOW
    } else {
        RED
    }
}

fn bar(val: f32, max: f32) -> String {
    let frac = (val / max).clamp(0.0, 1.0);
    let n = (frac * BAR_W as f32).round() as usize;
    // No gap between filled and empty — total is always exactly BAR_W chars.
    format!(
        "{}{}{DIM}{}{RESET}",
        heat(frac),
        "█".repeat(n),
        "░".repeat(BAR_W - n)
    )
}

/// Left column shared by every row: `  LABEL NN%  `.
/// Fixed visual width so bars line up across sections.
fn left_col(label: &str, pct: f32) -> String {
    format!(
        "  {BOLD}{label:<5}{RESET} {DIM}{:3.0}%{RESET}  ",
        pct.clamp(0.0, 100.0),
    )
}

fn power_row(label: &str, watts: f32, max: f32) -> String {
    let pct = (watts / max * 100.0).clamp(0.0, 100.0);
    format!(
        "{}{}  {CYAN}{watts:5.2}  W{RESET}",
        left_col(label, pct),
        bar(watts, max),
    )
}

/// Power row with a trailing temperature badge: `... 1.24  W  (46°C)`.
/// `live=false` dims the badge to indicate a cached (rail-gated) reading.
fn power_row_temp(label: &str, watts: f32, max: f32, temp: f32, live: bool) -> String {
    let pct = (watts / max * 100.0).clamp(0.0, 100.0);
    format!(
        "{}{}  {CYAN}{watts:5.2}  W{RESET}  {DIM}({RESET}{}{DIM}){RESET}",
        left_col(label, pct),
        bar(watts, max),
        temp_str(temp, live),
    )
}

fn util_row(label: &str, util: f32, freq_mhz: u32) -> String {
    format!(
        "{}{}  {CYAN}{freq_mhz:4} MHz{RESET}",
        left_col(label, util * 100.0),
        bar(util, 1.0),
    )
}

fn fan_row(rpm: u32, max_rpm: u32) -> String {
    if max_rpm == 0 {
        return format!(
            "  {BOLD}FAN{RESET}    {DIM}—  {}  fanless{RESET}",
            "─".repeat(BAR_W),
        );
    }
    let duty = (rpm as f32 / max_rpm as f32).clamp(0.0, 1.0);
    format!(
        "{}{}  {CYAN}{rpm:>4} RPM{RESET}",
        left_col("FAN", duty * 100.0),
        bar(duty, 1.0),
    )
}

fn mem_row(label: &str, used: u64, total: u64) -> String {
    let pct = if total > 0 {
        used as f32 / total as f32
    } else {
        0.0
    };
    format!(
        "{}{}  {CYAN}{}{RESET} {DIM}/ {} GB{RESET}",
        left_col(label, pct * 100.0),
        bar(pct, 1.0),
        fmt_gb(used),
        fmt_gb(total),
    )
}

fn temp_str(t: f32, live: bool) -> String {
    if t.is_nan() {
        return format!("{DIM}--°C{RESET}");
    }
    if !live {
        return format!("{DIM}{t:.0}°C{RESET}");
    }
    let color = if t < 70.0 {
        GREEN
    } else if t < 85.0 {
        YELLOW
    } else {
        RED
    };
    format!("{color}{t:.0}°C{RESET}")
}

/// Returns `(display_temp, live)`. When `current` is NaN, falls back to the
/// cached `last`; the bool says whether the displayed value is from this tick.
fn display_temp(current: f32, last: &mut f32) -> (f32, bool) {
    if !current.is_nan() {
        *last = current;
        (current, true)
    } else {
        (*last, false)
    }
}

/// Format bytes as GB, number only (no unit).
fn fmt_gb(b: u64) -> String {
    format!("{:4.1}", b as f64 / (1u64 << 30) as f64)
}

// ── Box drawing ──────────────────────────────────────────────────────────────

/// Visual (terminal column) length of a string, ignoring ANSI CSI escape sequences.
/// Handles multi-byte UTF-8 correctly for the characters we use (°, █, ░, box-draw).
fn visual_len(s: &str) -> usize {
    let b = s.as_bytes();
    let mut len = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == 0x1b && i + 1 < b.len() && b[i + 1] == b'[' {
            // CSI sequence: advance past \x1b[ then skip until final byte (A-Z or a-z)
            i += 2;
            while i < b.len() && !b[i].is_ascii_alphabetic() {
                i += 1;
            }
            i += 1;
        } else {
            // Count only UTF-8 leading bytes (not continuation bytes 10xxxxxx)
            if b[i] & 0xC0 != 0x80 {
                len += 1;
            }
            i += 1;
        }
    }
    len
}

/// Write one box line: `│ {content}{padding} │\n`, clearing any old content first.
fn box_row(out: &mut impl Write, content: &str, inner: usize) {
    let vlen = visual_len(content);
    let pad = inner.saturating_sub(vlen);
    writeln!(
        out,
        "{ERASE_LINE}{DIM}│{RESET}{content}{}{DIM}│{RESET}",
        " ".repeat(pad)
    )
    .ok();
}

/// Write an empty box line (blank row inside the border).
fn box_empty(out: &mut impl Write, inner: usize) {
    writeln!(
        out,
        "{ERASE_LINE}{DIM}│{RESET}{}{DIM}│{RESET}",
        " ".repeat(inner)
    )
    .ok();
}

/// Write a full-width horizontal rule inside the box: `│──────│`.
fn box_rule(out: &mut impl Write, inner: usize) {
    writeln!(out, "{ERASE_LINE}{DIM}│{}│{RESET}", "─".repeat(inner)).ok();
}

// ── Cursor guard ──────────────────────────────────────────────────────────────

struct Screen;

impl Screen {
    fn open() -> Self {
        print!("{HIDE_CURSOR}\x1b[2J{HOME}");
        std::io::stdout().flush().ok();
        Screen
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        println!("{SHOW_CURSOR}{RESET}");
        std::io::stdout().flush().ok();
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn print_usage() {
    eprintln!("Usage: power-monitor [COMMAND]");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  (none)          Live TUI dashboard");
    eprintln!("  pipe            Stream JSON metrics to stdout (NDJSON)");
    eprintln!("  serve           Serve JSON + Prometheus metrics over HTTP");
    eprintln!("  collect         Aggregate many agents into one fleet dashboard");
    eprintln!();
    eprintln!("Options for pipe:");
    eprintln!("  -s, --samples N   Stop after N samples (default: 0 = infinite)");
    eprintln!("  -i, --interval N  Sampling window in ms (default: 1000)");
    eprintln!();
    eprintln!("Options for serve:");
    eprintln!("      --bind ADDR   Bind address (default: 127.0.0.1)");
    eprintln!("  -p, --port N      Listen port (default: 9090)");
    eprintln!("  -i, --interval N  Sampling interval in ms (default: 1000)");
    eprintln!("      --auth TOKEN  Require 'Authorization: Bearer TOKEN' on all requests");
    eprintln!("      --install     Install as a launchd user agent and start");
    eprintln!("      --uninstall   Stop and remove the launchd agent");
    eprintln!();
    eprintln!("Options for collect:");
    eprintln!("      --host LIST   Comma-separated list of agents (host[:port])");
    eprintln!("  -p, --port N      Dashboard listen port (default: 8080)");
    eprintln!("  -i, --interval N  Per-agent poll interval in ms (default: 1000)");
    eprintln!("      --auth TOKEN  Forward 'Authorization: Bearer TOKEN' to every agent");
    eprintln!("      --install     Install as a launchd user agent and start");
    eprintln!("      --uninstall   Stop and remove the launchd agent");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("pipe") => {
            pipe_cmd::run(&args[1..]);
            return;
        }
        Some("serve") => {
            serve_cmd::run(&args[1..]);
            return;
        }
        Some("collect") => {
            collect_cmd::run(&args[1..]);
            return;
        }
        Some("-h") | Some("--help") => {
            print_usage();
            return;
        }
        Some(unknown) => {
            eprintln!("Unknown command: {unknown}");
            print_usage();
            std::process::exit(1);
        }
        None => {} // fall through to TUI
    }

    let mut sampler = Sampler::new().expect("failed to open Sampler");
    let _screen = Screen::open();
    let mut out = std::io::stdout().lock();
    let host = hostname();
    let version = env!("CARGO_PKG_VERSION");
    // Cached last-known temps so the badge holds a stale (dim) value
    // through rail-gated windows instead of blanking.
    let mut last_cpu_temp = f32::NAN;
    let mut last_gpu_temp = f32::NAN;

    loop {
        let m = sampler.get_metrics(1000);
        let (cpu_disp, cpu_live) = display_temp(m.cpu_temp, &mut last_cpu_temp);
        let (gpu_disp, gpu_live) = display_temp(m.gpu_temp, &mut last_gpu_temp);

        write!(out, "{HOME}").ok();

        // Inner width = content chars between │ and │.
        const INNER: usize = 56;

        let soc = &sampler.soc;
        let pcpu = soc.pcpu_level().map_or(0, |l| l.cores);
        let ecpu = soc.ecpu_level().map_or(0, |l| l.cores);
        let total_gb = (m.memory.total as f64 / (1u64 << 30) as f64).round() as u32;
        let title = if soc.gpu_cores > 0 {
            format!(
                " v{version} · {} · {pcpu}P + {ecpu}E · {} GPU · {total_gb}GB ",
                soc.chip_name, soc.gpu_cores
            )
        } else {
            format!(
                " v{version} · {} · {pcpu}P + {ecpu}E · {total_gb}GB ",
                soc.chip_name
            )
        };

        // Top border: ╭─{title}{dashes}╮  — total visible must equal INNER + 2.
        // Fixed chars: ╭─ (2) + ╮ (1) = 3, so dashes = INNER + 2 - 3 - t_len.
        let t_len = visual_len(&title);
        let pad = (INNER + 2).saturating_sub(t_len + 3);
        writeln!(
            out,
            "{ERASE_LINE}{DIM}╭─{RESET}{BOLD}{title}{RESET}{DIM}{pad}╮{RESET}",
            pad = "─".repeat(pad)
        )
        .ok();

        box_empty(&mut out, INNER);

        // Power
        box_row(&mut out, &power_row("SYS", m.sys_power, 40.0), INNER);
        box_row(&mut out, &fan_row(m.fan_rpm, m.fan_max_rpm), INNER);
        box_rule(&mut out, INNER);
        box_row(
            &mut out,
            &power_row_temp("GPU", m.gpu_power, 16.0, gpu_disp, gpu_live),
            INNER,
        );
        box_row(
            &mut out,
            &power_row_temp("CPU", m.cpu_power, 20.0, cpu_disp, cpu_live),
            INNER,
        );
        box_row(&mut out, &power_row("ANE", m.ane_power, 8.0), INNER);
        box_row(&mut out, &power_row("DRAM", m.dram_power, 5.0), INNER);

        box_empty(&mut out, INNER);

        // Utilisation
        box_row(
            &mut out,
            &util_row("PCPU", m.pcpu.utilization, m.pcpu.freq_mhz),
            INNER,
        );
        box_row(
            &mut out,
            &util_row("ECPU", m.ecpu.utilization, m.ecpu.freq_mhz),
            INNER,
        );
        box_row(
            &mut out,
            &util_row("GPU", m.gpu_util, m.gpu_freq_mhz),
            INNER,
        );

        box_empty(&mut out, INNER);

        // Memory
        box_row(
            &mut out,
            &mem_row("MEM", m.memory.used, m.memory.total),
            INNER,
        );
        box_row(&mut out, &mem_row("SWAP", m.swap.used, m.swap.total), INNER);

        box_empty(&mut out, INNER);

        // Bottom border: ╰ "host" HH:MM DD-Mon ────── interval ╯
        let interval_str = format!(" {:.0} ms ", m.interval_ms);
        let sys_str = format!(" \"{host}\" {} ", now_str());
        let pad = (INNER + 2).saturating_sub(sys_str.len() + interval_str.len() + 2);
        writeln!(
            out,
            "{ERASE_LINE}{DIM}╰{sys_str}{pad}{interval_str}╯{RESET}",
            pad = "─".repeat(pad),
        )
        .ok();

        out.flush().ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_temp_caches_through_nan() {
        let mut last = f32::NAN;
        let (v, live) = display_temp(46.0, &mut last);
        assert!((v - 46.0).abs() < 1e-6);
        assert!(live);

        let (v, live) = display_temp(f32::NAN, &mut last);
        assert!((v - 46.0).abs() < 1e-6);
        assert!(!live);
    }

    #[test]
    fn display_temp_first_nan_returns_nan() {
        let mut last = f32::NAN;
        let (v, live) = display_temp(f32::NAN, &mut last);
        assert!(v.is_nan());
        assert!(!live);
    }

    #[test]
    fn temp_str_nan_renders_dashes() {
        assert!(temp_str(f32::NAN, true).contains("--°C"));
    }
}
