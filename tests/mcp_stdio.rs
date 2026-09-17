#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

#[test]
#[allow(clippy::too_many_lines)]
fn mcp_binary_autostarts_daemon_and_serves_json_rpc() {
    let temp = TempDir::new().expect("temp");
    let database = temp.path().join("brain.sqlite");
    let socket = temp.path().join("engramd.sock");
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("engram-mcp"))
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
    assert_eq!(initialized["result"]["serverInfo"]["name"], "engram-mcp");

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
    let project = temp.path().join("ops-project");
    std::fs::create_dir(&project).expect("project");
    let mut call = |name: &str, arguments: Value| {
        write_request(
            &mut stdin,
            &json!({"jsonrpc":"2.0", "id":3, "method":"tools/call",
            "params":{"name":name,"arguments":arguments}}),
        );
        read_response(&mut stdout)["result"].clone()
    };
    let status = call("engram_project_status", json!({"path":project}));
    let project_id = status["structuredContent"]["project"]["id"].clone();
    assert!(project_id.is_string(), "{status}");
    let task = call(
        "engram_task_start",
        json!({"project_id":project_id,"prompt":"verify runtime traffic with sk_test_fakeMcp123"}),
    );
    let task_id = task["structuredContent"]["id"].clone();
    assert!(!task.to_string().contains("sk_test_fakeMcp123"));
    let evidence = call(
        "engram_record_evidence",
        json!({"project_id":project_id,"task_id":task_id,
        "kind":"runtime_observation","strength":"observational","command":"gcloud run services describe fixture",
        "exit_code":0,"artifact":"traffic fixture-old=100%; latestReady fixture-new"}),
    );
    let evidence_id = evidence["structuredContent"]["id"].clone();
    let lesson = json!({"project_id":project_id,"task_ids":[task_id],"evidence_ids":[evidence_id],
        "claim":"Cloud Run traffic remains pinned to a revision until explicitly shifted."});
    let rejected = call("engram_learn", lesson.clone());
    assert_eq!(rejected["isError"], true);
    assert!(rejected.to_string().contains("engram_mark_outcome"));
    call(
        "engram_mark_outcome",
        json!({"task_id":task_id,"outcome":"success"}),
    );
    let learned = call("engram_learn", lesson);
    assert_eq!(
        learned["structuredContent"]["memory"]["epistemic_state"],
        "candidate"
    );
    assert!(
        learned["structuredContent"]["next_step"]
            .as_str()
            .expect("guidance")
            .contains("Promote")
    );
    let catalog = call("engram_product", json!({"operation":"snapshot"}));
    assert!(catalog["structuredContent"]["repositories"].is_array());
    let schema = call("engram_product", json!({"operation":"schema"}));
    assert!(schema["structuredContent"]["oneOf"].is_array());
    let entity = call(
        "engram_product_edit",
        json!({"operation":"save","id":"fixture-product","expected_version":0,"item":{"type":"entity","value":{"kind":"product","name":"Fixture","project_id":project_id}}}),
    );
    assert_eq!(entity["structuredContent"]["version"], 1);
    let bundle = call(
        "engram_product",
        json!({"operation":"bundle","entity_id":"fixture-product","task":"inspect traffic","token_budget":256}),
    );
    assert!(
        engram::context::estimate_tokens(bundle["content"][0]["text"].as_str().unwrap()) <= 256
    );
    assert!(
        bundle.get("structuredContent").is_none(),
        "do not duplicate context or emit unbudgeted metadata to the agent"
    );
    let completion = call(
        "engram_complete_task",
        json!({"task_id":task_id,"outcome":"success","lessons":[{"claim":"Traffic allocation must be checked after a deployment","evidence_ids":[evidence_id]}]}),
    );
    assert_eq!(
        completion["structuredContent"]["reviews"][0]["status"],
        "pending"
    );
    drop(stdin);
    let status = child.wait().expect("MCP exits on EOF");
    assert!(status.success());

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let stopped = Command::new(assert_cmd::cargo::cargo_bin!("engram"))
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
