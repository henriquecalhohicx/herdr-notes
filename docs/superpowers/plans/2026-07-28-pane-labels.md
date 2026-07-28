# Pane Labels and a Live Capture Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Use the name you gave a herdr pane for prompt-group headings and note titles, and start capturing prompts the moment a Notes pane is open rather than the moment you type into the note.

**Architecture:** `PaneInfo` gains herdr's `label`, and `PaneInfo::nice_title` — already the one place the heading/title pairing is spelled — prefers it over the machine-set terminal title. `autotitle_wanted` drops its empty-title condition so an auto title tracks its source every heartbeat, which forces `git_tried` to cache the branch rather than merely record the attempt. The capture hook gains a bounded `pane.list` call whose answer is an ADDITIONAL way to pass the gate, so a socket failure degrades to today's note-file check.

**Tech Stack:** Rust 2024, ratatui + crossterm, `serde_json`, `std::thread` for the bounded socket call. No new dependencies.

## Global Constraints

- Phase D only. Codex capture, overlay-row grouping, pruning orphaned pane files, and the `meaningful_title` slash rule are all **out of scope** — do not build or change them.
- `cargo build --release`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` must all pass. `cargo build --release` fails with os error 5 while a `herdr-notes` TUI holds the binary — close the pane first.
- **The capture path must always exit 0.** A non-zero exit from a `UserPromptSubmit` hook can block the user's prompt from being sent.
- **The capture path must never write to stdout.** Whatever the hook prints there is injected into the prompt as context.
- Socket calls are best-effort: any call, parse, or field failure must degrade, never panic. The pane must work offline.
- Esc must NEVER exit the TUI. Only `q` quits.
- `render_markdown(text, width) -> Vec<Line>` must keep its exact signature and behavior.
- Wrap and cursor math budget by display columns (`unicode-width`), never char count.
- Ring size stays 3 per pane.
- Socket-dependent logic is tested against injected data, never a live socket.

## Spec Correction (read before starting)

**The spec contradicts itself on what a live socket answering "no Notes pane" should do**, and this plan resolves it one way. `docs/superpowers/specs/2026-07-28-pane-labels-design.md` says in Features §3 that `Some(false)` should **reject**, and then in Failure Modes says a stale token should **fall back to the note-file check rather than rejecting outright**. A stale token produces exactly `Some(false)`, so both cannot hold.

This plan implements the **fall-back** reading, making the socket answer purely additive:

```
allowed = notes_pane_live == Some(true) || note_file_exists
```

Reasons: it is monotonic — the gate can only ever start capturing earlier, never stop capturing something that works today; it honors the spec's own stated principle that a broken socket "degrades to current behavior, never to silent no-capture"; and the reject reading would silently stop capture for a tab with a real note the moment its Notes pane closed, which nobody asked for. The user should be told this reading was chosen.

---

## File Structure

- **Modify `src/launch.rs`** — add the pure "is a Notes pane in this tab alive right now" predicate. It already owns pane-list deserialization and the staleness rule; a second parser elsewhere is exactly the drift `CLAUDE.md` warns about.
- **Modify `src/ipc.rs`** — a bounded variant of `call_text`, so the hook can never sit on a hung socket.
- **Modify `src/prompts.rs`** — the capture gate consumes an injected answer so every gate test stays pure; only `capture_from_env` touches the socket.
- **Modify `src/app.rs`** — `PaneInfo.label`, `nice_title`'s preference, `autotitle_wanted`, `maybe_autotitle`, `git_tried`.
- **Modify `CLAUDE.md`** and **`docs/superpowers/specs/2026-07-27-agent-grouping-design.md`** — correct the two false claims and add the `token_stale` gotcha.

---

### Task 1: The pure "live Notes pane in this tab" predicate

`src/launch.rs` already deserializes pane lists and owns `HEARTBEAT_STALE_SECS` and `token_stale`. The predicate belongs there so the crate keeps one pane-list parser.

**The trap:** `Pane::is_notes()` accepts the token **or** the `"Notes"` label, and `token_stale` returns `false` for a **missing** token. Both are deliberate for the launcher, which must recognize a pane the launcher just labeled before its TUI has reported anything. Both are wrong here: the capture gate needs proof the pane is alive *now*, and a label outlives a dead pane. So this predicate requires the token **present AND fresh**, and must not call `is_notes()`.

**Files:**
- Modify: `src/launch.rs` (add after `token_stale`, around `:112`)
- Test: `src/launch.rs` (the existing `mod tests`)

**Interfaces:**
- Consumes: `PaneListMsg`, `Pane`, `token_stale`, `HEARTBEAT_STALE_SECS`, `METADATA_SOURCE`, `strip_bom` — all already in `launch.rs`.
- Produces: `pub fn notes_pane_fresh(pane_list_json: &str, tab_id: &str, now: u64) -> Option<bool>` — `Some(true)` when a pane in `tab_id` carries a `herdr-notes` token that is present and not stale, `Some(false)` when the list parsed and no such pane exists, `None` when the JSON could not be parsed.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/launch.rs`. Note the existing `pane_list(panes)` helper wraps pane JSON in the `{"result":{"panes":[...]}}` envelope — read it and use it rather than hand-writing envelopes:

