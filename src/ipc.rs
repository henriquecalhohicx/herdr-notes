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
///
/// A worker that cannot be CREATED degrades to an `Err` instead of panicking.
/// `std::thread::spawn` is `Builder::spawn(..).expect(..)`, so it panics when
/// the OS refuses the thread (thread/handle limit, memory pressure — a machine
/// already running several agent panes, a TUI and the herdr server is exactly
/// this code's environment). On the hook path a panic is exit 101 with a
/// message on stderr, and a non-zero exit from a `UserPromptSubmit` hook can
/// block the user's prompt from being sent — nothing here may ever cost them a
/// prompt. `Builder::spawn` returns that failure instead, and
/// `capture_from_env`'s `.ok()` maps it to `None`, which is the documented
/// socket-failure fallback: the additive gate simply falls back to the
/// note-file check.
pub fn call_text_bounded(
    method: &str,
    params: serde_json::Value,
    timeout: std::time::Duration,
) -> std::io::Result<String> {
    let method = method.to_string();
    bounded(timeout, move |tx| {
        std::thread::Builder::new()
            .name("herdr-notes-ipc".to_string())
            .spawn(move || {
                let _ = tx.send(call_text(&method, params));
            })
            .map(|_| ())
    })
}

/// The bound itself, with the worker's creation injected so the
/// spawn-failure-degrades-to-`Err` path is testable without forcing a real OS
/// thread-creation failure. `spawn` is handed the sender and reports whether
/// the worker started; if it did not, there is nothing to wait for and the
/// error is returned immediately rather than after `timeout`.
fn bounded<F>(timeout: std::time::Duration, spawn: F) -> std::io::Result<String>
where
    F: FnOnce(std::sync::mpsc::Sender<std::io::Result<String>>) -> std::io::Result<()>,
{
    let (tx, rx) = std::sync::mpsc::channel();
    spawn(tx)?;
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
        // When the socket cannot be reached, the inner call fails fast and this
        // returns THAT error rather than waiting out the timeout.
        //
        // HERDR_SOCKET_PATH is pointed at a path that can be neither a unix
        // socket nor a named pipe, under the shared ENV_LOCK. Leaving it unset
        // does not isolate anything: `socket_path()` falls back to the platform
        // default, so on a developer machine with a live herdr session this unit
        // test performed real socket I/O against whatever session happened to be
        // running, the connect SUCCEEDED, and the assertion below had to be
        // dropped for the test to pass — leaving a test whose name no longer
        // described what it did.
        let _guard = crate::state::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("HERDR_SOCKET_PATH");
        let dead = std::env::temp_dir()
            .join(format!("herdr-notes-no-such-socket-{}", std::process::id()));
        // SAFETY: serialized by ENV_LOCK; restored below.
        unsafe {
            std::env::set_var("HERDR_SOCKET_PATH", &dead);
        }

        let started = std::time::Instant::now();
        let out = call_text_bounded(
            "pane.list",
            serde_json::json!({}),
            std::time::Duration::from_secs(3),
        );
        let elapsed = started.elapsed();

        // Restore BEFORE asserting: a failing assert must not leak the dead
        // socket path into every test that acquires ENV_LOCK afterward.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("HERDR_SOCKET_PATH", v),
                None => std::env::remove_var("HERDR_SOCKET_PATH"),
            }
        }

        assert!(out.is_err(), "an unreachable socket path must fail the connect: {out:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "must not sit on the timeout when the connect itself fails: {elapsed:?}"
        );
    }

    #[test]
    fn a_worker_that_cannot_be_spawned_degrades_to_an_error() {
        // `std::thread::spawn` PANICS when the OS refuses the thread (it is
        // `Builder::spawn(..).expect(..)`), and on the `--capture-prompt` path a
        // panic is exit 101 plus stderr — the two things a `UserPromptSubmit`
        // hook must never do, because a non-zero exit can block the user's
        // prompt from being sent. The real OS failure (thread/handle exhaustion,
        // memory pressure) cannot be forced from a unit test without something
        // fragile and platform-specific, so the SEAM is tested instead: a
        // spawner that reports failure must come back as an `Err` — which
        // `capture_from_env`'s `.ok()` maps to `None`, the documented
        // socket-failure fallback — and must not wait out the bound either.
        let started = std::time::Instant::now();
        let out = bounded(std::time::Duration::from_secs(30), |_tx| {
            Err(std::io::Error::other("simulated thread-creation failure"))
        });
        let err = out.expect_err("a failed spawn must be an error, not a panic");
        assert_eq!(err.kind(), std::io::ErrorKind::Other, "the OS error is passed through: {err:?}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "must not sit on the bound when there is no worker to wait for: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn call_text_bounded_reports_a_timeout_as_timed_out() {
        // A zero timeout can never be met, so this exercises the timeout arm
        // without needing a hung server.
        //
        // Isolated for the same reason as the test above: this returns
        // immediately on the zero bound, but its worker thread keeps going and
        // calls `socket_path()` after we are gone. With the var unset that
        // resolves the platform default, so on a machine with a live herdr
        // session the detached worker performed a real `pane.list` against it
        // on every `cargo test`. Read-only, but a unit test should not be
        // talking to whatever session happens to be running.
        let _guard = crate::state::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("HERDR_SOCKET_PATH");
        let dead = std::env::temp_dir()
            .join(format!("herdr-notes-no-such-socket-timeout-{}", std::process::id()));
        // SAFETY: serialized by ENV_LOCK; restored below.
        unsafe {
            std::env::set_var("HERDR_SOCKET_PATH", &dead);
        }

        let out = call_text_bounded(
            "pane.list",
            serde_json::json!({}),
            std::time::Duration::from_millis(0),
        );

        // Restore BEFORE asserting, so a failing assert cannot leak the dead
        // path into every later test that takes ENV_LOCK.
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("HERDR_SOCKET_PATH", v),
                None => std::env::remove_var("HERDR_SOCKET_PATH"),
            }
        }

        let err = out.expect_err("zero timeout cannot succeed");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "got {err:?}");
    }
}
