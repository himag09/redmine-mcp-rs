mod common;

use std::fs;
use std::io::{BufRead, BufReader, Write};

use base64::Engine as _;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use common::{MockRedmine, MockResponse};

const BIN: &str = env!("CARGO_BIN_EXE_redmine-mcp-rs");

struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    responses: Receiver<String>,
    stderr: Receiver<String>,
}

impl McpProcess {
    fn spawn(mock: &MockRedmine, extra_env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(BIN);
        command
            .env_clear()
            .env("REDMINE_URL", &mock.base_url)
            .env("REDMINE_API_KEY", "test-api-key")
            .env("REDMINE_LOG", "off")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in extra_env {
            command.env(key, value);
        }

        let mut child = command.spawn().expect("spawn redmine-mcp-rs");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        let stderr_pipe = child.stderr.take().expect("child stderr");

        let (response_tx, responses) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if response_tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let (stderr_tx, stderr_rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines() {
                match line {
                    Ok(line) => {
                        if stderr_tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            child,
            stdin,
            responses,
            stderr: stderr_rx,
        }
    }

    fn send(&mut self, message: Value) {
        self.send_raw(&message.to_string());
    }

    fn send_raw(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("write to server stdin");
        self.stdin.flush().expect("flush server stdin");
    }

    fn recv(&self) -> Value {
        self.recv_within(Duration::from_secs(10))
    }

    fn recv_within(&self, timeout: Duration) -> Value {
        let line = self
            .responses
            .recv_timeout(timeout)
            .expect("timed out waiting for an MCP response on stdout");
        serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("stdout contained non-JSON data: {line:?}: {error}"))
    }

    fn stderr_line(&self, timeout: Duration) -> Option<String> {
        self.stderr.recv_timeout(timeout).ok()
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn initialize(process: &mut McpProcess) -> Value {
    process.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "stdio-test", "version": "1.0.0" }
        }
    }));
    process.recv()
}

#[test]
fn initialize_and_list_tools() {
    let mock = MockRedmine::new(|_| MockResponse::empty(200));
    let mut process = McpProcess::spawn(&mock, &[]);

    let response = initialize(&mut process);
    assert_eq!(response["id"], json!(1));
    assert_eq!(response["result"]["protocolVersion"], json!("2025-06-18"));
    assert!(
        response["result"]["capabilities"]["tools"].is_object(),
        "tools capability should be advertised"
    );
    assert_eq!(
        response["result"]["serverInfo"]["name"],
        json!("redmine-mcp-rs")
    );
    assert!(
        response["result"]["instructions"]
            .as_str()
            .expect("instructions")
            .contains("untrusted data")
    );

    process.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    process.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    }));

    let list = process.recv();
    assert_eq!(list["id"], json!(2));
    let tools = list["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 13);

    let read_issue = tools
        .iter()
        .find(|tool| tool["name"] == json!("read_issue"))
        .expect("read_issue tool");
    assert!(
        read_issue["description"]
            .as_str()
            .expect("description")
            .contains("journals")
    );
    assert_eq!(read_issue["annotations"]["readOnlyHint"], json!(true));
    assert_eq!(read_issue["inputSchema"]["required"][0], json!("issue_id"));
}

#[test]
fn tools_call_returns_json_text_content() {
    let mock =
        MockRedmine::new(|_| MockResponse::json(200, r#"{"issue":{"id":42,"subject":"Hello"}}"#));
    let mut process = McpProcess::spawn(&mock, &[]);
    initialize(&mut process);

    process.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "read_issue", "arguments": { "issue_id": 42 } }
    }));

    let response = process.recv();
    assert_eq!(response["id"], json!(2));
    assert_eq!(response["result"]["isError"], json!(false));

    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    let issue: Value = serde_json::from_str(text).expect("tool text is JSON");
    assert_eq!(issue["id"], json!(42));
    assert_eq!(issue["subject"], json!("Hello"));
}

