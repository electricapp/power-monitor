mod args;
mod collect_cmd;
mod completion_cmd;
mod doctor_cmd;
mod fan_cmd;
mod http;
mod man_cmd;
mod pipe_cmd;
mod seqlock;
mod serve_cmd;

use power_monitor::Sampler;
use std::io::{IsTerminal, Write};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

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
    // `handler` is a `void (*)(int)`, passed as a pointer-sized value so the
    // same declaration can install a real handler or the special SIG_DFL (0).
    fn signal(sig: i32, handler: usize) -> usize;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
}

const SIG_DFL: usize = 0;
const SIGINT: i32 = 2;
const SIGPIPE: i32 = 13;
const SIGTERM: i32 = 15;

/// Restore the default SIGPIPE action. Rust ignores SIGPIPE at startup, which
/// turns a closed downstream pipe (`power-monitor pipe | head`) into an EPIPE
/// that makes `print!` panic. Resetting to SIG_DFL makes the process exit
/// quietly on a broken pipe, like a conventional Unix tool.
fn reset_sigpipe() {
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
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
//
// Escape sequences are resolved at runtime, not baked in as `const`, so the
// dashboard can honor the terminal it's actually running in. Two independent
// capabilities gate them:
//
//   * `tty`   — gates cursor/screen *control* (hide cursor, clear, home, erase
//               line). Off a TTY these are noise, so they collapse to "".
//   * `color` — gates SGR *color* (bold/dim/colors). Honors NO_COLOR,
//               TERM=dumb, --no-color, and the no-TTY case.
//
// Both default to full color (`COLOR`) before `init_palette`, so unit tests —
// which never initialize the palette — measure the same visible widths.

#[derive(Clone, Copy)]
struct Palette {
    hide_cursor: &'static str,
    show_cursor: &'static str,
    clear: &'static str,
    home: &'static str,
    erase_line: &'static str,
    reset: &'static str,
    bold: &'static str,
    dim: &'static str,
    green: &'static str,
    yellow: &'static str,
    red: &'static str,
    cyan: &'static str,
}

const COLOR: Palette = Palette {
    hide_cursor: "\x1b[?25l",
    show_cursor: "\x1b[?25h",
    clear: "\x1b[2J",
    home: "\x1b[H",
    erase_line: "\x1b[2K",
    reset: "\x1b[0m",
    bold: "\x1b[1m",
    dim: "\x1b[2m",
    green: "\x1b[32m",
    yellow: "\x1b[33m",
    red: "\x1b[31m",
    cyan: "\x1b[36m",
};

static PALETTE: OnceLock<Palette> = OnceLock::new();
static IS_TTY: AtomicBool = AtomicBool::new(false);

/// The active palette. Falls back to full color before `init_palette` so any
/// pre-init formatting (and the unit tests) still render visible escapes.
fn pal() -> Palette {
    *PALETTE.get().unwrap_or(&COLOR)
}

/// Decide whether color is allowed, following the conventional precedence:
/// explicit `--no-color` < `NO_COLOR` < `TERM=dumb` < "is stdout a TTY".
fn color_enabled(tty: bool, no_color_flag: bool) -> bool {
    if no_color_flag {
        return false;
    }
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if std::env::var_os("TERM").is_some_and(|v| v == "dumb") {
        return false;
    }
    tty
}

/// Install the process-wide palette once, from the resolved capabilities.
fn init_palette(tty: bool, color: bool) {
    IS_TTY.store(tty, Ordering::SeqCst);
    let p = Palette {
        hide_cursor: if tty { COLOR.hide_cursor } else { "" },
        show_cursor: if tty { COLOR.show_cursor } else { "" },
        clear: if tty { COLOR.clear } else { "" },
        home: if tty { COLOR.home } else { "" },
        erase_line: if tty { COLOR.erase_line } else { "" },
        reset: if color { COLOR.reset } else { "" },
        bold: if color { COLOR.bold } else { "" },
        dim: if color { COLOR.dim } else { "" },
        green: if color { COLOR.green } else { "" },
        yellow: if color { COLOR.yellow } else { "" },
        red: if color { COLOR.red } else { "" },
        cyan: if color { COLOR.cyan } else { "" },
    };
    let _ = PALETTE.set(p);
}

const BAR_W: usize = 24;

/// Inner box width — content columns between the `│` borders. Every row is
/// padded to exactly this; a row whose visible width exceeds it overflows the
/// right border. The temperature rows are the widest, so this must hold for a
/// 3-digit (≥100 °C) reading, not just the common 2-digit case.
const INNER: usize = 56;

fn heat(frac: f32) -> &'static str {
    let p = pal();
    if frac < 0.40 {
        p.green
    } else if frac < 0.75 {
        p.yellow
    } else {
        p.red
    }
}

