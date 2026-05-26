//! Tiny no-deps HTTP helpers shared by `serve` and `collect`.
//!
//! We speak just enough HTTP/1.1 to serve JSON, Prometheus text, an embedded
//! HTML page, and Server-Sent Events. No framework, no async runtime, no
//! third-party dependencies.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Max bytes accepted in the request head before giving up. Anything larger
/// than this is either a malformed client or a probe.
const MAX_HEADER_BYTES: usize = 8192;

/// RAII permit for the connection-thread budget. Dropping decrements the
/// in-flight counter.
pub struct Permit {
    counter: Arc<AtomicUsize>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Bounded slot reservation. Returns `Some(Permit)` when a slot is free, or
/// `None` when all `max` slots are already busy.
pub fn try_acquire(counter: &Arc<AtomicUsize>, max: usize) -> Option<Permit> {
    let prev = counter.fetch_add(1, Ordering::Relaxed);
    if prev >= max {
        counter.fetch_sub(1, Ordering::Relaxed);
        return None;
    }
    Some(Permit {
        counter: Arc::clone(counter),
    })
}

/// Default cap for concurrent HTTP connection handlers per server.
pub const MAX_INFLIGHT: usize = 16;

/// Read from `stream` into a fresh buffer until `\r\n\r\n` is seen, EOF, or
/// [`MAX_HEADER_BYTES`] is reached. Returns the bytes read so far.
pub fn read_request_head(stream: &mut impl Read) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while buf.len() < MAX_HEADER_BYTES {
        let n = match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    buf
}

/// Build a full HTTP/1.1 response with `Connection: close` and permissive CORS.
pub fn http_response(status: &str, content_type: &str, body: &str) -> Vec<u8> {
    let len = body.len();
    format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         Access-Control-Allow-Origin: *\r\n\
         \r\n\
         {body}"
    )
    .into_bytes()
}

/// Extract the request path from a raw HTTP request buffer.
///
/// Parses the first line: `METHOD /path HTTP/1.1\r\n...`. Returns `None` on
/// malformed input. Path is copied into an owned `String`.
pub fn extract_path(buf: &[u8]) -> Option<String> {
    let line_end = buf.iter().position(|&b| b == b'\r').unwrap_or(buf.len());
    let line = std::str::from_utf8(&buf[..line_end]).ok()?;
    let mut parts = line.splitn(3, ' ');
    parts.next()?; // method
    let path = parts.next()?;
    Some(path.to_string())
}

/// Look for a bearer token in the `Authorization` header of a raw request.
///
/// Case-insensitive on the header name, trims whitespace around the token.
/// Returns `None` if the header is absent or malformed.
pub fn extract_bearer(buf: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(buf).ok()?;
    for line in text.split("\r\n") {
        if line.is_empty() {
            break; // end of headers
        }
        // Skip the request line and any malformed header without a colon.
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("authorization") {
            let value = value.trim();
            return value.strip_prefix("Bearer ").map(str::trim);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_path_basic() {
        assert_eq!(
            extract_path(b"GET /json HTTP/1.1\r\nHost: localhost\r\n\r\n").as_deref(),
            Some("/json")
        );
    }

    #[test]
    fn extract_path_root() {
        assert_eq!(
            extract_path(b"GET / HTTP/1.1\r\n\r\n").as_deref(),
            Some("/")
        );
    }

    #[test]
    fn extract_path_malformed_returns_none() {
        assert!(extract_path(b"nonsense").is_none());
    }

    #[test]
    fn extract_bearer_present() {
        let req = b"GET /json HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer abc123\r\n\r\n";
        assert_eq!(extract_bearer(req), Some("abc123"));
    }

    #[test]
    fn extract_bearer_case_insensitive_header() {
        let req = b"GET / HTTP/1.1\r\nauthorization: Bearer tok\r\n\r\n";
        assert_eq!(extract_bearer(req), Some("tok"));
    }

    #[test]
    fn extract_bearer_missing_returns_none() {
        let req = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n";
        assert!(extract_bearer(req).is_none());
    }

    #[test]
    fn extract_bearer_wrong_scheme_returns_none() {
        let req = b"GET / HTTP/1.1\r\nAuthorization: Basic ZGVtbw==\r\n\r\n";
        assert!(extract_bearer(req).is_none());
    }

    #[test]
    fn http_response_contains_headers_and_body() {
        let r = http_response("200 OK", "application/json", "{}");
        let s = std::str::from_utf8(&r).unwrap();
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(s.contains("Content-Type: application/json\r\n"));
        assert!(s.contains("Content-Length: 2\r\n"));
        assert!(s.ends_with("\r\n\r\n{}"));
    }
}