#[test]
fn read_only_mode_hides_and_rejects_write_tools() {
    let mock = MockRedmine::new(|_| MockResponse::empty(200));
    let mut process = McpProcess::spawn(&mock, &[("REDMINE_READ_ONLY", "true")]);
    initialize(&mut process);

    process.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    }));
    let list = process.recv();
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 8);

    process.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": { "name": "create_issue", "arguments": { "subject": "nope" } }
    }));
    let response = process.recv();
    assert_eq!(response["result"]["isError"], json!(true));
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("REDMINE_READ_ONLY")
    );
}

#[test]
fn malformed_input_does_not_kill_the_server() {
    let mock = MockRedmine::new(|_| MockResponse::empty(200));
    let mut process = McpProcess::spawn(&mock, &[]);
    initialize(&mut process);

    process.send_raw("this is not json");
    process.send(json!({ "jsonrpc": "2.0", "id": 9, "method": "ping" }));

    let pong = loop {
        let message = process.recv_within(Duration::from_secs(10));
        if message["id"] == json!(9) {
            break message;
        }
        // The official SDK may surface a parse error before the next reply.
        assert_eq!(
            message["error"]["code"],
            json!(-32700),
            "unexpected response: {message}"
        );
    };

    assert_eq!(pong["result"], json!({}));
}

#[test]
fn batch_requests_are_rejected_without_stopping_the_server() {
    let mock = MockRedmine::new(|_| MockResponse::empty(200));
    let mut process = McpProcess::spawn(&mock, &[]);
    initialize(&mut process);

    process.send(json!([{ "jsonrpc": "2.0", "id": 1, "method": "ping" }]));
    let error = process.recv();
    assert_eq!(error["error"]["code"], json!(-32600));

    process.send(json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }));
    assert_eq!(process.recv()["result"], json!({}));
}

#[test]
fn unknown_methods_and_tools_are_protocol_errors() {
    let mock = MockRedmine::new(|_| MockResponse::empty(200));
    let mut process = McpProcess::spawn(&mock, &[]);
    initialize(&mut process);

    process.send(json!({ "jsonrpc": "2.0", "id": 1, "method": "does/not/exist" }));
    let error = process.recv();
    assert_eq!(error["error"]["code"], json!(-32601));

    process.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "does_not_exist", "arguments": {} }
    }));
    let error = process.recv();
    assert_eq!(error["error"]["code"], json!(-32601));
}

#[test]
fn diagnostics_go_to_stderr_not_stdout() {
    let mock = MockRedmine::new(|_| MockResponse::empty(200));
    let mut process = McpProcess::spawn(&mock, &[("REDMINE_LOG", "debug")]);

    let response = initialize(&mut process);
    assert_eq!(response["id"], json!(1));

    let mut found = false;
    for _ in 0..10 {
        match process.stderr_line(Duration::from_millis(500)) {
            Some(line) if line.contains("initialize") || line.contains("starting") => {
                found = true;
                break;
            }
            Some(_) => continue,
            None => break,
        }
    }
    assert!(found, "expected debug diagnostics on stderr");
}

#[test]
fn check_command_verifies_connectivity_without_leaking_the_api_key() {
    let mock = MockRedmine::new(|_| {
        MockResponse::json(
            200,
            r#"{"user":{"id":35,"login":"tester","api_key":"secret-key"}}"#,
        )
    });

    let output = Command::new(BIN)
        .env_clear()
        .env("REDMINE_URL", &mock.base_url)
        .env("REDMINE_API_KEY", "test-api-key")
        .arg("--check")
        .output()
        .expect("run --check");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("tester"), "stdout: {stdout}");
    assert!(stdout.contains("id 35"), "stdout: {stdout}");
    assert!(!stdout.contains("secret-key"), "stdout leaked a secret");

    assert_eq!(mock.request_count(), 1);
    assert_eq!(mock.last_request().path, "/users/current.json");
}

