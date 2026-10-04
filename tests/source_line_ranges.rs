//! Hook delivery of line-ranged source pointers: checked against the live
//! checkout, never stale, and never a reason by itself to resend a pack.
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

fn cli(root: &Path, args: &[&str], input: Option<Value>) -> String {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("engram"));
    command
        .env("HOME", root)
        .args([
            "--db",
            root.join("brain.sqlite").to_str().unwrap(),
            "--socket",
            root.join("engramd.sock").to_str().unwrap(),
        ])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("cli");
    if let Some(input) = input {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.to_string().as_bytes())
            .expect("payload");
    }
    let output = child.wait_with_output().expect("output");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf8")
}

#[test]
fn hook_packs_carry_checked_ranges_and_edits_alone_do_not_resend_them() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    let source = "pub fn compute_total() -> u32 {\n    1\n}\n";
    std::fs::write(repo.join("compute.rs"), source).unwrap();
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    let prompt = json!({"session_id":"ranges", "cwd":repo, "prompt":"Inspect compute_total"});
    let first = cli(root, &["hook", "prompt"], Some(prompt.clone()));
    assert!(
        first.contains("compute.rs:1-3 — Function compute_total"),
        "{first}"
    );

    // An edit the index has not seen removes the range; the same sources are
    // not delivered again merely because the rendering lost its range.
    std::fs::write(repo.join("compute.rs"), format!("// edited\n{source}")).unwrap();
    assert!(cli(root, &["hook", "prompt"], Some(prompt.clone())).is_empty());
    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
    let latest: String = connection
        .query_row(
            "SELECT dynamic_context FROM context_runs ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        latest.contains("compute.rs — Function compute_total") && !latest.contains("compute.rs:"),
        "{latest}"
    );

    // A changed definition is new content and arrives with its current range.
    std::fs::write(
        repo.join("compute.rs"),
        "// edited\n\npub fn compute_total() -> u32 {\n    2\n}\n",
    )
    .unwrap();
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    let refreshed = cli(root, &["hook", "prompt"], Some(prompt));
    assert!(
        refreshed.contains("compute.rs:3-5 — Function compute_total"),
        "{refreshed}"
    );
    cli(root, &["daemon", "stop"], None);
}