```rust
    #[test]
    fn notes_pane_fresh_requires_a_present_and_fresh_token() {
        let json = pane_list(
            r#"{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{"herdr-notes":"95"}}"#,
        );
        assert_eq!(notes_pane_fresh(&json, "w1:t1", 100), Some(true), "5s old is fresh");
        assert_eq!(notes_pane_fresh(&json, "w1:t9", 100), Some(false), "other tab");
    }

    #[test]
    fn notes_pane_fresh_rejects_a_stale_token() {
        // 60s old against a 20s threshold.
        let json = pane_list(
            r#"{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{"herdr-notes":"40"}}"#,
        );
        assert_eq!(notes_pane_fresh(&json, "w1:t1", 100), Some(false));
    }

    #[test]
    fn notes_pane_fresh_rejects_a_missing_token_even_with_the_notes_label() {
        // `token_stale` says "not stale" for an absent token and `is_notes`
        // accepts the label alone — both right for the launcher, both wrong
        // here. A label outlives a dead pane; the token does not.
        let json = pane_list(
            r#"{"pane_id":"w1:p2","tab_id":"w1:t1","label":"Notes"}"#,
        );
        assert_eq!(notes_pane_fresh(&json, "w1:t1", 100), Some(false));
    }

    #[test]
    fn notes_pane_fresh_rejects_an_unparsable_token_value() {
        let json = pane_list(
            r#"{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{"herdr-notes":{"v":1}}}"#,
        );
        assert_eq!(notes_pane_fresh(&json, "w1:t1", 100), Some(false));
    }

    #[test]
    fn notes_pane_fresh_is_none_on_unparsable_json() {
        assert_eq!(notes_pane_fresh("not json", "w1:t1", 100), None);
        assert_eq!(notes_pane_fresh("", "w1:t1", 100), None);
    }

    #[test]
    fn notes_pane_fresh_strips_a_bom() {
        let json = format!(
            "\u{feff}{}",
            pane_list(r#"{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{"herdr-notes":"95"}}"#)
        );
        assert_eq!(notes_pane_fresh(&json, "w1:t1", 100), Some(true));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test launch`
Expected: FAIL — `cannot find function notes_pane_fresh in this scope`.

- [ ] **Step 3: Implement the predicate**

Add to `src/launch.rs`, immediately after `token_stale`:

```rust
/// Whether THIS tab has a Notes pane that is alive right now: `Some(true)` when
/// a pane in `tab_id` carries a `herdr-notes` heartbeat token that is present
/// AND fresh, `Some(false)` when the list parsed and no such pane exists,
/// `None` when the JSON could not be parsed at all.
///
/// Deliberately does NOT use [`Pane::is_notes`], which also accepts the "Notes"
/// LABEL, nor [`token_stale`] alone, which reports a MISSING token as not
/// stale. Both are right for the launcher — it must recognize a pane it has
/// just labeled, before that pane's TUI has reported anything. Both are wrong
/// for the capture gate, which needs evidence the pane is running: a label
/// outlives a dead pane, the token does not.
pub fn notes_pane_fresh(pane_list_json: &str, tab_id: &str, now: u64) -> Option<bool> {
    let msg = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)).ok()?;
    Some(msg.result.panes.iter().any(|p| {
        p.tab_id.as_deref() == Some(tab_id)
            && p.tokens.contains_key(METADATA_SOURCE)
            && !token_stale(&p.tokens, METADATA_SOURCE, now)
    }))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS. Clippy may flag `notes_pane_fresh` as dead code — nothing calls it until Task 3. Add a narrowly-scoped `#[allow(dead_code)]` with a comment naming Task 3 only if the gate demands it, and report which.

- [ ] **Step 5: Commit**

```bash
git add src/launch.rs
git commit -m "feat(launch): notes_pane_fresh, token present AND fresh"
```

---

### Task 2: A bounded socket call

The capture hook runs on the user's keystroke path. Claude Code kills a hook at its configured `timeout: 5`, and it is not established in this repo whether a killed hook blocks the prompt. Bounding the read well short of that makes the question moot instead of answering it on the user's messages.

**Why a thread and not a socket read timeout:** on Windows `roundtrip` opens the named pipe as a plain `std::fs::File`, which has no read-timeout API. A worker thread plus `recv_timeout` is portable and needs no platform-specific code. The worker may outlive the timeout; that is acceptable here because the hook process exits immediately afterward and teardown reaps it.

**Files:**
- Modify: `src/ipc.rs`
- Test: `src/ipc.rs` (add a `mod tests` if the file has none)