fn bar(val: f32, max: f32) -> String {
    let Palette { dim, reset, .. } = pal();
    let frac = (val / max).clamp(0.0, 1.0);
    let n = (frac * BAR_W as f32).round() as usize;
    // No gap between filled and empty — total is always exactly BAR_W chars.
    format!(
        "{}{}{dim}{}{reset}",
        heat(frac),
        "█".repeat(n),
        "░".repeat(BAR_W - n)
    )
}

/// Left column shared by every row: `  LABEL NN%  `.
/// Fixed visual width so bars line up across sections.
fn left_col(label: &str, pct: f32) -> String {
    let Palette {
        bold, dim, reset, ..
    } = pal();
    format!(
        "  {bold}{label:<5}{reset} {dim}{:3.0}%{reset}  ",
        pct.clamp(0.0, 100.0),
    )
}

fn power_row(label: &str, watts: f32, max: f32) -> String {
    let Palette { cyan, reset, .. } = pal();
    let pct = (watts / max * 100.0).clamp(0.0, 100.0);
    format!(
        "{}{}  {cyan}{watts:5.2} W{reset}",
        left_col(label, pct),
        bar(watts, max),
    )
}

/// Power row with a trailing temperature badge: `... 1.24  W  (46°C)`.
/// `live=false` dims the badge to indicate a cached (rail-gated) reading.
fn power_row_temp(label: &str, watts: f32, max: f32, temp: f32, live: bool) -> String {
    let Palette {
        cyan, dim, reset, ..
    } = pal();
    let pct = (watts / max * 100.0).clamp(0.0, 100.0);
    format!(
        "{}{}  {cyan}{watts:5.2} W{reset}  {dim}({reset}{}{dim}){reset}",
        left_col(label, pct),
        bar(watts, max),
        temp_str(temp, live),
    )
}

fn util_row(label: &str, util: f32, freq_mhz: u32) -> String {
    let Palette { cyan, reset, .. } = pal();
    format!(
        "{}{}  {cyan}{freq_mhz:4} MHz{reset}",
        left_col(label, util * 100.0),
        bar(util, 1.0),
    )
}

fn fan_row(rpm: u32, max_rpm: u32) -> String {
    let Palette {
        bold,
        dim,
        reset,
        cyan,
        ..
    } = pal();
    if max_rpm == 0 {
        return format!(
            "  {bold}FAN{reset}    {dim}—  {}  fanless{reset}",
            "─".repeat(BAR_W),
        );
    }
    let duty = (rpm as f32 / max_rpm as f32).clamp(0.0, 1.0);
    format!(
        "{}{}  {cyan}{rpm:>4} RPM{reset}",
        left_col("FAN", duty * 100.0),
        bar(duty, 1.0),
    )
}

fn mem_row(label: &str, used: u64, total: u64) -> String {
    let Palette {
        cyan, dim, reset, ..
    } = pal();
    let pct = if total > 0 {
        used as f32 / total as f32
    } else {
        0.0
    };
    format!(
        "{}{}  {cyan}{}{reset} {dim}/ {} GB{reset}",
        left_col(label, pct * 100.0),
        bar(pct, 1.0),
        fmt_gb(used),
        fmt_gb(total),
    )
}

fn temp_str(t: f32, live: bool) -> String {
    let Palette {
        dim,
        reset,
        green,
        yellow,
        red,
        ..
    } = pal();
    if t.is_nan() {
        return format!("{dim}--°C{reset}");
    }
    if !live {
        return format!("{dim}{t:.0}°C{reset}");
    }
    let color = if t < 70.0 {
        green
    } else if t < 85.0 {
        yellow
    } else {
        red
    };
    format!("{color}{t:.0}°C{reset}")
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
    let Palette {
        erase_line,
        dim,
        reset,
        ..
    } = pal();
    let vlen = visual_len(content);
    let pad = inner.saturating_sub(vlen);
    writeln!(
        out,
        "{erase_line}{dim}│{reset}{content}{}{dim}│{reset}",
        " ".repeat(pad)
    )
    .ok();
}

