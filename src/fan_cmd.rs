use power_monitor::Smc;
use std::sync::atomic::{AtomicBool, Ordering};

unsafe extern "C" {
    fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
}

static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn handle_signal(_sig: i32) {
    RUNNING.store(false, Ordering::SeqCst);
}

pub fn run(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("max") => run_max(),
        Some("auto") => run_auto(),
        _ => {
            eprintln!("Usage: power-monitor fan <max|auto>");
            eprintln!("  max   Set all fans to maximum RPM (hold until SIGTERM/SIGINT)");
            eprintln!("  auto  Restore automatic fan control");
            std::process::exit(2);
        }
    }
}

/// Restores automatic fan control on drop — covers the normal signal exit, an
/// early return, or a panic unwind alike. One rule: don't `process::exit()`
/// while this is alive, as `exit` skips destructors.
///
// TODO(exit-code): a failed restore (near-impossible) warns to stderr but still
// exits 0 — Drop can't set a non-zero code without process::exit (which skips
// other destructors). If scripting ever needs the non-zero code, have the guard
// set a shared "restore failed" flag and read it in `main` after the hold loop.
struct AutoRestore<'a>(&'a Smc);

impl Drop for AutoRestore<'_> {
    fn drop(&mut self) {
        match self.0.set_fans_auto() {
            Ok(()) => eprintln!("fans restored to automatic control"),
            Err(e) => eprintln!("warning: failed to restore auto: {e}"),
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
        signal(2, handle_signal); // SIGINT
        signal(15, handle_signal); // SIGTERM
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
    eprintln!("holding — send SIGTERM or SIGINT to restore automatic control");

    while RUNNING.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    eprintln!();
    // `_restore` drops here → set_fans_auto.
}

fn run_auto() {
    let smc = Smc::open().unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });

    if let Err(e) = smc.set_fans_auto() {
        eprintln!("error: {e}");
        eprintln!("hint: SMC writes require root — try running with sudo");
        std::process::exit(1);
    }
    eprintln!("fans restored to automatic control");
}