**Interfaces:**
- Consumes: `call_text(method: &str, params: serde_json::Value) -> std::io::Result<String>` (already present).
- Produces: `pub fn call_text_bounded(method: &str, params: serde_json::Value, timeout: std::time::Duration) -> std::io::Result<String>` — the same result as `call_text`, or `ErrorKind::TimedOut` if it does not answer in `timeout`.

- [ ] **Step 1: Write the failing tests**

Add to `src/ipc.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_text_bounded_returns_promptly_when_there_is_no_socket() {
        // No HERDR_SOCKET_PATH and no default socket to connect to: the inner
        // call fails fast, so this must return that error rather than waiting
        // out the timeout.
        let started = std::time::Instant::now();
        let out = call_text_bounded(
            "pane.list",
            serde_json::json!({}),
            std::time::Duration::from_secs(3),
        );
        assert!(out.is_err(), "no socket in the test environment");
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
```

If the environment running the suite DOES have a reachable herdr socket, the first test's `is_err` assertion will not hold. In that case change it to assert only the elapsed-time bound and say so in your report — the timing is the property under test, not the failure.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test ipc`
Expected: FAIL — `cannot find function call_text_bounded in this scope`.

- [ ] **Step 3: Implement the bounded call**

Add to `src/ipc.rs`, after `call_text`:

```rust
/// `call_text` with a wall-clock bound. Used on the `--capture-prompt` path,
/// which runs inside a `UserPromptSubmit` hook: Claude Code kills a hook at its
/// configured timeout, so the socket must never be allowed to sit anywhere near
/// it.
///
/// Bounded with a worker thread rather than a socket read timeout because on
/// Windows the named pipe is opened as a plain `File`, which has no
/// read-timeout API. The worker can outlive the bound; that is fine here
/// because the hook process exits straight afterward and teardown reaps it.
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ipc.rs
git commit -m "feat(ipc): call_text_bounded for the prompt-submit hook path"
```

---

### Task 3: The capture gate consults herdr

The gate answer is **injected** into `capture` so every gate test stays pure and socket-free; only `capture_from_env` touches the socket.

**The semantics, resolving the spec contradiction** (see Spec Correction above): the socket answer is purely additive.

```
allowed = notes_live == Some(true) || note_file_exists
```

A live Notes pane opens the gate immediately, and everything that captures today keeps capturing.

**Files:**
- Modify: `src/prompts.rs` — `capture`'s signature and gate 4, `capture_from_env`
- Test: `src/prompts.rs` (the existing `mod tests`)

**Interfaces:**
- Consumes: `launch::notes_pane_fresh` (Task 1), `ipc::call_text_bounded` (Task 2), `state::note_file_in`, `state::store_dir`, `state::unix_now`, `state::id_key`.
- Produces:
  - `pub const GATE_TIMEOUT: std::time::Duration` — the bound for the gate's socket call.
  - `pub fn capture(dir: Option<&Path>, env: &CaptureEnv, stdin: &str, now: u64, notes_live: Option<bool>) -> bool` — one new parameter, appended.
  - `capture_from_env(stdin: &str) -> bool` unchanged in signature; now resolves `notes_live` itself.

- [ ] **Step 1: Write the failing tests**

Every existing `capture(...)` call in `mod tests` needs the new argument. Pass `None` in all of them — that is the socket-unavailable case and preserves exactly what each was proving. Then add:

```rust
    #[test]
    fn capture_needs_no_note_file_when_a_notes_pane_is_live() {
        // The whole point of the phase: opening Notes is enough, without
        // typing anything into the note first.
        let dir = tempdir().join("gate_live_pane");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(capture(Some(&dir), &env_ok(), r#"{"prompt":"from a live pane"}"#, 7, Some(true)));
        let got = load_for_tab(&dir, &crate::state::id_key("w1:t1").unwrap());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].prompts[0].text, "from a live pane");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_falls_back_to_the_note_file_when_no_pane_is_live() {
        // Socket answered "no live Notes pane": everything that captures today
        // must keep capturing. The answer is additive, never subtractive.
        let dir = store_with_note("gate_no_pane");
        assert!(capture(Some(&dir), &env_ok(), r#"{"prompt":"still captured"}"#, 7, Some(false)));
        assert_eq!(captured(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_rejects_with_neither_a_live_pane_nor_a_note_file() {
        let dir = tempdir().join("gate_neither");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for live in [Some(false), None] {
            assert!(!capture(Some(&dir), &env_ok(), r#"{"prompt":"x"}"#, 7, live));
        }
        assert_eq!(load_for_tab(&dir, &crate::state::id_key("w1:t1").unwrap()), vec![]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_falls_back_to_the_note_file_when_the_socket_failed() {
        let dir = store_with_note("gate_socket_down");
        assert!(capture(Some(&dir), &env_ok(), r#"{"prompt":"offline"}"#, 7, None));
        assert_eq!(captured(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_live_pane_does_not_bypass_the_earlier_gates() {
        // The off switch, the in-herdr check and the id checks all still bind
        // regardless of what the socket says.
        let dir = tempdir().join("gate_precedence");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for env in [
            CaptureEnv { no_capture: true, ..env_ok() },
            CaptureEnv { in_herdr: false, ..env_ok() },
            CaptureEnv { tab_id: None, ..env_ok() },
            CaptureEnv { pane_id: Some("../escape".into()), ..env_ok() },
        ] {
            assert!(!capture(Some(&dir), &env, r#"{"prompt":"x"}"#, 7, Some(true)));
        }
        // And a live pane still cannot rescue an unusable payload.
        assert!(!capture(Some(&dir), &env_ok(), "not json", 7, Some(true)));
        let _ = std::fs::remove_dir_all(&dir);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test prompts`
