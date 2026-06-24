use power_monitor::Smc;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

unsafe extern "C" {
    fn signal(sig: i32, handler: usize) -> usize;
}

static RUNNING: AtomicBool = AtomicBool::new(true);

/// Set by [`AutoRestore`]'s destructor when restoring automatic control fails,
/// so `run_max` can surface a non-zero exit code after the guard has dropped.
/// (A `Drop` impl can't set the exit code itself without `process::exit`, which
/// would skip every other destructor.)
static RESTORE_FAILED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_signal(_sig: i32) {
    RUNNING.store(false, Ordering::SeqCst);
}

pub(crate) fn write_usage(w: &mut impl Write) {
    let _ = writeln!(w, "Usage: power-monitor fan <max|auto>");
    let _ = writeln!(w);
    let _ = writeln!(w, "Control fan speed. Requires root — run with sudo.");
    let _ = writeln!(w);
    let _ = writeln!(w, "Subcommands:");
    let _ = writeln!(
        w,
        "  max    Force all fans to maximum RPM (hold until SIGINT/SIGTERM)"
    );
    let _ = writeln!(w, "  auto   Restore automatic fan control");
    let _ = writeln!(w);
    let _ = writeln!(w, "Examples:");
    let _ = writeln!(w, "  sudo power-monitor fan max");
    let _ = writeln!(w, "  sudo power-monitor fan auto");
}

pub fn run(args: &[String]) {
    match args.first().map(String::as_str) {
        // `max`/`auto` take no further arguments; reject extras instead of
        // silently ignoring them, matching the rest of the CLI's exit-2 policy.
        Some(sub @ ("max" | "auto")) if args.len() > 1 => {
            eprintln!("error: fan {sub} takes no arguments");
            write_usage(&mut std::io::stderr().lock());
            std::process::exit(2);
        }
        Some("max") => run_max(),
        Some("auto") => run_auto(),
        Some("-h") | Some("--help") => write_usage(&mut std::io::stdout().lock()),
        other => {
            if let Some(arg) = other {
                eprintln!("error: unknown fan command: {arg}");
            } else {
                eprintln!("error: fan requires a subcommand");
            }
            write_usage(&mut std::io::stderr().lock());
            std::process::exit(2);
        }
    }
}

/// Restores automatic fan control on drop — covers the normal signal exit, an
/// early return, or a panic unwind alike. One rule: don't `process::exit()`
/// while this is alive, as `exit` skips destructors.
///
/// A failed restore sets [`RESTORE_FAILED`]; `run_max` reads it after the guard
/// drops and exits non-zero so a supervisor/script can tell the fans may still
/// be forced.
struct AutoRestore<'a>(&'a Smc);

impl Drop for AutoRestore<'_> {
    fn drop(&mut self) {
        match self.0.set_fans_auto() {
            Ok(()) => eprintln!("fans restored to automatic control"),
            Err(e) => {
                eprintln!("warning: failed to restore auto: {e}");
                RESTORE_FAILED.store(true, Ordering::SeqCst);
            }
        }
    }
}

fn run_max() {
    let smc = Smc::open().unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });

    let count = smc.fan_count();
    if count == 0 {
        eprintln!("no fans detected");
        return;
    }

    unsafe {
        signal(1, handle_signal as *const () as usize); // SIGHUP  (terminal closed)
        signal(2, handle_signal as *const () as usize); // SIGINT  (Ctrl-C)
        signal(3, handle_signal as *const () as usize); // SIGQUIT (Ctrl-\)
        signal(15, handle_signal as *const () as usize); // SIGTERM
    }

    if let Err(e) = smc.set_fans_max() {
        eprintln!("error: {e}");
        eprintln!("hint: SMC writes require root — try running with sudo");
        // set_fans_max restores on partial failure, so the fans aren't left
        // forced here — exiting before the guard is installed is safe.
        std::process::exit(1);
    }

    // Fans are now forced. The guard restores automatic control on every exit
    // from this scope: the SIGINT/SIGTERM path below, an early return, or a
    // panic unwind. Must be a *named* binding — `let _` would drop it instantly.
    let _restore = AutoRestore(&smc);

    for i in 0..count {
        let max = smc.fan_max_rpm(i);
        eprintln!("fan {i}: forced to {max:.0} RPM");
    }
    eprintln!(
        "holding — send SIGINT/SIGTERM/SIGQUIT or close the terminal (SIGHUP) to restore automatic control"
    );

    while RUNNING.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    eprintln!();
    // Drop the guard explicitly so its restore runs *before* we inspect the
    // result and pick an exit code.
    drop(_restore);
    if RESTORE_FAILED.load(Ordering::SeqCst) {
        std::process::exit(1);
    }
}

fn run_auto() {
    let smc = Smc::open().unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });

    if smc.fan_count() == 0 {
        eprintln!("no fans detected");
        return;
    }

    if let Err(e) = smc.set_fans_auto() {
        eprintln!("error: {e}");
        eprintln!("hint: SMC writes require root — try running with sudo");
        std::process::exit(1);
    }
    eprintln!("fans restored to automatic control");
}
