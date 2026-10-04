//! Client-side workspace selection from repository configuration is read-only.
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

fn selected(home: &Path, cwd: &Path, args: &[&str], environment: Option<&str>) -> Value {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("engram"));
    command
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("ENGRAM_WORKSPACE")
        .env_remove("ENGRAM_DB_PATH")
        .env_remove("ENGRAM_SOCKET_PATH")
        .args(args)
        .args(["workspace", "show"]);
    if let Some(workspace) = environment {
        command.env("ENGRAM_WORKSPACE", workspace);
    }
    let output = command.output().expect("workspace command");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("workspace JSON")
}

#[test]
fn project_workspace_is_selected_from_nested_cwd_with_env_and_cli_precedence() {
    let temp = tempfile::TempDir::new().expect("temp");
    let root = temp.path().join("project");
    let nested = root.join("src/module");
    std::fs::create_dir_all(root.join(".engram")).unwrap();
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(root.join(".engram/config.toml"), "workspace = \"team\"\n").unwrap();
    let project = selected(temp.path(), &nested, &[], None);
    assert_eq!(project["workspace"], "team");
    assert!(
        project["database"]
            .as_str()
            .unwrap()
            .ends_with("/.engram/workspaces/team/brain.sqlite")
    );
    assert_eq!(
        selected(temp.path(), &nested, &[], Some("personal"))["workspace"],
        "personal"
    );
    assert_eq!(
        selected(
            temp.path(),
            &nested,
            &["--workspace", "default"],
            Some("personal")
        )["workspace"],
        "default"
    );
    assert!(
        !temp.path().join(".engram").exists(),
        "selection must not initialize state"
    );
    assert!(
        !root.join(".engram/project.toml").exists(),
        "selection must not enroll the repo"
    );
}

#[test]
fn mcp_and_proxy_read_project_workspace_before_starting_daemons() {
    let temp = tempfile::TempDir::new().expect("temp");
    let root = temp.path().join("project");
    let nested = root.join("src");
    std::fs::create_dir_all(root.join(".engram")).unwrap();
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        root.join(".engram/config.toml"),
        "workspace = \"../invalid\"\n",
    )
    .unwrap();
    for binary in [
        assert_cmd::cargo::cargo_bin!("engram-mcp"),
        assert_cmd::cargo::cargo_bin!("engram-proxy"),
    ] {
        let output = Command::new(binary)
            .current_dir(&nested)
            .env("HOME", temp.path())
            .env_remove("ENGRAM_WORKSPACE")
            .env_remove("ENGRAM_DB_PATH")
            .env_remove("ENGRAM_SOCKET_PATH")
            .stdin(Stdio::null())
            .output()
            .expect("client");
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("workspace must be"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(!temp.path().join(".engram").exists());
}
