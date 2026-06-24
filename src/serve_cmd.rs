use std::fmt::Write as _;
use std::io::Write;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use power_monitor::Metrics;

use crate::http::{
    MAX_INFLIGHT, constant_time_eq, extract_bearer, extract_path, http_response, read_request_head,
    try_acquire,
};
use crate::seqlock::SeqLock;
use power_monitor::serialize::{
    AgentInfo, PROM_GAUGES, metrics_to_json, utc_now, write_prom_label,
};

// ── Prometheus format ─────────────────────────────────────────────────────────

fn metrics_to_prometheus(m: &Metrics, chip: &str, host: &str) -> String {
    // 19 gauges × ~80 bytes/line × ~3 lines/gauge ≈ 4 KiB. Preallocate.
    let mut out = String::with_capacity(4096);
    for &(name, help, value) in PROM_GAUGES {
        let _ = write!(
            out,
            "# HELP power_monitor_{name} {help}\n# TYPE power_monitor_{name} gauge\n"
        );
        let v = value(m);
        // Skip non-finite samples entirely (rail-gated temps are NaN; ±inf
        // would be invalid exposition text) rather than emit a bad line.
        if v.is_finite() {
            let _ = write!(out, "power_monitor_{name}{{chip=\"");
            let _ = write_prom_label(&mut out, chip);
            let _ = write!(out, "\",host=\"");
            let _ = write_prom_label(&mut out, host);
            let _ = writeln!(out, "\"}} {v:.3}");
        }
    }
    out
}

// ── launchd install/uninstall ─────────────────────────────────────────────────

fn plist_path() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(std::path::PathBuf::from(home).join("Library/LaunchAgents/com.power-monitor.plist"))
}

fn do_install(bind: &str, port: u16, interval_ms: u64, auth: crate::args::AuthArg) {
    use crate::args::AuthArg;

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: could not determine exe path: {e}");
            std::process::exit(1);
        }
    };
    let exe_str = exe.to_string_lossy().into_owned();

    let mut args_xml = String::new();
    let mut push = |s: &str| {
        args_xml.push_str("\t\t<string>");
        args_xml.push_str(s);
        args_xml.push_str("</string>\n");
    };
    push(&exe_str);
    push("serve");
    push("--bind");
    push(bind);
    push("--port");
    push(&port.to_string());
    push("--interval");
    push(&interval_ms.to_string());
    match auth {
        AuthArg::None => {}
        // An inline token has to be written into the plist in plaintext —
        // warn, and steer the user toward --auth-file.
        AuthArg::Inline(tok) => {
            eprintln!(
                "warning: storing the auth token in plaintext in the launchd plist; prefer --auth-file"
            );
            push("--auth");
            push(tok);
        }
        // For a file, persist the *path* (absolute, so launchd can find it)
        // rather than the secret itself.
        AuthArg::File(path) => {
            let abs = std::fs::canonicalize(path).unwrap_or_else(|e| {
                eprintln!("error: --auth-file '{path}': {e}");
                std::process::exit(1);
            });
            push("--auth-file");
            push(&abs.to_string_lossy());
        }
    }

    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>com.power-monitor</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n{args_xml}\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         \t<key>KeepAlive</key>\n\
         \t<true/>\n\
         \t<key>StandardErrorPath</key>\n\
         \t<string>/tmp/power-monitor.log</string>\n\
         </dict>\n\
         </plist>\n",
    );

    let path = match plist_path() {
        Some(p) => p,
        None => {
            eprintln!("error: could not determine HOME");
            std::process::exit(1);
        }
    };

    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("error: could not create LaunchAgents dir: {e}");
        std::process::exit(1);
    }

    if let Err(e) = std::fs::write(&path, &plist) {
        eprintln!("error: could not write plist to {}: {e}", path.display());
        std::process::exit(1);
    }

    let status = std::process::Command::new("launchctl")
        .args(["load", &path.to_string_lossy()])
        .status();

    match status {
        Ok(s) if s.success() => println!("installed and started: {}", path.display()),
        Ok(s) => eprintln!(
            "launchctl load exited with status {s}; plist at {}",
            path.display()
        ),
        Err(e) => eprintln!("error: launchctl load failed: {e}"),
    }
}

