//! A minimal HTTP server: enough to serve the UI and its JSON, no framework.
//!
//! A single-threaded accept loop is deliberate. The workload is a handful of
//! requests from one browser tab; a thread pool would add state without adding
//! throughput, and staying single-threaded keeps the whole server inspectable.
//! Requests are handled one at a time, so a slow scan delays the next request
//! rather than racing it.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;

use crate::session::load;

/// One parsed request. The body is only read for POST: the UI has exactly one
/// write operation (saving a config file), and reading bodies on GET would be
/// dead code with a size limit to get wrong.
pub struct Request {
    pub method: String,
    /// Path plus query, exactly as it arrived.
    pub target: String,
    pub body: Vec<u8>,
}

impl Request {
    /// Path without the query string.
    #[must_use]
    pub fn path(&self) -> &str {
        self.target.split_once('?').map_or(self.target.as_str(), |(path, _)| path)
    }

    /// Query without the path.
    #[must_use]
    pub fn query(&self) -> &str {
        self.target.split_once('?').map_or("", |(_, query)| query)
    }
}

/// Request handler: the parsed request, returning `(status, content type, body)`.
pub type Handler = Arc<dyn Fn(&Request) -> Response + Send + Sync>;

/// Refuse a body larger than this. The largest thing anyone saves here is
/// models.json at ~20 KB; a megabyte is already generous, and the limit keeps a
/// runaway client from making the single-threaded server allocate without bound.
const MAX_BODY: usize = 4 * 1024 * 1024;

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    #[must_use]
    pub fn json(body: String) -> Self {
        Self {
            status: 200,
            content_type: "application/json; charset=utf-8",
            body: body.into_bytes(),
        }
    }

    #[must_use]
    pub fn html(body: &str) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.as_bytes().to_vec(),
        }
    }

    #[must_use]
    pub fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.as_bytes().to_vec(),
        }
    }
}

/// Serve until the process is killed.
///
/// # Errors
///
/// Returns the bind error when the port is unavailable.
pub fn serve(address: SocketAddr, handler: Handler) -> std::io::Result<()> {
    let listener = TcpListener::bind(address)?;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let handler = Arc::clone(&handler);
                // One at a time: the UI issues a request, waits, then issues
                // the next. Concurrency here would only interleave file reads.
                handle(stream, &handler);
            }
            Err(error) => eprintln!("prime-agent-inspect: connection failed: {error}"),
        }
    }
    Ok(())
}

fn handle(mut stream: TcpStream, handler: &Handler) {
    let Ok(peek) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(peek);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    // Headers: drain them, keeping Content-Length so a POST body can be read.
    let mut content_length = 0_usize;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) if line.trim().is_empty() => break,
            Ok(_) => {
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        content_length = value.trim().parse().unwrap_or(0);
                    }
                }
            }
            Err(_) => break,
        }
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let mut body = Vec::new();
    if content_length > 0 && content_length <= MAX_BODY {
        use std::io::Read;
        body.resize(content_length, 0);
        if reader.read_exact(&mut body).is_err() {
            body.clear();
        }
    }
    let request = Request { method, target, body };
    let response = handler(&request);
    let _ = stream.write_all(
        format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            response.status,
            status_text(response.status),
            response.content_type,
            response.body.len()
        )
        .as_bytes(),
    );
    let _ = stream.write_all(&response.body);
    let _ = stream.flush();
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

/// Load a session file for a request, mapping failures to a 500 response.
pub fn load_for(path: &std::path::Path) -> Result<crate::session::Session, Response> {
    load(path).map_err(|error| {
        Response::text(
            500,
            &format!("{}: {}", error.path.display(), error.message),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_response_carries_its_content_type() {
        let response = Response::json("{}".to_string());
        assert_eq!(response.status, 200);
        assert!(response.content_type.starts_with("application/json"));
        assert_eq!(response.body, b"{}");
    }

    #[test]
    fn errors_carry_a_status_and_plain_body() {
        let response = Response::text(404, "missing");
        assert_eq!(response.status, 404);
        assert_eq!(status_text(404), "Not Found");
    }
}