#[test]
fn version_and_help_exit_zero() {
    for argument in ["--version", "--help"] {
        let output = Command::new(BIN)
            .env_clear()
            .arg(argument)
            .output()
            .expect("run command");
        assert!(
            output.status.success(),
            "{argument} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("redmine-mcp-rs"), "{argument}: {stdout}");
    }
}

#[test]
fn check_command_fails_when_authentication_fails() {
    let mock = MockRedmine::new(|_| MockResponse::json(401, r#"{"errors":["Unauthorized"]}"#));

    let output = Command::new(BIN)
        .env_clear()
        .env("REDMINE_URL", &mock.base_url)
        .env("REDMINE_API_KEY", "bad-key")
        .arg("--check")
        .output()
        .expect("run --check");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("401"), "stderr: {stderr}");
    assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);
    assert_eq!(mock.request_count(), 1);
}

#[test]
fn tools_call_accepts_null_arguments() {
    let mock = MockRedmine::new(|_| MockResponse::empty(200));
    let mut process = McpProcess::spawn(&mock, &[]);
    initialize(&mut process);

    process.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "read_issue", "arguments": null }
    }));

    let response = process.recv();
    assert_eq!(response["result"]["isError"], json!(true));
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("issue_id")
    );
    assert_eq!(mock.request_count(), 0);
}

#[test]
fn process_environment_wins_over_dotenv() {
    let mock =
        MockRedmine::new(|_| MockResponse::json(200, r#"{"user":{"id":1,"login":"tester"}}"#));

    let dir = std::env::temp_dir().join(format!(
        "redmine-mcp-rs-dotenv-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    fs::write(
        dir.join(".env"),
        "REDMINE_URL=https://wrong.invalid/\nREDMINE_API_KEY=key-from-dotenv\n",
    )
    .expect("write .env");

    let output = Command::new(BIN)
        .env_clear()
        .current_dir(&dir)
        .env("REDMINE_URL", &mock.base_url)
        .env("REDMINE_LOG", "off")
        .arg("--check")
        .output()
        .expect("run --check");

    let _ = fs::remove_dir_all(&dir);

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("tester"), "stdout: {stdout}");
    assert_eq!(mock.request_count(), 1);
    assert_eq!(
        mock.last_request().header("x-redmine-api-key"),
        Some("key-from-dotenv"),
        "the API key should come from .env when the process env omits it"
    );
}

#[test]
fn accepts_attachment_messages_above_the_sdk_default_line_limit() {
    // rmcp's stdio transport defaults to 16 MiB per line. A 13 MiB attachment
    // becomes ~17.3 MiB of base64 plus JSON framing, so this test would time
    // out if the server did not raise the line limit for the 25 MiB cap.
    let mock = MockRedmine::new(
        |request| match (request.method.as_str(), request.path.as_str()) {
            ("POST", "/uploads.json") => {
                MockResponse::json(201, r#"{"upload":{"id":1,"token":"tok"}}"#)
            }
            ("PUT", "/issues/1.json") => MockResponse::empty(204),
            ("GET", "/issues/1.json") => MockResponse::json(200, r#"{"issue":{"id":1}}"#),
            _ => MockResponse::json(404, "{}"),
        },
    );
    let mut process = McpProcess::spawn(&mock, &[]);
    initialize(&mut process);

    let data = vec![b'a'; 13 * 1024 * 1024];
    let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
    process.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "add_attachment",
            "arguments": {
                "issue_id": 1,
                "filename": "big.bin",
                "data_base64": encoded
            }
        }
    }));

    let response = process.recv_within(Duration::from_secs(30));
    assert_eq!(response["id"], json!(2));
    assert_eq!(response["result"]["isError"], json!(false));
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    let issue: Value = serde_json::from_str(text).expect("tool text is JSON");
    assert_eq!(issue["id"], json!(1));

    let requests = mock.requests();
    assert_eq!(requests[0].path, "/uploads.json");
    assert_eq!(requests[0].body.len(), data.len());
}