fn do_uninstall() {
    let path = match plist_path() {
        Some(p) => p,
        None => {
            eprintln!("error: could not determine HOME");
            std::process::exit(1);
        }
    };

    let status = std::process::Command::new("launchctl")
        .args(["unload", &path.to_string_lossy()])
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("warning: launchctl unload exited with status {s}"),
        Err(e) => eprintln!("warning: launchctl unload failed: {e}"),
    }

    match std::fs::remove_file(&path) {
        Ok(()) => println!("uninstalled: {}", path.display()),
        Err(e) => eprintln!("error: could not remove {}: {e}", path.display()),
    }
}

// ── serve subcommand ──────────────────────────────────────────────────────────

pub(crate) fn write_usage(w: &mut impl Write) {
    let _ = writeln!(
        w,
        "Usage: power-monitor serve [--bind <addr>] [-p <port>] [-i <ms>]"
    );
    let _ = writeln!(
        w,
        "                           [--auth <token> | --auth-file <path>] [--install | --uninstall]"
    );
    let _ = writeln!(w);
    let _ = writeln!(w, "Serve a JSON snapshot and Prometheus metrics over HTTP.");
    let _ = writeln!(w);
    let _ = writeln!(w, "Options:");
    let _ = writeln!(
        w,
        "      --bind ADDR    Bind address (default 127.0.0.1; 0.0.0.0 for LAN)"
    );
    let _ = writeln!(w, "  -p, --port N       Listen port (default 9090)");
    let _ = writeln!(
        w,
        "  -i, --interval N   Sampling interval in ms (default 1000)"
    );
    let _ = writeln!(
        w,
        "      --auth TOKEN   Require 'Authorization: Bearer TOKEN' on every request"
    );
    let _ = writeln!(
        w,
        "                     (insecure: visible in ps/shell history — prefer --auth-file)"
    );
    let _ = writeln!(
        w,
        "      --auth-file F  Read the bearer token from the first line of file F"
    );
    let _ = writeln!(
        w,
        "      --install      Install and start as a launchd user agent"
    );
    let _ = writeln!(w, "      --uninstall    Stop and remove the launchd agent");
    let _ = writeln!(w, "  -h, --help         Show this help");
    let _ = writeln!(w);
    let _ = writeln!(w, "Endpoints:  GET /json   GET /metrics");
    let _ = writeln!(w);
    let _ = writeln!(w, "Examples:");
    let _ = writeln!(w, "  power-monitor serve");
    let _ = writeln!(w, "  curl http://127.0.0.1:9090/json");
    let _ = writeln!(
        w,
        "  power-monitor serve --bind 0.0.0.0 --auth-file ~/.pm-token --install"
    );
}

