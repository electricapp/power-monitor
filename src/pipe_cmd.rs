//! `power-monitor pipe` subcommand: emit one JSON line per sample window
//! to stdout, suitable for line-oriented log shippers (vector, fluent-bit,
//! or a downstream process via stdout pipe).
//!
//! All serialization helpers live in [`power_monitor::serialize`]; this
//! module owns only the argv parsing and the alloc-free output loop.

use std::io::Write;

use power_monitor::serialize::{AgentInfo, write_metrics_json};

fn print_usage() {
    eprintln!("Usage: power-monitor pipe [-s <samples>] [-i <interval_ms>]");
    eprintln!("  -s, --samples   N samples then exit (default 0 = infinite)");
    eprintln!("  -i, --interval  Sampling window in ms (default 1000)");
}

/// Entry point for `power-monitor pipe`.
pub fn run(args: &[String]) {
    use crate::args;

    let mut samples: u64 = 0; // 0 = infinite
    let mut interval_ms: u64 = 1000;

    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "-s" | "--samples" => samples = args::parse_value(args, &mut i, "--samples", "samples"),
            "-i" | "--interval" => {
                interval_ms = args::parse_value(args, &mut i, "--interval", "interval")
            }
            "-h" | "--help" => {
                print_usage();
                return;
            }
            other => args::unknown_arg(other),
        }
        i += 1;
    }

    let mut sampler = power_monitor::Sampler::new().unwrap_or_else(|| {
        eprintln!("error: failed to open Sampler");
        std::process::exit(1);
    });

    let info = AgentInfo::from_sampler(&sampler, interval_ms);
    let stdout = std::io::stdout();
    // Reused per-frame: capacity covers a typical 540-byte payload + newline,
    // so the loop allocates zero times per tick after the first frame.
    let mut buf = String::with_capacity(640);

    let mut count: u64 = 0;
    loop {
        let m = sampler.get_metrics(interval_ms);
        buf.clear();
        let _ = write_metrics_json(&mut buf, &m, &info);
        buf.push('\n');
        {
            let mut out = stdout.lock();
            out.write_all(buf.as_bytes()).ok();
            out.flush().ok();
        }
        count += 1;
        if samples > 0 && count >= samples {
            break;
        }
    }
}