Expected: FAIL to compile — `this function takes 5 arguments but 4 arguments were supplied`.

- [ ] **Step 3: Implement the gate**

In `src/prompts.rs`, add the constant near `RING`:

```rust
/// Wall-clock bound on the capture gate's `pane.list` call. Well short of the
/// hook's own `timeout: 5`, because a hook killed at its limit is a risk to the
/// user's prompt and this path must never approach it.
pub const GATE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);
```

Change `capture`'s signature and its gate 4. The parameter is appended so the existing gate order is untouched:

```rust
/// The `UserPromptSubmit` gate chain. Returns `true` only when an entry was
/// written; every rejection is silent and total, because the caller runs
/// inside a prompt-submit hook.
///
/// Gates, in order: the off switch, running inside herdr, filename-safe tab
/// AND pane ids (no legacy fallback — prompts are per-tab or nothing), a live
/// Notes pane in this tab OR an existing note file, and a usable payload.
///
/// `notes_live` is injected so every gate stays testable without a socket:
/// `Some(true)` a Notes pane in this tab is alive, `Some(false)` the socket
/// answered and none is, `None` the socket could not be reached. It is
/// ADDITIVE — it can only open the gate earlier, never close it on something
/// that captures today — so a socket failure degrades to the note-file check
/// rather than to silent no-capture.
pub fn capture(
    dir: Option<&Path>,
    env: &CaptureEnv,
    stdin: &str,
    now: u64,
    notes_live: Option<bool>,
) -> bool {
```

Replace the note-file gate body with:

```rust
    // A live Notes pane means the user is looking at this tab's note right
    // now, so capture without waiting for them to type something into it —
    // an empty note is `state::is_blank` and therefore has no file at all.
    if notes_live != Some(true) && !state::note_file_in(dir, &tab_key).exists() {
        return false;
    }
```

Keep using `state::note_file_in` rather than re-deriving `{tab_key}.json`, exactly as the current code does.

Then update `capture_from_env`:

```rust
/// `capture` against the real environment, store dir, and herdr socket.
pub fn capture_from_env(stdin: &str) -> bool {
    let env = CaptureEnv::from_process();
    let now = crate::state::unix_now();
    // Bounded, and only worth asking when a tab id makes the answer meaningful.
    let notes_live = env.tab_id.as_deref().and_then(|tab| {
        crate::ipc::call_text_bounded("pane.list", serde_json::json!({}), GATE_TIMEOUT)
            .ok()
            .and_then(|json| crate::launch::notes_pane_fresh(&json, tab, now))
    });
    capture(crate::state::store_dir().as_deref(), &env, stdin, now, notes_live)
}
```

If `notes_pane_fresh` still carries a `#[allow(dead_code)]` from Task 1, delete it now — this is its call site.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Re-verify the two hard hook rules by hand**

These are process behavior no unit test sees. Run against the debug binary and paste the real output into your report:

```bash
cargo build
echo '{"prompt":"hello"}' | ./target/debug/herdr-notes --capture-prompt; echo "exit=$?"
echo 'not json at all' | ./target/debug/herdr-notes --capture-prompt; echo "exit=$?"
printf '' | ./target/debug/herdr-notes --capture-prompt; echo "exit=$?"
```

Expected, all three: `exit=0` and nothing printed on stdout before it. The socket call is new on this path, so this is a re-verification, not a formality.

- [ ] **Step 6: Commit**

```bash
git add src/prompts.rs
git commit -m "feat(prompts): capture when a Notes pane is live, not only when a note file exists"
```

---

### Task 4: Prefer the pane label

**Files:**
- Modify: `src/app.rs` — `PaneInfo` (`:194-201`), `nice_title` (`:203-213`), `build_pane_index` (`:230-248`)
- Test: `src/app.rs` (the existing `mod tests`)

**Interfaces:**
- Consumes: `meaningful_title(&str, &str) -> Option<String>` (unchanged).
- Produces: `PaneInfo` gains `label: Option<String>`; `PaneInfo::nice_title(&self) -> Option<String>` now prefers the label. `pane_label` and `pick_title` are untouched — they already route through `nice_title`.

