#![allow(dead_code)]

//! Test support: a tiny blocking HTTP server that imitates the Redmine API.
//!
//! It is intentionally dependency-free and handles one connection per thread so
//! the server's concurrent metadata requests work as they do against Redmine.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::Value;

use redmine_mcp_rs::config::Config;
use redmine_mcp_rs::tools::App;

#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    pub fn json_body(&self) -> Value {
        serde_json::from_slice(&self.body).expect("request body is JSON")
    }

    pub fn query_value(&self, key: &str) -> Option<String> {
        self.query
            .split('&')
            .filter(|part| !part.is_empty())
            .find_map(|part| {
                let (name, value) = part.split_once('=').unwrap_or((part, ""));
                (name == key).then(|| percent_decode(value))
            })
    }

    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .get(&key.to_ascii_lowercase())
            .map(String::as_str)
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                match (hex_value(bytes[index + 1]), hex_value(bytes[index + 2])) {
                    (Some(high), Some(low)) => {
                        output.push(high * 16 + low);
                        index += 3;
                    }
                    _ => {
                        output.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct MockResponse {
    pub status: u16,
    pub content_type: String,
    pub body: String,
    pub headers: Vec<(String, String)>,
}

impl MockResponse {
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "application/json".to_string(),
            body: body.into(),
            headers: Vec::new(),
        }
    }

    pub fn empty(status: u16) -> Self {
        Self {
            status,
            content_type: "application/json".to_string(),
            body: String::new(),
            headers: Vec::new(),
        }
    }

    pub fn html(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "text/html".to_string(),
            body: body.into(),
            headers: Vec::new(),
        }
    }

    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((key.into(), value.into()));
        self
    }
}

type Router = dyn Fn(&RecordedRequest) -> MockResponse + Send + Sync;

pub struct MockRedmine {
    pub base_url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    shutdown: Arc<AtomicBool>,
    address: SocketAddr,
    handle: Option<JoinHandle<()>>,
}

impl MockRedmine {
    pub fn new<F>(router: F) -> Self
    where
        F: Fn(&RecordedRequest) -> MockResponse + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let address = listener.local_addr().expect("mock address");
        listener
            .set_nonblocking(true)
            .expect("non-blocking listener");

        let router: Arc<Router> = Arc::new(router);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_shutdown = Arc::clone(&shutdown);
        let thread_requests = Arc::clone(&requests);
        let thread_router = Arc::clone(&router);
        let handle = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let router = Arc::clone(&thread_router);
                        let requests = Arc::clone(&thread_requests);
                        thread::spawn(move || handle_connection(stream, &router, &requests));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            base_url: format!("http://{address}/"),
            requests,
            shutdown,
            address,
            handle: Some(handle),
        }
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("requests lock").clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().expect("requests lock").len()
    }

    pub fn last_request(&self) -> RecordedRequest {
        self.requests()
            .last()
            .cloned()
            .expect("at least one request was recorded")
    }
}

impl Drop for MockRedmine {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Wake the non-blocking accept loop.
        let _ = TcpStream::connect(self.address);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_connection(
    mut stream: TcpStream,
    router: &Arc<Router>,
    requests: &Arc<Mutex<Vec<RecordedRequest>>>,
) {
    // On macOS an accepted socket can inherit the listener's non-blocking
    // flag; force blocking mode so concurrent requests are read completely.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(_) => return,
    };
    let response = router(&request);
    requests.lock().expect("requests lock").push(request);

    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        reason_phrase(response.status),
        response.content_type,
        response.body.len()
    );
    for (key, value) in &response.headers {
        head.push_str(key);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");

    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(response.body.as_bytes());
    let _ = stream.flush();
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<RecordedRequest> {
    let mut data = Vec::new();
    let mut buffer = [0u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed before headers",
            ));
        }
        data.extend_from_slice(&buffer[..read]);
        if let Some(position) = find_subslice(&data, b"\r\n\r\n") {
            break position;
        }
        if data.len() > 1024 * 1024 {
            return Err(std::io::Error::other("headers too large"));
        }
    };

    let header_text = String::from_utf8_lossy(&data[..header_end]);
    let mut lines = header_text.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| std::io::Error::other("missing request line"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/").to_string();

    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target, String::new()),
    };

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let content_length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = data[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&buffer[..read]);
    }
    body.truncate(content_length);

    Ok(RecordedRequest {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

pub fn test_app(mock: &MockRedmine, read_only: bool) -> App {
    let extra: Vec<(&str, &str)> = if read_only {
        vec![("REDMINE_READ_ONLY", "true")]
    } else {
        Vec::new()
    };
    test_app_with_env(mock, &extra)
}

pub fn test_app_with_env(mock: &MockRedmine, extra: &[(&str, &str)]) -> App {
    let base_url = mock.base_url.clone();
    let extra: Vec<(String, String)> = extra
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();

    let config = Config::from_lookup(move |name| {
        if name == "REDMINE_URL" {
            return Some(base_url.clone());
        }
        if name == "REDMINE_API_KEY" {
            return Some("test-api-key".to_string());
        }
        extra
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    })
    .expect("valid test configuration");

    App::new(config).expect("test app")
}
