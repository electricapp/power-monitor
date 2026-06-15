//! `power-monitor man` — print or install the bundled man page.
//!
//! The troff source is embedded at compile time, so the page always matches
//! the installed binary and works offline.

use std::io::Write;

const MAN_PAGE: &str = include_str!("../man/power-monitor.1");

pub(crate) fn write_usage(w: &mut impl Write) {
    let _ = writeln!(w, "Usage: power-monitor man [--install [DIR]]");
    let _ = writeln!(w);
    let _ = writeln!(
        w,
        "Print the man page to stdout, or install it for `man power-monitor`."
    );
    let _ = writeln!(w);
    let _ = writeln!(w, "Options:");
    let _ = writeln!(
        w,
        "      --install [DIR]  Install power-monitor.1 into DIR/man1"
    );
    let _ = writeln!(w, "                       (default: ~/.local/share/man)");
    let _ = writeln!(w, "  -h, --help           Show this help");
    let _ = writeln!(w);
    let _ = writeln!(w, "Examples:");
    let _ = writeln!(
        w,
        "  power-monitor man | man -l -                     # preview"
    );
    let _ = writeln!(
        w,
        "  power-monitor man --install                      # ~/.local/share/man/man1"
    );
    let _ = writeln!(w, "  sudo power-monitor man --install /usr/local/share/man");
}

pub fn run(args: &[String]) {
    match args.first().map(String::as_str) {
        None => {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(MAN_PAGE.as_bytes());
        }
        Some("-h") | Some("--help") => write_usage(&mut std::io::stdout().lock()),
        Some("--install") => install(args.get(1).map(String::as_str)),
        Some(other) => {
            eprintln!("error: unknown argument for man: {other}");
            write_usage(&mut std::io::stderr().lock());
            std::process::exit(2);
        }
    }
}

fn install(base: Option<&str>) {
    let base_dir = match base {
        Some(d) => std::path::PathBuf::from(d),
        None => match std::env::var_os("HOME") {
            Some(h) => std::path::PathBuf::from(h).join(".local/share/man"),
            None => {
                eprintln!("error: could not determine HOME; pass an explicit DIR");
                std::process::exit(1);
            }
        },
    };

    let man1 = base_dir.join("man1");
    if let Err(e) = std::fs::create_dir_all(&man1) {
        eprintln!("error: could not create {}: {e}", man1.display());
        std::process::exit(1);
    }

    let dest = man1.join("power-monitor.1");
    if let Err(e) = std::fs::write(&dest, MAN_PAGE) {
        eprintln!("error: could not write {}: {e}", dest.display());
        std::process::exit(1);
    }

    println!("installed man page: {}", dest.display());
    println!("if `man power-monitor` can't find it, add to MANPATH:");
    println!("  export MANPATH=\"{}:$MANPATH\"", base_dir.display());
}
