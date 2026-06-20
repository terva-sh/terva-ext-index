//! End-to-end wire smoke test: spawn the built binary, drive the handshake +
//! a tool_call over stdin, and assert the expected frames come back on stdout.
//!
//! This exercises the real protocol loop in src/main.rs (not just the pure
//! outline logic), proving the extension speaks the terva wire correctly.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn bin_path() -> std::path::PathBuf {
    // CARGO_BIN_EXE_<name> is set by cargo for integration tests.
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_terva-ext-index"))
}

#[test]
fn handshake_and_tool_call() {
    // Write a fixture file to outline.
    let dir = std::env::temp_dir().join(format!("terva-ext-index-wire-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let fixture = dir.join("sample.rs");
    std::fs::write(
        &fixture,
        "use std::fmt;\n\npub fn hello(name: &str) -> String {\n    format!(\"hi {name}\")\n}\n",
    )
    .unwrap();

    let mut child = Command::new(bin_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn binary");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    // 1. Read the five startup frames the extension emits unprompted.
    let hello = read_frame(&mut reader);
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["name"], "index");
    assert_eq!(hello["min_protocol"], 2);

    let reg_tool = read_frame(&mut reader);
    assert_eq!(reg_tool["type"], "register_tool");
    assert_eq!(reg_tool["name"], "index");
    assert_eq!(reg_tool["read_only"], true);
    assert_eq!(reg_tool["authority"], "local-read");

    let reg_ctx = read_frame(&mut reader);
    assert_eq!(reg_ctx["type"], "register_context");

    let subscribe = read_frame(&mut reader);
    assert_eq!(subscribe["type"], "subscribe");
    assert_eq!(subscribe["events"][0], "session_start");

    let ready = read_frame(&mut reader);
    assert_eq!(ready["type"], "ready");

    // 2. Send hello_ack with a cwd, then a tool_call.
    let cwd = dir.to_str().unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "type": "hello_ack",
            "protocol_version": 3,
            "cwd": cwd,
            "provider": "test",
            "model": "test"
        })
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "type": "tool_call",
            "id": "corr-1",
            "name": "index",
            "args": { "path": "sample.rs" }
        })
    )
    .unwrap();
    stdin.flush().unwrap();

    // 3. Read the tool_result.
    let result = read_frame(&mut reader);
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["id"], "corr-1");
    assert_eq!(result["is_error"], false);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("sample.rs"), "header missing: {text}");
    assert!(
        text.contains("imports: std::fmt"),
        "imports missing: {text}"
    );
    assert!(
        text.contains("pub fn hello(name: &str) -> String"),
        "signature missing: {text}"
    );
    assert!(text.contains("[3-5]"), "line range missing: {text}");

    // 4. Send an error case: unsupported extension.
    let other = dir.join("data.unknownext");
    std::fs::write(&other, "blah").unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "type": "tool_call",
            "id": "corr-2",
            "name": "index",
            "args": { "path": "data.unknownext" }
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    let err_result = read_frame(&mut reader);
    assert_eq!(err_result["id"], "corr-2");
    assert_eq!(err_result["is_error"], true);
    assert!(err_result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Fall back to `read`"));

    // 5. Sandbox: an absolute path outside the workspace is refused.
    let outside_dir =
        std::env::temp_dir().join(format!("terva-ext-index-wire-out-{}", std::process::id()));
    std::fs::create_dir_all(&outside_dir).unwrap();
    let outside = outside_dir.join("secret.rs");
    std::fs::write(&outside, "pub fn leak() {}\n").unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "type": "tool_call",
            "id": "corr-3",
            "name": "index",
            "args": { "path": outside.to_str().unwrap() }
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    let jail_result = read_frame(&mut reader);
    assert_eq!(jail_result["id"], "corr-3");
    assert_eq!(
        jail_result["is_error"], true,
        "out-of-workspace read must be refused"
    );
    assert!(jail_result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("outside the workspace"));
    let _ = std::fs::remove_dir_all(&outside_dir);

    // 6. session_start re-points cwd; a relative path now resolves there.
    let dir2 = dir.join("nested");
    std::fs::create_dir_all(&dir2).unwrap();
    std::fs::write(dir2.join("moved.rs"), "pub fn moved() {}\n").unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "type": "event",
            "event": "session_start",
            "cwd": dir2.to_str().unwrap()
        })
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "type": "tool_call",
            "id": "corr-4",
            "name": "index",
            "args": { "path": "moved.rs" }
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    let moved_result = read_frame(&mut reader);
    assert_eq!(moved_result["id"], "corr-4");
    assert_eq!(
        moved_result["is_error"], false,
        "cwd should follow session_start"
    );
    assert!(moved_result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("pub fn moved()"));

    // 7. Shutdown.
    writeln!(stdin, "{}", serde_json::json!({ "type": "shutdown" })).unwrap();
    stdin.flush().unwrap();
    let ack = read_frame(&mut reader);
    assert_eq!(ack["type"], "shutdown_ack");

    let status = child.wait().expect("wait");
    assert!(status.success(), "exit status: {status:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

fn read_frame<R: BufRead>(reader: &mut R) -> serde_json::Value {
    let mut line = String::new();
    let n = reader.read_line(&mut line).expect("read frame");
    assert!(n > 0, "unexpected EOF on wire");
    serde_json::from_str(line.trim_end()).unwrap_or_else(|e| panic!("bad frame {line:?}: {e}"))
}
