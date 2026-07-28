//! The two hard rules of the `--capture-prompt` path, asserted against the
//! REAL binary as a property over hostile input.
//!
//! A `UserPromptSubmit` hook must **always exit 0** — a non-zero exit can block
//! the user's prompt from being sent — and must **never write to stdout**,
//! because Claude Code injects whatever it prints there into that prompt as
//! context. Both have been verified by hand at least three times during
//! development and by two whole-branch reviews; neither was ever encoded, so
//! nothing stopped a future edit from breaking them.
//!
//! This lives in `tests/` rather than a unit test because it is about PROCESS
//! behavior — exit status and stream contents — which a unit test cannot see,
//! and because `CARGO_BIN_EXE_*` is only set for integration targets.
//!
//! Every child gets its own env, so nothing here mutates the test process and
//! no lock is needed. `HERDR_PLUGIN_STATE_DIR` is redirected at a temp dir so a
//! capture can never touch a real note store, and `HERDR_SOCKET_PATH` is
//! pointed at a path that cannot exist so the gate's socket call cannot reach a
//! live herdr session.

use std::io::Write;
use std::process::{Command, Stdio};

/// A per-case temp dir plus an unreachable socket path, so the child can write
/// only where we say and can talk to nobody.
fn scratch(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let base = std::env::temp_dir().join(format!(
        "herdr-notes-hook-contract-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("temp store dir");
    let dead_socket = base.join("no-such-socket");
    (base, dead_socket)
}

/// Runs `--capture-prompt` with `stdin_bytes` on stdin and returns
/// `(exit code, stdout bytes, stderr bytes)`. `inside_herdr` decides whether
/// the child gets the `HERDR_*` vars that carry it past the early gates.
fn run_hook(tag: &str, stdin_bytes: &[u8], inside_herdr: bool) -> (Option<i32>, Vec<u8>, Vec<u8>) {
    let (store, dead_socket) = scratch(tag);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_herdr-notes"));
    cmd.arg("--capture-prompt")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Never the real store, never a live session.
        .env("HERDR_PLUGIN_STATE_DIR", &store)
        .env("HERDR_SOCKET_PATH", &dead_socket)
        .env_remove("HERDR_NOTES_NO_CAPTURE");
    if inside_herdr {
        cmd.env("HERDR_ENV", "1")
            .env("HERDR_TAB_ID", "w1:t1")
            .env("HERDR_PANE_ID", "w1:p2");
    } else {
        cmd.env_remove("HERDR_ENV").env_remove("HERDR_TAB_ID").env_remove("HERDR_PANE_ID");
    }

    let mut child = cmd.spawn().expect("spawn the capture binary");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(stdin_bytes)
        // A broken pipe here means the child exited before reading, which is
        // itself allowed — the contract is about its exit code and streams.
        .ok();
    let out = child.wait_with_output().expect("wait for the capture binary");
    let _ = std::fs::remove_dir_all(&store);
    (out.status.code(), out.stdout, out.stderr)
}

/// Hostile stdin, each named so a failure says which shape broke the contract.
fn hostile_inputs() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("valid", br#"{"prompt":"add a rate limiter"}"#.to_vec()),
        ("valid_multiline", b"{\"prompt\":\"first line\\nsecond line\"}".to_vec()),
        ("empty", Vec::new()),
        ("not_json", b"not json at all".to_vec()),
        ("json_wrong_shape", br#"{"prompt":42}"#.to_vec()),
        ("json_no_prompt", br#"{"session_id":"abc"}"#.to_vec()),
        ("blank_prompt", br#"{"prompt":"   "}"#.to_vec()),
        ("bom_prefixed", "\u{feff}{\"prompt\":\"after a bom\"}".as_bytes().to_vec()),
        // `read_to_string` FAILS on these bytes; the arm must absorb that.
        ("invalid_utf8", vec![0xff, 0xfe, 0x00, 0x80, 0xc0]),
        ("nul_bytes", b"{\"prompt\":\"a\0b\"}".to_vec()),
        ("lone_brace", b"{".to_vec()),
        ("whitespace_only", b"   \n\t  ".to_vec()),
        (
            "huge_single_line",
            format!(r#"{{"prompt":"{}"}}"#, "x".repeat(4 * 1024 * 1024)).into_bytes(),
        ),
        (
            "deeply_nested",
            format!("{}{}", "[".repeat(20_000), "]".repeat(20_000)).into_bytes(),
        ),
    ]
}

#[test]
fn capture_prompt_always_exits_zero_and_never_writes_stdout() {
    // Both env shapes: outside herdr the gate rejects early, inside it the
    // socket call and the store lookup both run. The contract binds every path.
    for inside_herdr in [false, true] {
        for (name, bytes) in hostile_inputs() {
            let tag = format!("{name}-{inside_herdr}");
            let (code, stdout, stderr) = run_hook(&tag, &bytes, inside_herdr);

            assert_eq!(
                code,
                Some(0),
                "{name} (inside_herdr={inside_herdr}): a UserPromptSubmit hook must exit 0 — \
                 a non-zero exit can block the user's prompt. stderr: {}",
                String::from_utf8_lossy(&stderr)
            );
            assert!(
                stdout.is_empty(),
                "{name} (inside_herdr={inside_herdr}): the hook must print NOTHING on stdout — \
                 Claude Code injects it into the prompt. got {} bytes: {:?}",
                stdout.len(),
                String::from_utf8_lossy(&stdout).chars().take(200).collect::<String>()
            );
        }
    }
}

#[test]
fn capture_prompt_exits_zero_when_stdin_is_closed_immediately() {
    // Not the same as empty input: the pipe is dropped without a write, so the
    // read itself may fail rather than return nothing.
    let (store, dead_socket) = scratch("closed-stdin");
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-notes"))
        .arg("--capture-prompt")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("HERDR_PLUGIN_STATE_DIR", &store)
        .env("HERDR_SOCKET_PATH", &dead_socket)
        .env("HERDR_ENV", "1")
        .env("HERDR_TAB_ID", "w1:t1")
        .env("HERDR_PANE_ID", "w1:p2")
        .output()
        .expect("run the capture binary");
    let _ = std::fs::remove_dir_all(&store);

    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty(), "got {:?}", String::from_utf8_lossy(&out.stdout));
}

#[test]
fn capture_prompt_writes_nothing_outside_the_store_dir_it_was_given() {
    // The gate needs either a live Notes pane or an existing note file. The
    // socket is unreachable and the store is empty, so a well-formed prompt
    // must be rejected and leave the dir untouched — proving the redirect in
    // these tests is load-bearing rather than decorative.
    let (store, dead_socket) = scratch("no-writes");
    let mut child = Command::new(env!("CARGO_BIN_EXE_herdr-notes"))
        .arg("--capture-prompt")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("HERDR_PLUGIN_STATE_DIR", &store)
        .env("HERDR_SOCKET_PATH", &dead_socket)
        .env("HERDR_ENV", "1")
        .env("HERDR_TAB_ID", "w1:t1")
        .env("HERDR_PANE_ID", "w1:p2")
        .spawn()
        .expect("spawn the capture binary");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(br#"{"prompt":"should not be captured"}"#)
        .ok();
    let out = child.wait_with_output().expect("wait for the capture binary");

    assert_eq!(out.status.code(), Some(0));
    let left: Vec<String> = std::fs::read_dir(&store)
        .expect("store dir still exists")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let _ = std::fs::remove_dir_all(&store);
    assert!(
        left.is_empty(),
        "no note file and no live pane means no capture, so nothing should be written: {left:?}"
    );
}
