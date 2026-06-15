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

// ── Bearer-token plumbing (shared by `serve` and `collect`) ──────────────────
//
// A `--auth TOKEN` flag leaks the secret into `ps` output and shell history,
// and `--install` would persist it in plaintext in the launchd plist. The
// `--auth-file` alternative keeps the token in a file; on install we pass the
// *path* through to the agent rather than the token itself.

/// Where a bearer token came from on the command line — preserved through to
/// `--install` so the plist references the file instead of inlining the secret.
pub enum AuthArg<'a> {
    None,
    Inline(&'a str),
    File(&'a str),
}

/// Read a bearer token from a file: the first line, trimmed. Exits with code 1
/// on an I/O error or an empty file.
pub fn read_auth_file(path: &str) -> String {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let tok = s.lines().next().unwrap_or("").trim().to_string();
            if tok.is_empty() {
                eprintln!("error: auth file '{path}' is empty");
                std::process::exit(1);
            }
            tok
        }
        Err(e) => {
            eprintln!("error: could not read auth file '{path}': {e}");
            std::process::exit(1);
        }
    }
}

/// Reject `--auth` and `--auth-file` being passed together. Exits with code 2.
pub fn check_auth_exclusive(inline: &Option<String>, file: &Option<String>) {
    if inline.is_some() && file.is_some() {
        eprintln!("error: --auth and --auth-file are mutually exclusive");
        std::process::exit(2);
    }
}

/// Resolve the two auth flags into the runtime token. Assumes
/// [`check_auth_exclusive`] has already run.
pub fn resolve_auth(inline: Option<String>, file: Option<String>) -> Option<String> {
    match (inline, file) {
        (Some(t), _) => Some(t),
        (None, Some(p)) => Some(read_auth_file(&p)),
        (None, None) => None,
    }
}
