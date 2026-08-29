#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

#[test]
fn mcp_binary_autostarts_daemon_and_serves_json_rpc() {
    let temp = TempDir::new().expect("temp");
    let database = temp.path().join("brain.sqlite");
    let socket = temp.path().join("lebd.sock");
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("leb-mcp"))
        .args([
            "--db",
            database.to_str().expect("db path"),
            "--socket",
            socket.to_str().expect("socket path"),
        ])
        .env("HOME", temp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn MCP");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    write_request(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}
        }),
    );
    let initialized = read_response(&mut stdout);
    assert_eq!(initialized["id"], 1);
    assert_eq!(initialized["result"]["serverInfo"]["name"], "leb-mcp");

    write_request(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    );
    let tools = read_response(&mut stdout);
    assert_eq!(tools["id"], 2);
    assert!(
        tools["result"]["tools"]
            .as_array()
            .is_some_and(|items| items.len() >= 6)
    );
    drop(stdin);
    let status = child.wait().expect("MCP exits on EOF");
    assert!(status.success());

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let stopped = Command::new(assert_cmd::cargo::cargo_bin!("leb"))
            .args([
                "--db",
                database.to_str().expect("db path"),
                "--socket",
                socket.to_str().expect("socket path"),
                "daemon",
                "stop",
            ])
            .env("HOME", temp.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("stop daemon");
        if stopped.success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "auto-started daemon was unavailable"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn write_request(stdin: &mut impl Write, value: &Value) {
    serde_json::to_writer(&mut *stdin, value).expect("encode request");
    stdin.write_all(b"\n").expect("request newline");
    stdin.flush().expect("flush request");
}

fn read_response(stdout: &mut impl BufRead) -> Value {
    let mut line = String::new();
    stdout.read_line(&mut line).expect("read response");
    assert!(!line.is_empty(), "MCP exited without a response");
    serde_json::from_str(&line).expect("JSON-RPC response")
}
