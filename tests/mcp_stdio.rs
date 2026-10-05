#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
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
    assert!(
        initialized["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("engram_complete_task")
    );

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
    let usage = json!({"sample_id":"fixture-response","project_id":project_id,"source":"test_harness","metrics":{"input_tokens":100,"tool_calls":2}});
    assert_eq!(
        call("engram_record_usage", usage.clone())["structuredContent"]["inserted"],
        true
    );
    assert_eq!(
        call("engram_record_usage", usage)["structuredContent"]["inserted"],
        false
    );
    let effectiveness = call("engram_effectiveness", json!({"project_ids":[project_id]}));
    assert_eq!(
        effectiveness["structuredContent"]["usage"]["totals"]["input_tokens"],
        100
    );
    assert!(effectiveness["structuredContent"]["usage"]["totals"]["cost_usd"].is_null());
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
    stop_daemon(temp.path(), &database, &socket);
}

/// Checkouts of one project (a repository and its linked worktree) share one
/// structural graph, which reflects whichever checkout was indexed last. With
/// a `path`, the graph tools must answer for the checkout that asks and say so.
#[test]
#[allow(clippy::too_many_lines)]
fn graph_tools_answer_for_the_asking_checkout() {
    let temp = TempDir::new().expect("temp");
    let database = temp.path().join("brain.sqlite");
    let socket = temp.path().join("engramd.sock");
    let main = temp.path().join("repo");
    let feature = temp.path().join("feature");
    std::fs::create_dir(&main).expect("repo");
    git(&main, &["init", "-q", "-b", "main"]);
    git(&main, &["config", "user.email", "fixture@example.invalid"]);
    git(&main, &["config", "user.name", "Fixture"]);
    std::fs::write(main.join("lib.py"), "def alpha():\n    return 1\n").expect("main lib");
    git(&main, &["add", "lib.py"]);
    git(&main, &["commit", "-q", "-m", "alpha"]);
    let main_head = git(&main, &["rev-parse", "HEAD"]);

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
    read_response(&mut stdout);
    let mut call = |name: &str, arguments: Value| {
        write_request(
            &mut stdin,
            &json!({"jsonrpc":"2.0", "id":2, "method":"tools/call",
            "params":{"name":name,"arguments":arguments}}),
        );
        read_response(&mut stdout)["result"].clone()
    };
    let labels = |result: &Value| -> Vec<String> {
        result["structuredContent"]["result"]
            .as_array()
            .unwrap_or_else(|| panic!("node list: {result}"))
            .iter()
            .filter_map(|node| node["label"].as_str().map(str::to_owned))
            .collect()
    };

    let project =
        call("engram_project_status", json!({"path": main}))["structuredContent"]["project"]["id"]
            .clone();
    assert!(project.is_string());

    // Indexing a linked worktree of the same project rewrites the shared graph.
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            feature.to_str().expect("feature path"),
        ],
    );
    std::fs::write(feature.join("lib.py"), "def beta():\n    return 2\n").expect("feature lib");
    git(&feature, &["commit", "-q", "-am", "beta"]);
    let feature_head = git(&feature, &["rev-parse", "HEAD"]);
    let shared = call("engram_project_status", json!({"path": feature}));
    assert_eq!(
        shared["structuredContent"]["project"]["id"], project,
        "a linked worktree shares the project identity"
    );
    assert!(
        !labels(&call(
            "engram_query_world",
            json!({"project_id": project, "query": "alpha"})
        ))
        .contains(&"alpha".to_owned()),
        "without a path the graph reflects the checkout indexed last"
    );

    // With a path, the read answers for that checkout and names it.
    let alpha = call(
        "engram_query_world",
        json!({"project_id": project, "query": "alpha", "path": main}),
    );
    assert!(labels(&alpha).contains(&"alpha".to_owned()), "{alpha}");
    let checkout = &alpha["structuredContent"]["checkout"];
    assert_eq!(checkout["head"], main_head.as_str(), "{alpha}");
    assert!(
        checkout["root"]
            .as_str()
            .is_some_and(|root| Path::new(root).ends_with("repo")),
        "{alpha}"
    );
    assert!(
        !labels(&call(
            "engram_query_world",
            json!({"project_id": project, "query": "beta", "path": main})
        ))
        .contains(&"beta".to_owned())
    );
    let node_id = alpha["structuredContent"]["result"]
        .as_array()
        .and_then(|nodes| nodes.iter().find(|node| node["label"] == "alpha"))
        .map(|node| node["id"].clone())
        .expect("alpha node");
    let impact = call(
        "engram_get_impact",
        json!({"project_id": project, "node_id": node_id, "path": main}),
    );
    assert_eq!(
        impact["structuredContent"]["checkout"]["head"],
        main_head.as_str(),
        "{impact}"
    );
    assert!(
        impact["structuredContent"]["nodes"]
            .as_array()
            .is_some_and(|nodes| nodes.iter().any(|node| node["label"] == "alpha")),
        "{impact}"
    );
    let beta = call(
        "engram_query_world",
        json!({"project_id": project, "query": "beta", "path": feature}),
    );
    assert!(labels(&beta).contains(&"beta".to_owned()), "{beta}");
    assert_eq!(
        beta["structuredContent"]["checkout"]["head"],
        feature_head.as_str()
    );

    // A path in another project is refused instead of read as this one.
    let other = temp.path().join("other");
    std::fs::create_dir(&other).expect("other");
    git(&other, &["init", "-q", "-b", "main"]);
    let refused = call(
        "engram_query_world",
        json!({"project_id": project, "query": "alpha", "path": other}),
    );
    assert_eq!(refused["isError"], true, "{refused}");
    assert!(
        refused["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("belongs to project")),
        "{refused}"
    );

    drop(stdin);
    assert!(child.wait().expect("MCP exits on EOF").success());
    stop_daemon(temp.path(), &database, &socket);
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn stop_daemon(home: &Path, database: &Path, socket: &Path) {
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
            .env("HOME", home)
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
