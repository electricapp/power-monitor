//! Hardened argv parsing helpers shared by `pipe`, `serve`, and `collect`.
//!
//! Single error policy: invalid input prints a one-line diagnostic to stderr
//! and exits with status 2 (the conventional "usage error" code). The previous
//! "warn and silently fall back to the default" pattern was confusing UX —
//! the value the user thought they passed wasn't the value used, but the
//! program kept running.

use std::str::FromStr;

/// Consume the next argv element after `args[*i]`, advancing `*i` past it.
///
/// Exits with code 2 if no value follows the flag.
pub fn take_value<'a>(args: &'a [String], i: &mut usize, flag: &str) -> &'a str {
    *i += 1;
    match args.get(*i) {
        Some(v) => v.as_str(),
        None => {
            eprintln!("error: {flag} requires a value");
            std::process::exit(2);
        }
    }
}

/// Like [`take_value`], but parses the value as `T`. Exits with code 2 on
/// either a missing or unparseable value, with the parse error in the message.
///
/// `kind` is a short noun for the error message (e.g. `"port"`, `"interval"`).
pub fn parse_value<T: FromStr>(args: &[String], i: &mut usize, flag: &str, kind: &str) -> T
where
    T::Err: std::fmt::Display,
{
    let raw = take_value(args, i, flag);
    raw.parse::<T>().unwrap_or_else(|e| {
        eprintln!("error: invalid {kind} for {flag}: '{raw}' ({e})");
        std::process::exit(2);
    })
}

/// Print an unknown-argument error and exit with code 2.
pub fn unknown_arg(arg: &str) -> ! {
    eprintln!("error: unknown argument: {arg}");
    std::process::exit(2);
}
