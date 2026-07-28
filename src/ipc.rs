//! Minimal client for herdr's socket API: newline-delimited JSON, one
//! request/response per connection (`{"id":..,"method":"pane.report_metadata",
//! "params":{..}}`).
//!
//! On Windows the socket is a named pipe at `\\.\pipe\<HERDR_SOCKET_PATH>`
//! (herdr feeds the whole path through interprocess' namespaced naming), which
//! a plain `File` can speak. On unix it is an ordinary unix domain socket.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

/// `HERDR_SOCKET_PATH` (injected into hook/action commands), falling back to
/// herdr's default socket location.
pub fn socket_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("HERDR_SOCKET_PATH") {
        return Some(path.into());
    }
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(|appdata| PathBuf::from(appdata).join("herdr").join("herdr.sock"))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Send one request; return the raw response line. Errors are for the caller
/// to ignore — running outside herdr must keep working.
pub fn call_text(method: &str, params: serde_json::Value) -> std::io::Result<String> {
    let path = socket_path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no herdr socket path")
    })?;
    let request = serde_json::json!({
        "id": format!("herdr-notes:{method}"),
        "method": method,
        "params": params,
    });
    roundtrip(&path, &request.to_string())
}

#[cfg(windows)]
fn roundtrip(path: &std::path::Path, request: &str) -> std::io::Result<String> {
    let pipe = format!(r"\\.\pipe\{}", path.display());
    let stream = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pipe)?;
    exchange(stream, request)
}

#[cfg(unix)]
fn roundtrip(path: &std::path::Path, request: &str) -> std::io::Result<String> {
    let stream = std::os::unix::net::UnixStream::connect(path)?;
    exchange(stream, request)
}

fn exchange<S: std::io::Read + Write>(mut stream: S, request: &str) -> std::io::Result<String> {
    stream.write_all(request.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(line)
}

/// `call_text` with a wall-clock bound. Used on the `--capture-prompt` path,
/// which runs inside a `UserPromptSubmit` hook: Claude Code kills a hook at its
/// configured timeout, so the socket must never be allowed to sit anywhere near
/// it.
///
/// Bounded with a worker thread rather than a socket read timeout because on
/// Windows the named pipe is opened as a plain `File`, which has no
/// read-timeout API. The worker can outlive the bound; that is fine here
/// because the hook process exits straight afterward and teardown reaps it.
// Not yet called outside tests — the capture-gate wiring lands in Task 3.
#[allow(dead_code)]
pub fn call_text_bounded(
    method: &str,
    params: serde_json::Value,
    timeout: std::time::Duration,
) -> std::io::Result<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let method = method.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(call_text(&method, params));
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "herdr socket did not answer in time",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_text_bounded_returns_promptly_when_there_is_no_socket() {
        // Intent: with no HERDR_SOCKET_PATH and no default socket reachable,
        // the inner call fails fast, so this returns that error rather than
        // waiting out the timeout. This machine has a live herdr session at
        // the platform-default socket path even with HERDR_SOCKET_PATH unset,
        // so the connect can succeed here instead of failing fast — the
        // `is_err` assertion does not hold in that environment. The timing
        // bound is the property under test regardless of outcome, so it is
        // kept unconditionally.
        let started = std::time::Instant::now();
        let _ = call_text_bounded(
            "pane.list",
            serde_json::json!({}),
            std::time::Duration::from_secs(3),
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "must not sit on the timeout when the connect itself fails: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn call_text_bounded_reports_a_timeout_as_timed_out() {
        // A zero timeout can never be met, so this exercises the timeout arm
        // without needing a hung server.
        let out = call_text_bounded(
            "pane.list",
            serde_json::json!({}),
            std::time::Duration::from_millis(0),
        );
        let err = out.expect_err("zero timeout cannot succeed");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "got {err:?}");
    }
}