- [ ] **Step 1: Write the failing tests**

The `mod tests` helper `pane_json(pane_id, tab_id, agent, title, cwd)` has no label parameter. Add a sibling rather than changing every existing call site:

```rust
    fn pane_json_labelled(
        pane_id: &str,
        tab_id: &str,
        agent: Option<&str>,
        title: &str,
        cwd: &str,
        label: Option<&str>,
    ) -> serde_json::Value {
        let mut v = pane_json(pane_id, tab_id, agent, title, cwd);
        if let Some(l) = label {
            v["label"] = serde_json::Value::String(l.to_string());
        }
        v
    }

    #[test]
    fn build_pane_index_reads_the_label_when_present() {
        // herdr omits `label` entirely until one is set — which is exactly how
        // phase C came to claim the field did not exist.
        let panes = vec![
            pane_json_labelled("wD:pE", "wD:t3", Some("claude"), "Claude Code", "C:\\repo", Some("test-1")),
            pane_json("wD:pG", "wD:t3", Some("claude"), "Claude Code", "C:\\repo"),
        ];
        let idx = build_pane_index(&panes);
        assert_eq!(idx.get("wD:pE").unwrap().label.as_deref(), Some("test-1"));
        assert_eq!(idx.get("wD:pG").unwrap().label, None, "absent key -> None");
    }

    #[test]
    fn nice_title_prefers_the_label_over_the_terminal_title() {
        let info = PaneInfo {
            agent: "claude".into(),
            tab_id: "wD:t3".into(),
            title: Some("HM-54271 Importer".into()),
            cwd: None,
            label: Some("test-1".into()),
        };
        assert_eq!(info.nice_title().as_deref(), Some("test-1"));
    }

    #[test]
    fn a_label_bypasses_the_meaningful_title_rejections() {
        // The rejection list exists because the TERMINAL title is machine-set.
        // A label is typed on purpose, so a path-shaped or tool-shaped label is
        // the user's choice and must be honored.
        for label in ["src/app.rs", "Claude Code", "build.exe", "C:\\repo\\thing"] {
            let info = PaneInfo {
                agent: "claude".into(),
                tab_id: "wD:t3".into(),
                title: Some("Claude Code".into()),
                cwd: None,
                label: Some(label.into()),
            };
            assert_eq!(info.nice_title().as_deref(), Some(label), "label {label:?}");
        }
    }

    #[test]
    fn a_blank_label_falls_through_to_the_terminal_title() {
        for label in [Some(""), Some("   "), None] {
            let info = PaneInfo {
                agent: "claude".into(),
                tab_id: "wD:t3".into(),
                title: Some("HM-54271 Importer".into()),
                cwd: None,
                label: label.map(|s| s.to_string()),
            };
            assert_eq!(info.nice_title().as_deref(), Some("HM-54271 Importer"), "label {label:?}");
        }
    }

    #[test]
    fn a_label_is_trimmed() {
        let info = PaneInfo {
            agent: "claude".into(),
            tab_id: "wD:t3".into(),
            title: None,
            cwd: None,
            label: Some("  test-1  ".into()),
        };
        assert_eq!(info.nice_title().as_deref(), Some("test-1"));
    }

    #[test]
    fn pane_label_heads_a_group_with_the_label() {
        let panes = vec![pane_json_labelled(
            "wD:pE", "wD:t3", Some("claude"), "Claude Code", "C:\\repo", Some("test-1"),
        )];
        let idx = build_pane_index(&panes);
        assert_eq!(pane_label("wD:pE", "claude", Some(&idx)), "test-1");
    }
```

Every existing test that constructs `PaneInfo { .. }` literally will need `label`. Add `label: None` — that preserves exactly what each was proving, since `None` falls through to the terminal title.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test`
Expected: FAIL to compile — `missing field label in initializer of PaneInfo`.

- [ ] **Step 3: Add the field and the preference**

In `src/app.rs`, add the field to `PaneInfo`:

```rust
    /// herdr's pane label — the name the user gave this pane. `None` until one
    /// is set: `pane.list` omits the key entirely, which is why a dump taken
    /// before any rename made phase C conclude no such field existed.
    label: Option<String>,
```

Replace `nice_title`:

```rust
    /// The best human-readable name for this pane: its herdr LABEL when set,
    /// else its terminal title when that actually says something. The ONE
    /// definition, shared by `pane_label`, `pick_title` and `maybe_autotitle`'s
    /// source-1 probe — that probe exists only to decide whether to spawn
    /// `git`, so if it ever drifted from `pick_title`'s copy the branch would
    /// be computed when it should not be, or skipped when it should not be.
    ///
    /// A label deliberately does NOT go through `meaningful_title`. That
    /// rejection list — generic tool names, path-shaped strings, a `.exe`
    /// suffix — exists because `terminal_title_stripped` is machine-set and
    /// unreliable. A label is a string the user typed on purpose, so rejecting
    /// `src/app.rs` as path-shaped would be overruling them.
    fn nice_title(&self) -> Option<String> {
        if let Some(label) = self.label.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
            return Some(label.to_string());
        }
        self.title.as_deref().and_then(|t| meaningful_title(t, &self.agent))
    }