/// Write an empty box line (blank row inside the border).
fn box_empty(out: &mut impl Write, inner: usize) {
    let Palette {
        erase_line,
        dim,
        reset,
        ..
    } = pal();
    writeln!(
        out,
        "{erase_line}{dim}│{reset}{}{dim}│{reset}",
        " ".repeat(inner)
    )
    .ok();
}

/// Write a full-width horizontal rule inside the box: `│──────│`.
fn box_rule(out: &mut impl Write, inner: usize) {
    let Palette {
        erase_line,
        dim,
        reset,
        ..
    } = pal();
    writeln!(out, "{erase_line}{dim}│{}│{reset}", "─".repeat(inner)).ok();
}

// ── Cursor guard ──────────────────────────────────────────────────────────────

struct Screen;

impl Screen {
    fn open() -> Self {
        let p = pal();
        print!("{}{}{}", p.hide_cursor, p.clear, p.home);
        std::io::stdout().flush().ok();
        Screen
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let p = pal();
        println!("{}{}", p.show_cursor, p.reset);
        std::io::stdout().flush().ok();
    }
}

// ── Signals ────────────────────────────────────────────────────────────────────

/// Cleared by SIGINT/SIGTERM so the render loop can exit and let `Screen`'s
/// `Drop` restore the terminal — a default-handled signal would kill the
/// process mid-loop and leave the cursor hidden.
static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn handle_signal(_sig: i32) {
    // Restoring the cursor here is async-signal-safe (a raw `write` syscall),
    // so the terminal is usable the instant Ctrl-C lands — even though the
    // process takes up to one sample window to actually unwind and exit.
    if IS_TTY.load(Ordering::SeqCst) {
        const SHOW: &[u8] = b"\x1b[?25h\x1b[0m";
        unsafe {
            write(1, SHOW.as_ptr(), SHOW.len());
        }
    }
    RUNNING.store(false, Ordering::SeqCst);
}