/// Entry point for `power-monitor serve`.
pub fn run(args: &[String]) {
    use crate::args as argp;

    let mut bind: String = "127.0.0.1".to_string();
    let mut port: u16 = 9090;
    let mut interval_ms: u64 = 1000;
    let mut auth_token: Option<String> = None;
    let mut auth_file: Option<String> = None;
    let mut install = false;
    let mut uninstall = false;

    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" => bind = argp::take_value(args, &mut i, "--bind").to_string(),
            "-p" | "--port" => port = argp::parse_value(args, &mut i, "--port", "port"),
            "-i" | "--interval" => {
                interval_ms = argp::parse_value(args, &mut i, "--interval", "interval")
            }
            "--auth" => auth_token = Some(argp::take_value(args, &mut i, "--auth").to_string()),
            "--auth-file" => {
                auth_file = Some(argp::take_value(args, &mut i, "--auth-file").to_string())
            }
            "--install" => install = true,
            "--uninstall" => uninstall = true,
            "-h" | "--help" => {
                write_usage(&mut std::io::stdout().lock());
                return;
            }
            other => argp::unknown_arg(other),
        }
        i += 1;
    }

    argp::check_auth_exclusive(&auth_token, &auth_file);

    if uninstall {
        do_uninstall();
        return;
    }

    argp::require_positive_interval(interval_ms);

    if install {
        let auth_arg = match (&auth_token, &auth_file) {
            (Some(t), _) => argp::AuthArg::Inline(t),
            (None, Some(p)) => argp::AuthArg::File(p),
            (None, None) => argp::AuthArg::None,
        };
        do_install(&bind, port, interval_ms, auth_arg);
        return;
    }

    // Serving path: resolve the file token now (install passed the path through).
    let auth = argp::resolve_auth(auth_token, auth_file);

    // Start sampler on a background thread, updating shared Metrics.
    let mut sampler = power_monitor::Sampler::new().unwrap_or_else(|| {
        eprintln!("error: failed to open Sampler");
        std::process::exit(1);
    });

    let info = AgentInfo::from_sampler(&sampler, interval_ms);
    // Lock-free SPMC: single sampling thread publishes via SeqLock; HTTP
    // handlers snapshot without ever blocking the sampler.
    let shared: Arc<SeqLock<Metrics>> = Arc::new(SeqLock::new(Metrics::default()));

    {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            loop {
                let m = sampler.get_metrics(interval_ms);
                shared.store(m);
            }
        });
    }

    // Bind listener.
    let addr = format!("{bind}:{port}");
    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        eprintln!("error: could not bind {addr}: {e}");
        std::process::exit(1);
    });

    eprintln!("Listening on http://{addr}");
    eprintln!("  GET /json    -- JSON metrics snapshot");
    eprintln!("  GET /metrics -- Prometheus text format");
    if auth.is_some() {
        eprintln!("  (auth required: Authorization: Bearer <token>)");
    }

    let auth = Arc::new(auth);
    let info = Arc::new(info);
    let inflight = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };

        // Slowloris defense: a client that connects but drips (or never sends)
        // the request would otherwise pin a handler thread forever. With only
        // MAX_INFLIGHT slots, a handful of idle connections wedge the server at
        // 503 for everyone. Bound the read so stalled clients are reaped.
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(15)));
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(30)));

        let Some(permit) = try_acquire(&inflight, MAX_INFLIGHT) else {
            let body = "{\"error\":\"server busy\"}";
            let response = http_response("503 Service Unavailable", "application/json", body);
            stream.write_all(&response).ok();
            continue;
        };

        // Thread-per-connection: a slow client must not stall the accept loop.
        let shared = Arc::clone(&shared);
        let auth = Arc::clone(&auth);
        let info = Arc::clone(&info);
        std::thread::spawn(move || {
            let _permit = permit;
            // SeqLock::load is wait-free under a 1 Hz writer — handlers never
            // block the sampler and never block each other.
            let metrics_snapshot = shared.load();
            handle_connection(stream, &metrics_snapshot, &info, auth.as_deref());
        });
    }
}

fn handle_connection(
    mut stream: std::net::TcpStream,
    metrics: &Metrics,
    info: &AgentInfo,
    auth: Option<&str>,
) {
    let buf = read_request_head(&mut stream);
    if buf.is_empty() {
        return;
    }

    // Enforce auth before routing.
    if let Some(required) = auth
        && !constant_time_eq(extract_bearer(&buf), required)
    {
        let body = "{\"error\":\"unauthorized\"}";
        let response = http_response("401 Unauthorized", "application/json", body);
        stream.write_all(&response).ok();
        return;
    }

    let path = extract_path(&buf).unwrap_or_default();
    let route = path.split('?').next().unwrap_or("/");

    let response = match route {
        "/json" => {
            let body = metrics_to_json(metrics, info);
            http_response("200 OK", "application/json", &body)
        }
        "/metrics" => {
            let body = metrics_to_prometheus(metrics, &info.chip, &info.hostname);
            http_response("200 OK", "text/plain; version=0.0.4; charset=utf-8", &body)
        }
        _ => {
            let ts = utc_now();
            let body = format!("{{\"error\":\"not found\",\"timestamp\":\"{ts}\"}}");
            http_response("404 Not Found", "application/json", &body)
        }
    };

    stream.write_all(&response).ok();
}