```

In `build_pane_index`, add to the constructed `PaneInfo`:

```rust
                label: p.get("label").and_then(|v| v.as_str()).map(|s| s.to_string()),
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS. If `pane_label_prefers_a_meaningful_title` or `pick_title_prefers_a_meaningful_terminal_title` broke, a fixture gained a label it should not have — those cover the no-label path.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "feat(notes): prefer herdr's pane label over the terminal title"
```

---

### Task 5: Live re-derive, and cache the branch

**Files:**
- Modify: `src/app.rs` — `git_tried`'s type and its initializer, `git_branch`, `autotitle_wanted` (`:694-700`), `maybe_autotitle` (`:711-733`)
- Test: `src/app.rs` (the existing `mod tests`)

**Interfaces:**
- Consumes: `PaneInfo::nice_title` (Task 4), `pick_title`, `pick_agent_pane`, `oldest_prompt_text`, `state::is_blank`.
- Produces: `App.git_tried: std::collections::HashMap<String, Option<String>>`; `git_branch(&mut self, cwd: &str) -> Option<String>` unchanged in signature but now cached.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/app.rs`:

```rust
    #[test]
    fn autotitle_wanted_no_longer_requires_an_empty_title() {
        // An auto title tracks its source; only typing one freezes it.
        let mut a = app("a real body");
        a.persist = true;
        a.note.title = "20260728-team-solutions".into();
        a.note.title_auto = true;
        assert!(a.autotitle_wanted(), "a derived title is still derivable");
        a.note.title_auto = false;
        assert!(!a.autotitle_wanted(), "a typed title is frozen");
    }

    #[test]
    fn autotitle_wanted_still_refuses_a_blank_note() {
        // Phase C's rule: deriving into a blank note would defeat the
        // delete-on-save rule and leave an orphan file forever.
        let mut a = app("");
        a.persist = true;
        a.note.title_auto = true;
        assert!(!a.autotitle_wanted());
    }

    #[test]
    fn git_branch_caches_a_success_and_reuses_it() {
        // Under re-derive the chain runs repeatedly. Without caching, a pane
        // that loses its label would fall PAST the branch to the prompt text,
        // because that cwd's one attempt was already spent.
        let mut a = app("body");
        let cwd = std::env::current_dir().unwrap().display().to_string();
        let first = a.git_branch(&cwd);
        assert!(a.git_tried.contains_key(&cwd), "the attempt is remembered");
        assert_eq!(a.git_branch(&cwd), first, "second call returns the cached answer");
        assert_eq!(a.git_tried.len(), 1, "still one entry, still one spawn");
    }

    #[test]
    fn git_branch_still_caches_a_failure_as_none() {
        let mut a = app("body");
        let cwd = "C:\\definitely\\not\\a\\repo\\anywhere";
        assert_eq!(a.git_branch(cwd), None);
        assert_eq!(a.git_tried.get(cwd), Some(&None), "the failure is cached, not retried");
        assert_eq!(a.git_branch(cwd), None);
        assert_eq!(a.git_tried.len(), 1);
    }
```

The re-derive-and-only-touch-on-change behavior needs a real `persist = true` App. Follow `maybe_autotitle_derives_from_the_oldest_prompt_and_is_gated_by_title_state_and_active_note`'s harness exactly — `ENV_LOCK`, a temp store dir, `HERDR_*` redirected and restored, a dead socket. Add:

```rust
    #[test]
    fn maybe_autotitle_re_derives_on_change_and_leaves_the_note_alone_otherwise() {
        // Same harness as the existing autotitle test: temp store, HERDR_* under
        // ENV_LOCK, dead socket so only the prompt source is reachable.
        // (Set up dir/env/note/prompt exactly as that test does, then:)
        //
        // 1. First beat derives from the oldest prompt and sets `dirty`.
        // 2. Clear `dirty`, beat again with nothing changed: the title is the
        //    same and `dirty` must STAY false — otherwise every heartbeat
        //    dirties the note, the 2s autosave fires forever, `updated` keeps
        //    bumping and the header age resets to `just now` on a loop.
        // 3. Append a NEWER prompt whose text differs, and confirm the title
        //    does NOT change: the oldest surviving prompt is still the same
        //    one, so source 4's answer is unchanged. Then remove the older
        //    prompt file so the newer one becomes oldest, beat again, and
        //    confirm the title follows and `dirty` is set.
    }
```

