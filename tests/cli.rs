use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn unrun(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_unrun")).args(args).current_dir(dir).output().unwrap()
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = unrun(dir, args);
    assert!(out.status.success(), "{args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A workspace with a few files, already initialised.
fn workspace() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("src/deep")).unwrap();
    fs::write(root.join("keep.txt"), "keep").unwrap();
    fs::write(root.join("edit.txt"), "original").unwrap();
    fs::write(root.join("src/a.rs"), "fn a() {}").unwrap();
    fs::write(root.join("src/deep/b.rs"), "fn b() {}").unwrap();
    ok(root, &["init"]);
    tmp
}

#[test]
fn undo_and_redo_restore_deletes_edits_and_creates() {
    let tmp = workspace();
    let root = tmp.path();

    ok(root, &["begin", "--command", "wreck things"]);
    fs::remove_dir_all(root.join("src")).unwrap();
    fs::write(root.join("edit.txt"), "clobbered").unwrap();
    fs::create_dir(root.join("new")).unwrap();
    fs::write(root.join("new/made.txt"), "made").unwrap();
    ok(root, &["end"]);

    ok(root, &["undo"]);
    assert_eq!(fs::read_to_string(root.join("src/a.rs")).unwrap(), "fn a() {}");
    assert_eq!(fs::read_to_string(root.join("src/deep/b.rs")).unwrap(), "fn b() {}");
    assert_eq!(fs::read_to_string(root.join("edit.txt")).unwrap(), "original");
    assert_eq!(fs::read_to_string(root.join("keep.txt")).unwrap(), "keep");
    assert!(!root.join("new").exists());

    ok(root, &["redo"]);
    assert!(!root.join("src").exists());
    assert_eq!(fs::read_to_string(root.join("edit.txt")).unwrap(), "clobbered");
    assert_eq!(fs::read_to_string(root.join("new/made.txt")).unwrap(), "made");
}

#[test]
fn undo_refuses_to_overwrite_later_changes() {
    let tmp = workspace();
    let root = tmp.path();

    ok(root, &["begin", "--command", "edit"]);
    fs::write(root.join("edit.txt"), "first edit").unwrap();
    ok(root, &["end"]);
    fs::write(root.join("edit.txt"), "later work").unwrap();

    let out = unrun(root, &["undo"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("edit.txt"));
    assert_eq!(fs::read_to_string(root.join("edit.txt")).unwrap(), "later work");

    ok(root, &["undo", "--force"]);
    assert_eq!(fs::read_to_string(root.join("edit.txt")).unwrap(), "original");
}

#[test]
fn run_records_a_shell_command_and_passes_its_exit_code() {
    let tmp = workspace();
    let root = tmp.path();
    let delete = if cfg!(windows) { "Remove-Item keep.txt; exit 3" } else { "rm keep.txt; exit 3" };

    let out = unrun(root, &["run", "-c", delete]);
    assert_eq!(out.status.code(), Some(3));
    assert!(!root.join("keep.txt").exists());

    let shown = ok(root, &["show"]);
    assert!(shown.contains("- keep.txt"), "{shown}");
    assert!(shown.contains("exit code 3"), "{shown}");

    ok(root, &["undo"]);
    assert_eq!(fs::read_to_string(root.join("keep.txt")).unwrap(), "keep");
}

#[test]
fn ignored_folders_and_untouched_commands_leave_no_entry() {
    let tmp = workspace();
    let root = tmp.path();

    ok(root, &["begin", "--command", "build"]);
    fs::create_dir(root.join("node_modules")).unwrap();
    fs::write(root.join("node_modules/pkg.js"), "x").unwrap();
    ok(root, &["end"]);

    assert!(ok(root, &["log"]).contains("no recorded commands"));
}

#[test]
fn hook_reads_command_and_cwd_from_stdin_json() {
    use std::io::Write;
    use std::process::Stdio;

    let tmp = workspace();
    let root = tmp.path();
    let hook = |phase: &str| {
        let json = serde_json::json!({
            "cwd": root.to_str().unwrap(),
            "tool_name": "Bash",
            "tool_input": { "command": "rm edit.txt" },
        });
        // Run from elsewhere to prove the workspace comes from the JSON, not the cwd.
        let mut child = Command::new(env!("CARGO_BIN_EXE_unrun"))
            .args(["hook", phase])
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(json.to_string().as_bytes()).unwrap();
        assert!(child.wait().unwrap().success());
    };

    hook("pre");
    fs::remove_file(root.join("edit.txt")).unwrap();
    hook("post");

    assert!(ok(root, &["log"]).contains("rm edit.txt"));
    ok(root, &["undo"]);
    assert_eq!(fs::read_to_string(root.join("edit.txt")).unwrap(), "original");
}