fn install_signal_handlers() {
    unsafe {
        signal(SIGINT, handle_signal as *const () as usize); // Ctrl-C
        signal(SIGTERM, handle_signal as *const () as usize);
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

/// `0.1.0 (built <ts>, <git>)` — the build-stamped version detail. `PM_GIT`
/// and `PM_BUILD_TIME` are injected by `build.rs`.
fn version_detail() -> String {
    format!(
        "{} (built {}, {})",
        env!("CARGO_PKG_VERSION"),
        env!("PM_BUILD_TIME"),
        env!("PM_GIT"),
    )
}

/// Top-level help, in the phonon section layout: NAME / USAGE / DESCRIPTION /
/// COMMANDS / GLOBAL OPTIONS / EXAMPLES / LEARN MORE / SUPPORT / COPYRIGHT.
fn write_usage(w: &mut impl Write) {
    let _ = writeln!(w, "NAME:");
    let _ = writeln!(
        w,
        "   power-monitor - command line for Apple Silicon power & performance monitoring"
    );
    let _ = writeln!(w, "   version {}", version_detail());
    let _ = writeln!(w);
    let _ = writeln!(w, "USAGE:");
    let _ = writeln!(w, "   power-monitor [OPTIONS] [COMMAND]");
    let _ = writeln!(w);
    let _ = writeln!(w, "DESCRIPTION:");
    let _ = writeln!(
        w,
        "   power-monitor reads power, temperature, fan RPM, CPU/GPU utilisation,"
    );
    let _ = writeln!(
        w,
        "   frequency, voltage, current, battery, and RAM/swap on Apple Silicon by"
    );
    let _ = writeln!(
        w,
        "   talking directly to AppleSMC, IOReport, and IOKit over FFI — no"
    );
    let _ = writeln!(w, "   subprocesses and no sudo for reads.");
    let _ = writeln!(w);
    let _ = writeln!(
        w,
        "   With no command it renders a live terminal dashboard."
    );
    let _ = writeln!(w);
    let _ = writeln!(w, "COMMANDS:");
    let _ = writeln!(w, "  pipe          Stream metrics to stdout as NDJSON");
    let _ = writeln!(
        w,
        "  serve         Serve JSON + Prometheus metrics over HTTP"
    );
    let _ = writeln!(
        w,
        "  collect       Aggregate many agents into one fleet dashboard"
    );
    let _ = writeln!(
        w,
        "  fan           Control fan speed: max | auto (requires root)"
    );
    let _ = writeln!(
        w,
        "  doctor        Run health checks on the monitoring subsystems"
    );
    let _ = writeln!(w, "  man           Print or install the man page");
    let _ = writeln!(
        w,
        "  completion    Generate shell completions (bash | zsh | fish)"
    );
    let _ = writeln!(
        w,
        "  help          Print this message or the help of the given subcommand(s)"
    );
    let _ = writeln!(w);
    let _ = writeln!(w, "GLOBAL OPTIONS:");
    let _ = writeln!(w, "      --no-color   Disable ANSI colors [env: NO_COLOR=]");
    let _ = writeln!(w, "  -h, --help       Print help");
    let _ = writeln!(w, "  -V, --version    Print version");
    let _ = writeln!(w);
    let _ = writeln!(w, "EXAMPLES:");
    let _ = writeln!(
        w,
        "   $ power-monitor                       # live dashboard"
    );
    let _ = writeln!(
        w,
        "   $ power-monitor pipe | jq             # stream NDJSON metrics"
    );
    let _ = writeln!(
        w,
        "   $ power-monitor pipe -s 10 -i 500     # 10 samples, 500 ms apart"
    );
    let _ = writeln!(
        w,
        "   $ power-monitor serve --bind 0.0.0.0  # HTTP + Prometheus exporter"
    );
    let _ = writeln!(
        w,
        "   $ power-monitor collect --tailnet     # fleet view across a tailnet"
    );
    let _ = writeln!(
        w,
        "   $ sudo power-monitor fan max          # pin fans to full"
    );
    let _ = writeln!(w);
    let _ = writeln!(w, "LEARN MORE:");
    let _ = writeln!(
        w,
        "   Use `power-monitor <command> --help` for details on any command."
    );
    let _ = writeln!(
        w,
        "   Run `power-monitor doctor` to verify subsystem access."
    );
    let _ = writeln!(
        w,
        "   Read the full docs at https://github.com/electricapp/power-monitor"
    );
    let _ = writeln!(w);
    let _ = writeln!(w, "SUPPORT:");
    let _ = writeln!(
        w,
        "   Report bugs at https://github.com/electricapp/power-monitor/issues"
    );
    let _ = writeln!(w);
    let _ = writeln!(w, "COPYRIGHT:");
    let _ = writeln!(w, "   (c) 2026 electricapp. Licensed MIT or Apache-2.0.");
}

/// Route `--help` / `help [command]` to the right usage text, on **stdout**.
fn print_help_for(sub: Option<&str>) {
    let mut out = std::io::stdout().lock();
    match sub {
        Some("pipe") => pipe_cmd::write_usage(&mut out),
        Some("serve") => serve_cmd::write_usage(&mut out),
        Some("collect") => collect_cmd::write_usage(&mut out),
        Some("fan") => fan_cmd::write_usage(&mut out),
        Some("doctor") => doctor_cmd::write_usage(&mut out),
        Some("man") => man_cmd::write_usage(&mut out),
        Some("completion") => completion_cmd::write_usage(&mut out),
        Some(other) => {
            let _ = writeln!(out, "No help topic for '{other}'.\n");
            write_usage(&mut out);
        }
        None => write_usage(&mut out),
    }
}

/// Levenshtein edit distance, for "did you mean" command suggestions.
fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Nearest known command within edit distance 2, if any.
fn suggest_command(input: &str) -> Option<&'static str> {
    const CMDS: [&str; 9] = [
        "pipe",
        "serve",
        "collect",
        "fan",
        "doctor",
        "man",
        "completion",
        "help",
        "version",
    ];
    CMDS.iter()
        .copied()
        .map(|c| (c, levenshtein(input, c)))
        .filter(|&(_, d)| d <= 2)
        .min_by_key(|&(_, d)| d)
        .map(|(c, _)| c)
}

fn main() {
    // Behave like a normal Unix tool when a downstream pipe closes early.
    reset_sigpipe();

    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("pipe") => return pipe_cmd::run(&args[1..]),
        Some("serve") => return serve_cmd::run(&args[1..]),
        Some("collect") => return collect_cmd::run(&args[1..]),
        Some("fan") => return fan_cmd::run(&args[1..]),
        Some("doctor") => return doctor_cmd::run(&args[1..]),
        Some("man") => return man_cmd::run(&args[1..]),
        Some("completion") => return completion_cmd::run(&args[1..]),
        Some("help") | Some("-h") | Some("--help") => {
            // `help <command>` shows that command's help; otherwise top-level.
            let sub = if args[0] == "help" {
                args.get(1).map(String::as_str)
            } else {
                None
            };
            print_help_for(sub);
            return;
        }
        Some("-V") | Some("--version") => {
            println!("power-monitor {}", version_detail());
            return;
        }
        // A leading-dash token that isn't a known global flag — fall through
        // to the dashboard's own option parsing (it accepts --no-color).
        Some(s) if s.starts_with('-') => {}
        Some(unknown) => {
            eprintln!("error: unknown command: {unknown}");
            if let Some(sug) = suggest_command(unknown) {
                eprintln!("did you mean '{sug}'?");
            }
            eprintln!();
            write_usage(&mut std::io::stderr().lock());
            std::process::exit(2);
        }
        None => {} // fall through to TUI
    }

    // ── Bare dashboard: parse the handful of options it accepts. ──
    let mut no_color = false;
    for a in &args {
        match a.as_str() {
            "--no-color" => no_color = true,
            other => {
                eprintln!("error: unknown option for the dashboard: {other}");
                eprintln!("run 'power-monitor --help' for usage");
                std::process::exit(2);
            }
        }
    }

    let tty = std::io::stdout().is_terminal();
    init_palette(tty, color_enabled(tty, no_color));
    if !tty {
        eprintln!(
            "note: the live dashboard is meant for a terminal; for scripts use 'power-monitor pipe'"
        );
    }
    // Restore the cursor and exit cleanly on Ctrl-C / SIGTERM.
    install_signal_handlers();

    let mut sampler = Sampler::new().unwrap_or_else(|| {
        eprintln!("error: failed to open Sampler (power-monitoring subsystems)");
        std::process::exit(1);
    });
    let _screen = Screen::open();
    let mut out = std::io::stdout().lock();
    let host = hostname();
    let version = env!("CARGO_PKG_VERSION");

    // Draw an immediate placeholder so the screen isn't blank for the first
    // sample window (~1s). The first real frame overwrites it from HOME.
    {
        let p = pal();
        write!(
            out,
            "{}{}collecting first sample…{}",
            p.home, p.dim, p.reset
        )
        .ok();
        out.flush().ok();
    }
    // Cached last-known temps so the badge holds a stale (dim) value
    // through rail-gated windows instead of blanking.
    let mut last_cpu_temp = f32::NAN;
    let mut last_gpu_temp = f32::NAN;

    while RUNNING.load(Ordering::Relaxed) {
        let m = sampler.get_metrics(1000);
        // A signal may have arrived during the sample window; if so, break
        // before drawing so `Screen`'s Drop restores the terminal cleanly.
        if !RUNNING.load(Ordering::Relaxed) {
            break;
        }
        let (cpu_disp, cpu_live) = display_temp(m.cpu_temp, &mut last_cpu_temp);
        let (gpu_disp, gpu_live) = display_temp(m.gpu_temp, &mut last_gpu_temp);

        let Palette {
            home,
            erase_line,
            bold,
            dim,
            reset,
            ..
        } = pal();
        write!(out, "{home}").ok();

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
            "{erase_line}{dim}╭─{reset}{bold}{title}{reset}{dim}{pad}╮{reset}",
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
            "{erase_line}{dim}╰{sys_str}{pad}{interval_str}╯{reset}",
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

    #[test]
    fn rows_never_exceed_box_width() {
        // Every row must fit INNER so it can't overflow the right border.
        // The temp rows are the widest; a 3-digit (≥100 °C) reading is the
        // case that used to break the box.
        for &t in &[46.0_f32, 100.0, 105.0, f32::NAN] {
            for &live in &[true, false] {
                let row = power_row_temp("GPU", 12.34, 16.0, t, live);
                assert!(
                    visual_len(&row) <= INNER,
                    "temp row width {} > INNER {INNER} (temp={t}, live={live})",
                    visual_len(&row),
                );
            }
        }
        // Plain power rows and the high-magnitude SYS row too.
        assert!(visual_len(&power_row("SYS", 199.99, 40.0)) <= INNER);
        assert!(visual_len(&util_row("PCPU", 1.0, 9999)) <= INNER);
        assert!(visual_len(&mem_row("MEM", 99 << 30, 128 << 30)) <= INNER);
        assert!(visual_len(&fan_row(6550, 6550)) <= INNER);
    }
}