**Write that test out fully rather than leaving the comment** — read the existing autotitle test and mirror its setup and teardown line for line, substituting the three phases above. The point being proved is that `touch()` fires only when the derived value actually differs.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -- autotitle_wanted git_branch maybe_autotitle_re_derives`
Expected: FAIL — `autotitle_wanted_no_longer_requires_an_empty_title` fails its first assertion, and the `git_tried` tests fail to compile (`HashSet` has no `get` returning `Option<&Option<String>>`).

- [ ] **Step 3: Implement**

Change `git_tried`'s declaration on `App`:

```rust
    /// Branch lookups already made, keyed by cwd: `Some(branch)` cached from a
    /// success, `None` cached from a failure. Caching the SUCCESS matters under
    /// re-derive — the chain runs every heartbeat, and a pane that later loses
    /// its label must still be able to fall back to the branch it found the
    /// first time. Caching the FAILURE is what bounds the spawn: without it a
    /// tab that is not a repo would spawn `git` every 5 seconds for the life of
    /// the pane. See `git_branch` for what a hang costs.
    git_tried: std::collections::HashMap<String, Option<String>>,
```

Initialize it in `with_note` as `std::collections::HashMap::new()`.

Replace `git_branch`'s bookkeeping — the guard becomes a cache lookup:

```rust
    fn git_branch(&mut self, cwd: &str) -> Option<String> {
        if let Some(cached) = self.git_tried.get(cwd) {
            return cached.clone();
        }
        let mut cmd = std::process::Command::new("git");
        cmd.args(["rev-parse", "--abbrev-ref", "HEAD"]).current_dir(cwd);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let branch = cmd
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|b| !b.is_empty());
        self.git_tried.insert(cwd.to_string(), branch.clone());
        branch
    }
```

Keep whatever doc comment `git_branch` already carries about the hang-starves-the-heartbeat chain — that warning is why the bound exists and must not be lost.

Drop the empty-title condition from `autotitle_wanted`:

```rust
    /// Whether a title should be derived on this beat. `title_auto` is the
    /// freeze switch — only typing a title with `r` clears it. There is
    /// deliberately no "title is empty" condition: an auto title TRACKS its
    /// source, so renaming a pane updates the note within one heartbeat even
    /// if the branch name had already landed.
    fn autotitle_wanted(&self) -> bool {
        self.persist
            && self.showing_tab_note()
            && self.note.title_auto
            && !state::is_blank(&self.note)
    }
