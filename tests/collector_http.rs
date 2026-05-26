//! End-to-end HTTP test for `power-monitor collect`.
//!
//! Spins up a minimal mock agent serving a canned `/json` payload, runs the
//! actual `power-monitor collect` binary pointing at it, and hits the
//! dashboard's `/snapshot` endpoint. This exercises the full poller + HTTP
//! server plumbing without touching IOReport or real hardware.

#![cfg(target_os = "macos")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Canned `/json` body a mock agent returns.
const MOCK_JSON: &str = r#"{"timestamp":"2026-04-09T12:00:00Z","version":"0.1.0","hostname":"mock01","chip":"Apple M5","pcpu_cores":4,"ecpu_cores":6,"gpu_cores":10,"interval_ms":1000,"sys_power":12.3,"cpu_power":3.1,"gpu_power":1.2,"ane_power":0.0,"dram_power":0.5,"all_power":4.3,"ecpu_util":0.10,"ecpu_freq_mhz":1200,"pcpu_util":0.15,"pcpu_freq_mhz":2500,"gpu_util":0.05,"gpu_freq_mhz":700,"cpu_temp_c":45.0,"gpu_temp_c":44.0,"memory_used_bytes":8000000000,"memory_total_bytes":16000000000,"swap_used_bytes":0,"swap_total_bytes":0}"#;

/// Bind an ephemeral port on localhost and return it.
fn ephemeral_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .unwrap()
        .port()
}

/// Spawn a background mock agent that answers every `GET /json` with the
/// canned payload. Returns the port it's listening on.
fn spawn_mock_agent() -> u16 {
    let port = ephemeral_port();
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind mock");
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let body = MOCK_JSON;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}

/// Fetch a single HTTP/1.1 response body from the given host:port + path.
fn http_get(host: &str, port: u16, path: &str) -> Result<String, String> {
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect_timeout(
        &addr.parse().map_err(|e| format!("parse: {e}"))?,
        Duration::from_secs(2),
    )
    .map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| format!("timeout: {e}"))?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut raw = Vec::with_capacity(4096);
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8(raw).map_err(|e| format!("utf8: {e}"))?;
    let (_, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "no body".to_string())?;
    Ok(body.to_string())
}

/// Wait until `/snapshot` reports `ok:true` for the mock host, or time out.
fn wait_for_ok(dashboard_port: u16, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if let Ok(body) = http_get("127.0.0.1", dashboard_port, "/snapshot")
            && body.contains("\"ok\":true")
        {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

struct CollectorGuard(Child);
impl Drop for CollectorGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Block until the collector prints its readiness banner.
///
/// `collect_cmd` emits "Fleet dashboard: http://..." on stderr the line
/// after `TcpListener::bind` returns, so observing that line is a
/// happens-before signal that the listener is up and accepting --
/// strictly stronger than a fixed sleep or a connect-poll loop.
///
/// The drain thread keeps reading until the child closes stderr (EOF on
/// child exit). If we dropped the reader once we'd seen the banner, the
/// pipe's read end would close while the child is still alive, and the
/// child's next eprintln would either get EPIPE or SIGPIPE depending on
/// the runtime's signal disposition.
fn wait_for_ready(stderr: ChildStderr) {
    let (tx, rx) = mpsc::channel::<()>();
    thread::spawn(move || {
        let mut signaled = false;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if !signaled && line.contains("Fleet dashboard:") {
                let _ = tx.send(());
                signaled = true;
            }
        }
    });
    rx.recv_timeout(Duration::from_secs(5))
        .expect("collector readiness banner not seen");
}

#[test]
fn collector_polls_mock_agent_and_serves_snapshot() {
    let agent_port = spawn_mock_agent();
    let dashboard_port = ephemeral_port();

    // Use the already-built test binary to run `collect`.
    let exe = env!("CARGO_BIN_EXE_power-monitor");
    let mut child = Command::new(exe)
        .args([
            "collect",
            "--host",
            &format!("127.0.0.1:{agent_port}"),
            "--port",
            &dashboard_port.to_string(),
            "--interval",
            "200",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn collector");
    let stderr = child.stderr.take().expect("stderr piped");
    let _guard = CollectorGuard(child);
    wait_for_ready(stderr);

    assert!(
        wait_for_ok(dashboard_port, Duration::from_secs(5)),
        "collector never reported ok:true for mock agent"
    );

    // Full snapshot check: must inline the agent JSON verbatim.
    let body = http_get("127.0.0.1", dashboard_port, "/snapshot").expect("snapshot");
    assert!(
        body.contains("\"chip\":\"Apple M5\""),
        "snapshot missing inlined chip field: {body}"
    );
    assert!(
        body.contains("\"sys_power\":12.3"),
        "snapshot missing inlined sys_power: {body}"
    );
    assert!(
        body.contains("\"ok\":true"),
        "snapshot missing ok:true: {body}"
    );

    // Dashboard HTML is served on /.
    let html = http_get("127.0.0.1", dashboard_port, "/").expect("html");
    assert!(html.contains("<title>power-monitor fleet</title>"));
    assert!(html.contains("EventSource"));
}

#[test]
fn collector_rejects_unauthorized_when_auth_required() {
    let agent_port = spawn_mock_agent();
    let dashboard_port = ephemeral_port();

    let exe = env!("CARGO_BIN_EXE_power-monitor");
    let mut child = Command::new(exe)
        .args([
            "collect",
            "--host",
            &format!("127.0.0.1:{agent_port}"),
            "--port",
            &dashboard_port.to_string(),
            "--interval",
            "200",
            "--auth",
            "secret123",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn collector");
    let stderr = child.stderr.take().expect("stderr piped");
    let _guard = CollectorGuard(child);
    wait_for_ready(stderr);

    let addr = format!("127.0.0.1:{dashboard_port}");
    let mut stream = TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_secs(2))
        .expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .write_all(b"GET /snapshot HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.contains("401 Unauthorized"),
        "expected 401, got: {text}"
    );
}
