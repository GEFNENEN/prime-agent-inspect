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

/// Request handler: path plus query, returning `(status, content type, body)`.
pub type Handler = Arc<dyn Fn(&str) -> Response + Send + Sync>;

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
    // Drain the headers so the client's write completes.
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) if line.trim().is_empty() => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }

    let target = request_line.split_whitespace().nth(1).unwrap_or("/");
    let response = handler(target);
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