```

In `maybe_autotitle`, only write when the value actually changed:

```rust
        if let Some(title) = pick_title(agent_pane, branch.as_deref(), oldest.as_deref())
            && title != self.note.title
        {
            self.note.title = title;
            self.touch();
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS. The existing `autotitle_only_runs_...`-style tests that asserted an already-titled note is untouched will now be wrong for the `title_auto = true` case — that is the behavior change. Update those assertions to match re-derive, and say in your report which you changed and why; do NOT reintroduce the empty-title gate to keep them green.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "feat(notes): re-derive an auto title while title_auto holds"
```

---

### Task 6: Documentation, including two corrections

**Files:**
- Modify: `CLAUDE.md`, `docs/superpowers/specs/2026-07-27-agent-grouping-design.md`, `README.md`
- Test: manual re-reading against the code

**Interfaces:**
- Consumes: everything from Tasks 1-5.
- Produces: nothing.

- [ ] **Step 1: Correct the two false claims**

Both say herdr exposes no pane label. Both are wrong, and both must say WHY the mistake happened, because that is the transferable part.

In `docs/superpowers/specs/2026-07-27-agent-grouping-design.md`'s ground-truth section, add a blockquote in the style that spec already uses for its amendments, stating that herdr does expose `label`, that the key is absent from `pane.list` until a label is set, and that a dump taken before any rename is therefore not evidence the field does not exist.

In `CLAUDE.md`, correct the gotcha that calls `terminal_title_stripped` the ONLY human-readable per-pane string. It is the only one herdr reports *unprompted*; `label` appears once the user names the pane.

- [ ] **Step 2: Document the new behavior**

In `CLAUDE.md`'s `src/app.rs` bullet: `nice_title` prefers the label, a label bypasses `meaningful_title` because it is user-typed rather than machine-set, and an auto title re-derives every beat while `title_auto` holds, writing only when the value changes.

In the `src/prompts.rs` bullet: the gate is now a live Notes pane OR an existing note file, the socket answer is additive, and `GATE_TIMEOUT` bounds the call.

Add these Gotchas:

- `launch::token_stale` returns FALSE for a MISSING token, and `Pane::is_notes` accepts the `"Notes"` LABEL alone. Both are right for the launcher, which must recognize a pane it has just labeled before that pane's TUI has reported anything. Both are wrong for a liveness check: a label outlives a dead pane. `notes_pane_fresh` therefore requires the token present AND fresh, and anything else asking "is this pane alive" must do the same rather than reusing `is_notes`.
- The capture gate's socket answer is ADDITIVE — `Some(true)` opens the gate early, but `Some(false)` and `None` both fall back to the note-file check. Making it subtractive would stop capture for a tab with a real note the moment its Notes pane closed.
- `ipc::call_text_bounded` bounds with a worker thread, not a socket read timeout, because on Windows the named pipe is a plain `File` with no read-timeout API. The worker may outlive the bound; that is safe only because the hook process exits immediately after.
- An auto title now re-derives every heartbeat, so `maybe_autotitle` MUST compare before writing. Writing unconditionally dirties the note every beat, which autosaves forever, bumps `updated`, and resets the header age to `just now` on a loop.

- [ ] **Step 3: Update the README**

In the prompt-capture section: prompts are captured as soon as a Notes pane is open in the tab — no longer only after the note has content. In the auto-title paragraph: the pane label is the first source, and renaming a pane updates an auto title.

- [ ] **Step 4: Verify every claim against the code**

Re-read each new sentence against the diff. `notes_pane_fresh`, `call_text_bounded`, `GATE_TIMEOUT`, `PaneInfo.label`, `nice_title`'s order, `git_tried`'s type, and `autotitle_wanted`'s conditions must all exist as written.

- [ ] **Step 5: Commit**

```bash
git add CLAUDE.md README.md docs/superpowers/specs/2026-07-27-agent-grouping-design.md
git commit -m "docs: pane labels, the live capture gate, and two corrections"
```

---

### Task 7: Full verification

**Files:** none — verification only.

**Interfaces:**
- Consumes: everything from Tasks 1-6.
- Produces: nothing.

- [ ] **Step 1: Run the full gate**

Close any open Notes pane first (`cargo build --release` fails with os error 5 while the TUI holds the binary), then:

```bash
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 2: Drive it end to end**

**Ask the human partner before touching any live herdr pane** — they have real work running. With their go-ahead, in a tab with two Claude panes:

1. Open Notes and type NOTHING. Submit a prompt from a Claude pane. Within ~5s the block appears — this is the phase's headline change, and it failed before it.
2. Rename a pane. Within ~5s its group heading changes to the new label.
3. Rename a pane to something path-shaped like `src/app.rs`. The heading shows it verbatim — a label is not subject to the terminal-title rejections.
4. With an untitled note, rename a pane; the note's title follows within ~5s even if it had already picked up the branch name.
5. Press `r`, type a name, Enter. It survives every subsequent heartbeat.
6. Confirm the header age does not reset to `just now` every 5s on a note whose derived title is not changing.
7. Close the Notes pane, submit a prompt from a Claude pane in a tab that HAS a note file, reopen Notes: the prompt was still captured (the gate is additive).

- [ ] **Step 3: Report**

Record which steps passed and any that could not be run, and why. Do not report the feature verified on the strength of the unit tests alone.

---

## Self-Review

**Spec coverage:**

| Spec section | Task |
|---|---|
| `PaneInfo.label`, read from `pane.list` | 4 |
| Headings and title chain prefer the label | 4 (both route through `nice_title`) |
| A label bypasses `meaningful_title` | 4 |
| `nice_title` stays the one definition | 4 |
| Re-derive while `title_auto` holds | 5 |
| Only `touch()` on a changed value | 5 |
| `git_tried` caches the branch | 5 |
| Gate consults herdr; three outcomes | 3 |
| Token present AND fresh; the `token_stale` trap | 1 |
| Bounded socket read | 2 |
| Hook still exits 0, never prints | 3 (step 5) |
| Both doc corrections + the new gotchas | 6 |
| Failure modes (socket down, no label, stale token, not a repo) | 1, 3, 4 |
| Testing: every listed case | 1-5 |
| End-to-end against a real multi-agent tab | 7 |

One spec requirement is deliberately NOT implemented as written: Features §3's `Some(false) -> reject`. It contradicts the same spec's Failure Modes, and this plan implements the fall-back reading — see Spec Correction at the top. The user must be told.

**Placeholder scan:** one intentional prose-plus-instruction block, in Task 5 Step 1's re-derive test, where the setup must mirror an existing test's harness line for line and inventing a second harness would be worse. The instruction is explicit that the test must be written out fully. Everything else carries its code.

**Type consistency:** `notes_pane_fresh(&str, &str, u64) -> Option<bool>` (Task 1) is consumed by `capture_from_env` (Task 3). `call_text_bounded(&str, Value, Duration) -> io::Result<String>` (Task 2) is consumed there too. `capture(Option<&Path>, &CaptureEnv, &str, u64, Option<bool>) -> bool` (Task 3) is called with `None` by every pre-existing test. `PaneInfo { agent, tab_id, title, cwd, label }` (Task 4) is constructed in `build_pane_index` and in Task 4's and Task 5's fixtures. `git_tried: HashMap<String, Option<String>>` (Task 5) is read and written only by `git_branch`.
