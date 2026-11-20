//! HTTP access, behind a trait so tests and the mock never touch the network.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::CancelToken;
use crate::error::{Error, ErrorKind, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    /// Body bytes (capped by the client).
    pub body: Vec<u8>,
}

/// Blocking HTTP GET.
pub trait HttpClient: Send + Sync {
    /// GETs `url` with extra headers. Non-2xx statuses are returned, not errors.
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse>;

    /// Downloads `url` to `dest`, returning the bytes written. `progress` is called with
    /// `(written, total)` as the body arrives. Non-2xx statuses are [`ErrorKind::Http`].
    ///
    /// The default reads the whole body through [`Self::get`], so it is capped by that
    /// client's read limit (64 MiB for the real one) and suits tests and fakes.
    fn download(
        &self,
        url: &str,
        dest: &Path,
        cancel: &CancelToken,
        progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
    ) -> Result<u64> {
        let response = self.get(url, &[])?;
        if !(200..=299).contains(&response.status) {
            return Err(Error::new(ErrorKind::Http)
                .with("status", response.status)
                .with("url", url));
        }
        cancel.check()?;
        let bytes = response.body.len() as u64;
        std::fs::write(dest, &response.body)?;
        progress(bytes, Some(bytes));
        Ok(bytes)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NoHttp;

impl HttpClient for NoHttp {
    fn get(&self, url: &str, _: &[(&str, &str)]) -> Result<HttpResponse> {
        Err(Error::new(ErrorKind::HttpUnavailable).with("url", url))
    }
}

#[cfg(feature = "http")]
#[derive(Debug, Clone)]
pub struct UreqClient {
    agent: ureq::Agent,
}

#[cfg(feature = "http")]
impl Default for UreqClient {
    fn default() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            .user_agent(concat!("colab-playground/", env!("CARGO_PKG_VERSION")))
            .build();
        UreqClient {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

#[cfg(feature = "http")]
impl HttpClient for UreqClient {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse> {
        let mut req = self.agent.get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let mut resp = req.call().map_err(|e| {
            Error::new(ErrorKind::Http)
                .with("url", url)
                .detail(e.to_string())
        })?;
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_vec()
            .map_err(|e| {
                Error::new(ErrorKind::Http)
                    .with("url", url)
                    .detail(e.to_string())
            })?;
        Ok(HttpResponse { status, body })
    }

    fn download(
        &self,
        url: &str,
        dest: &Path,
        cancel: &CancelToken,
        progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
    ) -> Result<u64> {
        // A streaming agent without the global timeout a big download would trip; a connect
        // timeout still bounds the handshake.
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            .user_agent(concat!("colab-playground/", env!("CARGO_PKG_VERSION")))
            .build();
        let agent = ureq::Agent::new_with_config(config);
        let mut resp = agent.get(url).call().map_err(|e| {
            Error::new(ErrorKind::Http)
                .with("url", url)
                .detail(e.to_string())
        })?;
        let status = resp.status().as_u16();
        if !(200..=299).contains(&status) {
            return Err(Error::new(ErrorKind::Http)
                .with("status", status)
                .with("url", url));
        }
        let total = resp.body().content_length();
        let mut reader = resp.body_mut().as_reader();
        let mut file = std::fs::File::create(dest)?;
        let mut buf = [0u8; 64 * 1024];
        let mut written: u64 = 0;
        loop {
            cancel.check()?;
            let n = reader.read(&mut buf).map_err(|e| {
                Error::new(ErrorKind::Http)
                    .with("url", url)
                    .detail(e.to_string())
            })?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
            written += n as u64;
            progress(written, total);
        }
        Ok(written)
    }
}

pub fn default_client() -> Arc<dyn HttpClient> {
    #[cfg(feature = "http")]
    {
        Arc::new(UreqClient::default())
    }
    #[cfg(not(feature = "http"))]
    {
        Arc::new(NoHttp)
    }
}

/// GETs `http://127.0.0.1:<port><path>` and returns the status code.
pub fn local_http_status(port: u16, path: &str, timeout: Duration) -> Result<u16> {
    if !path.starts_with('/') || path.chars().any(|c| c.is_control() || c == ' ') {
        return Err(Error::invalid(
            "path",
            "health path must start with '/' and contain no spaces",
        ));
    }
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nUser-Agent: colab-playground\r\n\r\n"
    );
    stream.write_all(request.as_bytes())?;
    let mut buf = [0u8; 256];
    let mut got = Vec::new();
    while !got.windows(2).any(|w| w == b"\r\n") && got.len() < 4096 {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
    parse_status_line(&got)
}

pub fn local_port_open(port: u16, timeout: Duration) -> bool {
    TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), timeout).is_ok()
}

fn parse_status_line(bytes: &[u8]) -> Result<u16> {
    let line = String::from_utf8_lossy(bytes);
    let first = line.lines().next().unwrap_or_default();
    let mut parts = first.split_whitespace();
    match (
        parts.next(),
        parts.next().and_then(|c| c.parse::<u16>().ok()),
    ) {
        (Some(v), Some(code)) if v.starts_with("HTTP/") => Ok(code),
        _ => Err(Error::new(ErrorKind::Parse).detail(format!("bad HTTP status line: {first:?}"))),
    }
}

/// Percent-encodes a query component (RFC 3986 unreserved characters kept).
pub fn encode_query(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn status_line_parsing() {
        assert_eq!(parse_status_line(b"HTTP/1.1 200 OK\r\n").unwrap(), 200);
        assert_eq!(parse_status_line(b"HTTP/1.0 503 Busy\r\n").unwrap(), 503);
        assert!(parse_status_line(b"garbage").is_err());
    }

    #[test]
    fn query_encoding() {
        assert_eq!(encode_query("qwen 3/gguf&x"), "qwen%203%2Fgguf%26x");
        assert_eq!(encode_query("a-b_c.d~"), "a-b_c.d~");
    }

    #[test]
    fn local_probe_against_a_tiny_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            // First connection: the port check (connects and closes).
            drop(listener.accept().unwrap());
            // Second connection: the health probe.
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let n = s.read(&mut buf).unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).starts_with("GET /health HTTP/1.1\r\n"));
            s.write_all(b"HTTP/1.1 204 No Content\r\n\r\n").unwrap();
        });
        assert!(local_port_open(port, Duration::from_secs(2)));
        assert_eq!(
            local_http_status(port, "/health", Duration::from_secs(5)).unwrap(),
            204
        );
        server.join().unwrap();
        assert!(local_http_status(port, "no-slash", Duration::from_secs(1)).is_err());
        assert!(
            NoHttp
                .get("https://x", &[])
                .unwrap_err()
                .is(ErrorKind::HttpUnavailable)
        );
    }
}
