//! `power-monitor doctor` — health checks on the monitoring subsystems.
//!
//! Verifies the preconditions every other command depends on: the CPU
//! architecture, AppleSMC access, and a live IOReport sample. Prints a
//! checklist and exits non-zero if any critical check fails — useful on a
//! fresh box or in CI.

use std::io::{IsTerminal, Write};

use power_monitor::{Sampler, Smc};

pub(crate) fn write_usage(w: &mut impl Write) {
    let _ = writeln!(w, "Usage: power-monitor doctor [--no-color]");
    let _ = writeln!(w);
    let _ = writeln!(
        w,
        "Run health checks on the power-monitoring subsystems and print a"
    );
    let _ = writeln!(
        w,
        "checklist. Exits 0 if every critical check passes, 1 otherwise."
    );
}

struct Paint {
    ok: &'static str,
    bad: &'static str,
    dim: &'static str,
    reset: &'static str,
}

fn paint(no_color: bool) -> Paint {
    let color = !no_color
        && std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
        && std::env::var_os("TERM").is_none_or(|v| v != "dumb");
    if color {
        Paint {
            ok: "\x1b[32m",
            bad: "\x1b[31m",
            dim: "\x1b[2m",
            reset: "\x1b[0m",
        }
    } else {
        Paint {
            ok: "",
            bad: "",
            dim: "",
            reset: "",
        }
    }
}

pub fn run(args: &[String]) {
    let mut no_color = false;
    for a in args {
        match a.as_str() {
            "-h" | "--help" => {
                write_usage(&mut std::io::stdout().lock());
                return;
            }
            "--no-color" => no_color = true,
            other => {
                eprintln!("error: unknown argument for doctor: {other}");
                write_usage(&mut std::io::stderr().lock());
                std::process::exit(2);
            }
        }
    }

    let p = paint(no_color);
    let mut out = std::io::stdout().lock();
    let mut failed = false;

    let _ = writeln!(out, "power-monitor doctor");
    let _ = writeln!(out);

    // 1. Architecture — every sensor path assumes Apple Silicon.
    if cfg!(target_arch = "aarch64") {
        check(
            &mut out,
            &p,
            true,
            "CPU architecture",
            "Apple Silicon (aarch64)",
        );
    } else {
        check(
            &mut out,
            &p,
            false,
            "CPU architecture",
            "not aarch64 — most sensors will be absent",
        );
        failed = true;
    }

    // 2. AppleSMC — temperatures, fans, voltages, currents, battery.
    match Smc::open() {
        Ok(smc) => {
            let fans = smc.fan_count();
            let (cpu_t, gpu_t) = smc.read_cpu_gpu_temps();
            let detail = format!("opened; {fans} fan(s), CPU {cpu_t:.0}°C / GPU {gpu_t:.0}°C");
            check(&mut out, &p, true, "AppleSMC", &detail);
        }
        Err(e) => {
            check(&mut out, &p, false, "AppleSMC", &format!("{e}"));
            failed = true;
        }
    }

    // 3. IOReport — power rails and CPU/GPU utilisation. Take one quick sample.
    match Sampler::new() {
        Some(mut s) => {
            let chip = s.soc.chip_name.clone();
            let pcpu = s.soc.pcpu_level().map_or(0, |l| l.cores);
            let ecpu = s.soc.ecpu_level().map_or(0, |l| l.cores);
            let gpu = s.soc.gpu_cores;
            let m = s.get_metrics(200);
            let detail = format!(
                "{chip} · {pcpu}P+{ecpu}E · {gpu} GPU · sys {:.2} W",
                m.sys_power
            );
            check(&mut out, &p, true, "IOReport sampler", &detail);
        }
        None => {
            check(
                &mut out,
                &p,
                false,
                "IOReport sampler",
                "failed to open the energy subscription",
            );
            failed = true;
        }
    }

    let _ = writeln!(out);
    if failed {
        let _ = writeln!(out, "{}✗ some checks failed{}", p.bad, p.reset);
        std::process::exit(1);
    }
    let _ = writeln!(out, "{}✓ all checks passed{}", p.ok, p.reset);
}

fn check(out: &mut impl Write, p: &Paint, ok: bool, name: &str, detail: &str) {
    let (mark, col) = if ok { ("✓", p.ok) } else { ("✗", p.bad) };
    let (dim, reset) = (p.dim, p.reset);
    let _ = writeln!(out, "  {col}{mark}{reset} {name:<18} {dim}{detail}{reset}");
}
