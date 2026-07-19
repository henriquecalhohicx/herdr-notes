# Notes Manager Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add titles, created/updated timestamps, an in-note list/manage overlay, and empty-note cleanup to the per-tab notes plugin.

**Architecture:** Extend the `Note` value type and its JSON with `title`/`tab_id`/`created`/`updated` (backward-compatible forgiving parse). Add pure data helpers in `state.rs` (enumerate notes, classify a tab as live/closed, format ages) that the TUI drives. Add a modal list overlay and an in-note title editor to `app.rs`. Live-tab status comes from one `pane.list` socket call via the existing `ipc::call_text`.

**Tech Stack:** Rust, ratatui/crossterm TUI, serde_json, unicode-width. Tests are `#[cfg(test)]` modules run with `cargo test`.

## Global Constraints

- All three MUST pass before shipping: `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo build --release`. Do NOT run `cargo build --release` while a Notes TUI is open in a pane (os error 5); unit tests are safe.
- No new dependencies.
- Esc must NEVER quit the TUI — only `q` (in preview, with no overlay/input open) quits. Esc closes overlays/inputs at most.
- The pane border label stays `"Notes"`. The in-note header shows at most ONE title (the note's own title) plus `[preview]`/`[edit]` + scroll. Never render a second "Notes".
- Parsing stays forgiving: any missing/garbled field falls back to a default, never panics. Pre-existing `{text, mode}` files MUST still load.
- Saving stays atomic: temp file + `sync_all` + rename. Path logic takes an injected base dir so tests never touch real APPDATA.
- Metadata token values are strings (unchanged).
- Column math uses `unicode-width`, not char counts, for any cursor/width work.

---

## File Structure

- `src/state.rs` — MODIFY. `Note` gains fields; `parse`/`to_json` handle them; new `persist_at` (atomic write OR delete-when-empty), `save` refactor, `is_blank`, `store_dir`, `list_notes` + `NoteSummary`, `read_note`/`read_note` made `pub(crate)`, `format_age`, `TabStatus` + `classify_tab`, `set_title`.
- `src/app.rs` — MODIFY. New `title_input` + `overlay` state on `App`; `on_key` dispatch to `on_key_title`/`on_key_overlay`; `r` opens the title editor; `l` opens the overlay; header renders the title; `draw_overlay` renders list/preview/rename/confirm; free fn `live_tab_ids`.
- `src/ipc.rs` — unchanged (reuse `call_text`).

---

## Task 1: Note v2 format

**Files:**
- Modify: `src/state.rs` (`Note` struct ~47-52, `parse` ~211-226, `to_json` ~229-235, tests)
- Modify: `src/app.rs` (test `Note { .. }` literals at ~468, ~569 — add `..Default::default()`)

**Interfaces:**
- Produces: `Note { text: String, mode: Mode, title: String, tab_id: String, created: u64, updated: u64 }` (all `pub`); `parse(&str) -> Note`; `to_json(&Note) -> String` round-trip all fields.

- [ ] **Step 1: Write the failing test** — add to `state.rs` `#[cfg(test)] mod tests`:

```rust
#[test]
fn v2_roundtrip_preserves_all_fields() {
    let note = Note {
        text: "body".into(),
        mode: Mode::Edit,
        title: "My Title".into(),
        tab_id: "w9:t1".into(),
        created: 100,
        updated: 200,
    };
    assert_eq!(parse(&to_json(&note)), note);
}

#[test]
fn pre_v2_file_still_parses_with_defaults() {
    let note = parse("{\"text\":\"hi\",\"mode\":\"edit\"}");
    assert_eq!(note.text, "hi");
    assert_eq!(note.mode, Mode::Edit);
    assert_eq!(note.title, "");
    assert_eq!(note.tab_id, "");
    assert_eq!(note.created, 0);
    assert_eq!(note.updated, 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test v2_roundtrip pre_v2_file`
Expected: FAIL to compile — `Note` has no field `title`.

- [ ] **Step 3: Write minimal implementation** — extend the struct:

```rust
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Note {
    /// Raw markdown of the single note.
    pub text: String,
    pub mode: Mode,
    /// Optional user-set title (blank shows as "(untitled)").
    pub title: String,
    /// Raw herdr tab id that owns this note (e.g. "w9:t1"); "" if unknown.
    pub tab_id: String,
    /// Unix seconds; 0 = unknown. `created` is set once, `updated` per save.
    pub created: u64,
    pub updated: u64,
}
```

Update `parse` to read the new fields (append inside `parse`, before building `Note`):

```rust
    let title = value.get("title").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let tab_id = value.get("tab_id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let created = value.get("created").and_then(|v| v.as_u64()).unwrap_or(0);
    let updated = value.get("updated").and_then(|v| v.as_u64()).unwrap_or(0);
    Note { text, mode, title, tab_id, created, updated }
```

Update `to_json`:

```rust
pub fn to_json(note: &Note) -> String {
    serde_json::json!({
        "text": note.text,
        "mode": note.mode.name(),
        "title": note.title,
        "tab_id": note.tab_id,
        "created": note.created,
        "updated": note.updated,
    })
    .to_string()
}
```

- [ ] **Step 4: Fix existing struct literals** so the crate compiles. In `state.rs` test `write_note` helper and any `Note { text, mode }` literal, and in `app.rs` tests at ~468 and ~569, change `Note { text: ..., mode: ... }` to `Note { text: ..., mode: ..., ..Default::default() }`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS (all existing + two new).

- [ ] **Step 6: Commit**

```bash
git add src/state.rs src/app.rs
git commit -m "feat: Note v2 format with title, tab_id, created, updated"
```

---

## Task 2: Empty-note deletion + timestamp stamping

**Files:**
- Modify: `src/state.rs` (`save` ~242-256, add `is_blank`, `persist_at`)

**Interfaces:**
- Consumes: `Note` (Task 1), `unix_now()`, `tab_env()`.
- Produces: `pub fn is_blank(note: &Note) -> bool`; `pub fn persist_at(path: &Path, note: &Note, tab_id: &str, now: u64)`; `save` delegates to it.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn persist_stamps_timestamps_and_tab_id() {
    let dir = temp_base("persist").join("herdr").join("notes");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("w1_t2.json");
    let note = Note { text: "hi".into(), ..Default::default() };
    persist_at(&path, &note, "w1:t2", 500);
    let back = read_note(&path);
    assert_eq!(back.tab_id, "w1:t2");
    assert_eq!(back.created, 500);
    assert_eq!(back.updated, 500);
    // second save preserves created, bumps updated
    persist_at(&path, &back, "w1:t2", 900);
    let back2 = read_note(&path);
    assert_eq!(back2.created, 500);
    assert_eq!(back2.updated, 900);
    let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
}

#[test]
fn persist_deletes_file_when_note_is_blank() {
    let dir = temp_base("blank").join("herdr").join("notes");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("w1_t2.json");
    persist_at(&path, &Note { text: "x".into(), ..Default::default() }, "w1:t2", 1);
    assert!(path.exists());
    // now blank (no text, no title) -> file removed
    persist_at(&path, &Note::default(), "w1:t2", 2);
    assert!(!path.exists(), "blank note deletes its file");
    let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test persist_stamps persist_deletes`
Expected: FAIL to compile — `persist_at` not defined.

- [ ] **Step 3: Write minimal implementation** — add helpers and refactor `save`:

```rust
/// A note with no text and no title carries nothing worth a file.
pub fn is_blank(note: &Note) -> bool {
    note.text.trim().is_empty() && note.title.trim().is_empty()
}

/// Atomic best-effort persist, OR delete the file when the note is blank.
/// `created` is preserved across saves (set once); `updated` is `now`; an
/// empty incoming `tab_id` keeps whatever the note already had.
pub fn persist_at(path: &Path, note: &Note, tab_id: &str, now: u64) {
    if is_blank(note) {
        let _ = std::fs::remove_file(path);
        return;
    }
    let mut out = note.clone();
    if !tab_id.is_empty() {
        out.tab_id = tab_id.to_string();
    }
    out.created = if note.created == 0 { now } else { note.created };
    out.updated = now;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    let written = std::fs::File::create(&tmp).and_then(|mut f| {
        use std::io::Write;
        f.write_all(to_json(&out).as_bytes())?;
        f.sync_all()
    });
    if written.is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

pub fn save(note: &Note) {
    let Some(path) = state_path() else { return };
    persist_at(&path, note, &tab_env().unwrap_or_default(), unix_now());
}
```

Ensure `read_note` is visible to tests (it already is inside the module). Keep the existing `save` doc-comment above the new `save`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/state.rs
git commit -m "feat: delete blank notes on save; stamp created/updated/tab_id"
```

---

## Task 3: Enumerate notes

**Files:**
- Modify: `src/state.rs` (add `store_dir`, `NoteSummary`, `list_notes`; make `read_note` `pub(crate)`)

**Interfaces:**
- Consumes: `parse`, `read_note`, `StoreBase`/`store_base`.
- Produces:
  ```rust
  pub struct NoteSummary { pub file: PathBuf, pub tab_id: String,
      pub title: String, pub updated: u64, pub nonempty: bool, pub preview: String }
  pub fn store_dir() -> Option<PathBuf>          // the dir holding per-note files
  pub fn list_notes(dir: &Path) -> Vec<NoteSummary>  // newest-updated first
  ```

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn list_notes_summarizes_and_sorts_newest_first() {
    let dir = temp_base("list").join("notes");
    std::fs::create_dir_all(&dir).unwrap();
    persist_at(&dir.join("w1_t1.json"),
        &Note { text: "first line\nmore".into(), title: "Old".into(), ..Default::default() },
        "w1:t1", 100);
    persist_at(&dir.join("w1_t2.json"),
        &Note { text: "newer".into(), ..Default::default() }, "w1:t2", 300);
    // a temp file must be ignored
    std::fs::write(dir.join("w1_t3.json.tmp"), "garbage").unwrap();
    let notes = list_notes(&dir);
    assert_eq!(notes.len(), 2, "only .json files");
    assert_eq!(notes[0].tab_id, "w1:t2", "newest updated first");
    assert_eq!(notes[0].preview, "newer");
    assert_eq!(notes[1].title, "Old");
    assert_eq!(notes[1].preview, "first line");
    assert!(notes[1].nonempty);
    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test list_notes_summarizes`
Expected: FAIL to compile — `list_notes`/`NoteSummary` undefined.

- [ ] **Step 3: Write minimal implementation**

Change `fn read_note` to `pub(crate) fn read_note`. Add:

```rust
/// The directory holding per-note files for THIS process, or None outside herdr
/// with no config dir. Mirrors `state_path` but yields the containing dir.
pub fn store_dir() -> Option<PathBuf> {
    Some(match store_base()? {
        StoreBase::PluginState(dir) => dir,
        StoreBase::Config(base) => base.join("herdr").join("notes"),
    })
}

/// One row of the notes list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NoteSummary {
    pub file: PathBuf,
    pub tab_id: String,
    pub title: String,
    pub updated: u64,
    pub nonempty: bool,
    pub preview: String,
}

/// All notes in `dir`, newest `updated` first. Skips non-`.json` files (so the
/// `.json.tmp` write-temp is ignored). Never panics on a garbled file.
pub fn list_notes(dir: &Path) -> Vec<NoteSummary> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let note = read_note(&path);
        let preview: String = note.text.lines().next().unwrap_or("").trim().chars().take(48).collect();
        out.push(NoteSummary {
            file: path,
            tab_id: note.tab_id,
            title: note.title,
            updated: note.updated,
            nonempty: !note.text.trim().is_empty(),
            preview,
        });
    }
    out.sort_by(|a, b| b.updated.cmp(&a.updated));
    out
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/state.rs
git commit -m "feat: list_notes enumeration + store_dir"
```

---

## Task 4: Age formatting

**Files:**
- Modify: `src/state.rs` (add `format_age`)

**Interfaces:**
- Produces: `pub fn format_age(secs_ago: u64) -> String`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn format_age_covers_boundaries() {
    assert_eq!(format_age(0), "just now");
    assert_eq!(format_age(59), "just now");
    assert_eq!(format_age(60), "1m");
    assert_eq!(format_age(3599), "59m");
    assert_eq!(format_age(3600), "1h");
    assert_eq!(format_age(86_399), "23h");
    assert_eq!(format_age(86_400), "1d");
    assert_eq!(format_age(604_799), "6d");
    assert_eq!(format_age(604_800), "1w");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test format_age_covers`
Expected: FAIL to compile — `format_age` undefined.

- [ ] **Step 3: Write minimal implementation**

```rust
/// Human age from a "seconds ago" delta: just now / Nm / Nh / Nd / Nw.
pub fn format_age(secs_ago: u64) -> String {
    match secs_ago {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m", secs_ago / 60),
        3600..=86_399 => format!("{}h", secs_ago / 3600),
        86_400..=604_799 => format!("{}d", secs_ago / 86_400),
        _ => format!("{}w", secs_ago / 604_800),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test format_age_covers`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/state.rs
git commit -m "feat: format_age helper"
```

---

## Task 5: Tab status classifier + live-tab lookup

**Files:**
- Modify: `src/state.rs` (add `TabStatus`, `classify_tab`)
- Modify: `src/app.rs` (add free fn `live_tab_ids`)

**Interfaces:**
- Produces:
  ```rust
  #[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum TabStatus { Live, Closed, Unknown }
  pub fn classify_tab(tab_id: &str, live: Option<&std::collections::HashSet<String>>) -> TabStatus
  ```
  and in `app.rs`: `fn live_tab_ids() -> Option<std::collections::HashSet<String>>`.

- [ ] **Step 1: Write the failing test** (in `state.rs` tests):

```rust
#[test]
fn classify_tab_maps_live_closed_unknown() {
    use std::collections::HashSet;
    let live: HashSet<String> = ["w1:t1".to_string()].into_iter().collect();
    assert_eq!(classify_tab("w1:t1", Some(&live)), TabStatus::Live);
    assert_eq!(classify_tab("w1:t9", Some(&live)), TabStatus::Closed);
    assert_eq!(classify_tab("", Some(&live)), TabStatus::Unknown, "no owner id");
    assert_eq!(classify_tab("w1:t1", None), TabStatus::Unknown, "socket unavailable");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test classify_tab_maps`
Expected: FAIL to compile — `TabStatus`/`classify_tab` undefined.

- [ ] **Step 3: Write minimal implementation** (in `state.rs`):

```rust
/// Whether the tab that owns a note is still open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TabStatus {
    Live,
    Closed,
    Unknown,
}

/// Classify a note's owner tab against the set of live tab ids. `None` (socket
/// unreachable) or an empty owner id is Unknown; otherwise Live iff present.
pub fn classify_tab(tab_id: &str, live: Option<&std::collections::HashSet<String>>) -> TabStatus {
    match live {
        None => TabStatus::Unknown,
        Some(_) if tab_id.is_empty() => TabStatus::Unknown,
        Some(set) => {
            if set.contains(tab_id) {
                TabStatus::Live
            } else {
                TabStatus::Closed
            }
        }
    }
}
```

- [ ] **Step 4: Add the socket glue** in `app.rs` (below the imports, a free fn). It has no unit test (thin wrapper over `ipc`); it is exercised via the overlay later.

```rust
/// Distinct tab ids of all live panes (one `pane.list` round-trip). None when
/// the socket is unavailable (running the binary by hand outside herdr).
fn live_tab_ids() -> Option<std::collections::HashSet<String>> {
    let resp = crate::ipc::call_text("pane.list", serde_json::json!({})).ok()?;
    let value: serde_json::Value =
        serde_json::from_str(resp.trim_start_matches('\u{feff}')).ok()?;
    let panes = value.get("result")?.get("panes")?.as_array()?;
    let mut set = std::collections::HashSet::new();
    for pane in panes {
        if let Some(tab) = pane.get("tab_id").and_then(|t| t.as_str()) {
            set.insert(tab.to_string());
        }
    }
    Some(set)
}
```

- [ ] **Step 5: Run tests + clippy**

Run: `cargo test classify_tab_maps && cargo clippy --all-targets -- -D warnings`
Expected: PASS, no warnings (note: `live_tab_ids` is now unused — add `#[allow(dead_code)]` above it with a comment `// used by the overlay in a later task`, removed in Task 7).

- [ ] **Step 6: Commit**

```bash
git add src/state.rs src/app.rs
git commit -m "feat: TabStatus classifier + live_tab_ids socket lookup"
```

---

## Task 6: In-note title editor + header title

**Files:**
- Modify: `src/app.rs` (`App` struct ~31-48, `with_note` ~58-73, `on_key` ~140-160, `on_key_preview` ~162-181, `draw` header ~316-337, add `on_key_title`)

**Interfaces:**
- Consumes: `Note.title`, `save`.
- Produces: `App.title_input: Option<String>`; `r` in preview opens it; header shows title / input.

- [ ] **Step 1: Write the failing test** (in `app.rs` tests):

```rust
#[test]
fn r_edits_title_enter_saves_esc_cancels() {
    let mut a = app("body");
    a.on_key(key(KeyCode::Char('r')));
    assert!(a.title_input.is_some(), "r opens the title editor");
    a.on_key(key(KeyCode::Char('H')));
    a.on_key(key(KeyCode::Char('i')));
    a.on_key(key(KeyCode::Enter));
    assert!(a.title_input.is_none());
    assert_eq!(a.note.title, "Hi");
    // Esc cancels an edit without changing the saved title
    a.on_key(key(KeyCode::Char('r')));
    a.on_key(key(KeyCode::Char('X')));
    a.on_key(key(KeyCode::Esc));
    assert_eq!(a.note.title, "Hi", "Esc discards the title edit");
    assert!(!a.on_key(key(KeyCode::Esc)), "Esc still never quits");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test r_edits_title`
Expected: FAIL to compile — no field `title_input`.

- [ ] **Step 3: Add state + dispatch.** In the `App` struct add:

```rust
    /// Some(buf) while editing THIS note's title (opened with `r`).
    title_input: Option<String>,
```

In `with_note`, initialize `title_input: None,` in the struct literal.

In `on_key`, insert this BEFORE the `confirm_clear` block:

```rust
        if self.title_input.is_some() {
            self.on_key_title(key);
            return false;
        }
```

Add the handler:

```rust
    fn on_key_title(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                if let Some(buf) = self.title_input.take() {
                    self.note.title = buf.trim().to_string();
                    self.save();
                }
            }
            KeyCode::Esc => self.title_input = None,
            KeyCode::Backspace => {
                if let Some(buf) = self.title_input.as_mut() {
                    buf.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(buf) = self.title_input.as_mut() {
                    buf.push(c);
                }
            }
            _ => {}
        }
    }
```

In `on_key_preview`, add an arm:

```rust
            KeyCode::Char('r') => self.title_input = Some(self.note.title.clone()),
```

- [ ] **Step 4: Render the title in the header.** Replace the header-building block in `draw` (the `let mut title = vec![...]` through `render_widget(Paragraph::new(Line::from(title)), title_a);`) with:

```rust
        let mut title: Vec<Span> = Vec::new();
        if let Some(buf) = &self.title_input {
            title.push(Span::styled(
                format!(" Title: {buf}"),
                Style::default().fg(Color::Yellow),
            ));
            title.push(Span::styled(
                "  (Enter save, Esc cancel)",
                Style::default().add_modifier(Modifier::DIM),
            ));
        } else {
            if !self.note.title.trim().is_empty() {
                title.push(Span::styled(
                    format!(" {}", self.note.title),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                title.push(Span::raw(" —"));
            }
            title.push(Span::styled(
                format!(" [{mode}]"),
                Style::default().fg(Color::Cyan),
            ));
            if let Some(hint) = scroll_hint {
                title.push(Span::styled(
                    format!("  {hint}"),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
        }
        frame.render_widget(Paragraph::new(Line::from(title)), title_a);
```

- [ ] **Step 5: Update the preview hint line** (~340) to advertise `r`:

```rust
            Mode::Preview => " e edit  r title  l list  Up/Dn scroll  x clear  q quit",
```

- [ ] **Step 6: Run tests + clippy**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/app.rs
git commit -m "feat: in-note title editor (r) + header title"
```

---

## Task 7: List overlay — open, navigate, render

**Files:**
- Modify: `src/app.rs` (`App` struct, `with_note`, `on_key`, `on_key_preview`, `draw`, add `Overlay`/`OverlayMode`/`OverlayEntry` types, `open_overlay`, `on_key_overlay`, `handle_overlay`, `draw_overlay`; remove the `#[allow(dead_code)]` from `live_tab_ids`)

**Interfaces:**
- Consumes: `state::{list_notes, store_dir, classify_tab, format_age, unix_now, read_note, TabStatus}`, `live_tab_ids`, `state::tab_env` (make `tab_env` `pub(crate)` in `state.rs`).
- Produces: `App.overlay: Option<Overlay>`; `l` opens it; List-mode navigation + `esc`/`l` close.

- [ ] **Step 1: Make `tab_env` reachable.** In `state.rs` change `fn tab_env` to `pub(crate) fn tab_env`.

- [ ] **Step 2: Write the failing test** (in `app.rs` tests). Because `open_overlay` reads the real store dir, the test drives the pieces that do not touch disk by constructing an overlay directly through a test-only constructor. Add this test AND a `#[cfg(test)]` helper:

```rust
#[test]
fn overlay_opens_navigates_and_closes() {
    let mut a = app("body");
    a.overlay = Some(Overlay {
        entries: vec![
            OverlayEntry { file: "a.json".into(), title: "A".into(), updated: 0,
                status: state::TabStatus::Live, text: "aa".into(), is_self: true },
            OverlayEntry { file: "b.json".into(), title: String::new(), updated: 0,
                status: state::TabStatus::Closed, text: "bb".into(), is_self: false },
        ],
        selected: 0,
        mode: OverlayMode::List,
    });
    a.on_key(key(KeyCode::Down));
    assert_eq!(a.overlay.as_ref().unwrap().selected, 1);
    a.on_key(key(KeyCode::Down)); // clamps at last
    assert_eq!(a.overlay.as_ref().unwrap().selected, 1);
    a.on_key(key(KeyCode::Up));
    assert_eq!(a.overlay.as_ref().unwrap().selected, 0);
    assert!(!a.on_key(key(KeyCode::Esc)), "Esc closes overlay, never quits");
    assert!(a.overlay.is_none());
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test overlay_opens_navigates`
Expected: FAIL to compile — `Overlay`/`OverlayEntry`/`OverlayMode` undefined.

- [ ] **Step 4: Add the types** (top of `app.rs`, after imports):

```rust
/// One row in the list overlay.
struct OverlayEntry {
    file: std::path::PathBuf,
    title: String,
    updated: u64,
    status: state::TabStatus,
    text: String,
    is_self: bool,
}

/// Sub-mode of the open list overlay.
enum OverlayMode {
    List,
    Preview { scroll: usize },
    Rename(String),
    ConfirmDelete,
}

/// The list overlay: all notes on disk, browsable/manageable over the note.
struct Overlay {
    entries: Vec<OverlayEntry>,
    selected: usize,
    mode: OverlayMode,
}
```

- [ ] **Step 5: Add state + dispatch.** In `App` add `overlay: Option<Overlay>,`; initialize `overlay: None,` in `with_note`. In `on_key`, insert BEFORE the `title_input` block:

```rust
        if self.overlay.is_some() {
            self.on_key_overlay(key);
            return false;
        }
```

In `on_key_preview` add:

```rust
            KeyCode::Char('l') => self.open_overlay(),
```

Add the open + key handlers (the `take()`/put-back pattern avoids borrowing `self.overlay` across `self` mutations):

```rust
    fn open_overlay(&mut self) {
        let Some(dir) = state::store_dir() else { return };
        let live = live_tab_ids();
        let self_tab = state::tab_env().unwrap_or_default();
        let entries = state::list_notes(&dir)
            .into_iter()
            .map(|s| {
                let text = state::read_note(&s.file).text;
                OverlayEntry {
                    status: state::classify_tab(&s.tab_id, live.as_ref()),
                    is_self: !self_tab.is_empty() && s.tab_id == self_tab,
                    file: s.file,
                    title: s.title,
                    updated: s.updated,
                    text,
                }
            })
            .collect();
        self.overlay = Some(Overlay { entries, selected: 0, mode: OverlayMode::List });
    }

    fn on_key_overlay(&mut self, key: KeyEvent) {
        let Some(mut ov) = self.overlay.take() else { return };
        if self.handle_overlay(&mut ov, key) {
            self.overlay = Some(ov);
        }
    }

    /// Returns false when the overlay should close.
    fn handle_overlay(&mut self, ov: &mut Overlay, key: KeyEvent) -> bool {
        let last = ov.entries.len().saturating_sub(1);
        match &mut ov.mode {
            OverlayMode::List => match key.code {
                KeyCode::Esc | KeyCode::Char('l') => return false,
                KeyCode::Up | KeyCode::Char('k') => ov.selected = ov.selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => ov.selected = (ov.selected + 1).min(last),
                _ => {}
            },
            _ => {}
        }
        true
    }
```

- [ ] **Step 6: Render the overlay.** Remove `#[allow(dead_code)]` from `live_tab_ids`. In `draw`, after the `if self.confirm_clear { draw_confirm(...) }` block, add:

```rust
        if let Some(ov) = &self.overlay {
            draw_overlay(frame, area, ov);
        }
```

Add the drawing fn (List mode now; other modes are filled in Task 8 but render as the list for now):

```rust
fn draw_overlay(frame: &mut Frame, area: Rect, ov: &Overlay) {
    let w = area.width.saturating_sub(4).min(60).max(20);
    let h = area.height.saturating_sub(2).min(u16::try_from(ov.entries.len() + 2).unwrap_or(u16::MAX)).max(3);
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, rect);
    let now = state::unix_now();
    let mut lines: Vec<Line> = Vec::new();
    for (i, e) in ov.entries.iter().enumerate() {
        let marker = if i == ov.selected { ">" } else { " " };
        let name = if e.title.trim().is_empty() { "(untitled)".to_string() } else { e.title.clone() };
        let age = if e.updated == 0 { "—".to_string() } else { state::format_age(now.saturating_sub(e.updated)) };
        let status = match e.status {
            state::TabStatus::Live => "live",
            state::TabStatus::Closed => "closed",
            state::TabStatus::Unknown => "?",
        };
        let self_mark = if e.is_self { "*" } else { " " };
        let style = if i == ov.selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::styled(
            format!("{marker}{self_mark}{name:<28.28} {age:>7}  {status}"),
            style,
        ));
    }
    if lines.is_empty() {
        lines.push(Line::from("(no notes)"));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(" All notes   ↑↓ move  enter preview  r rename  d delete  esc ")),
        rect,
    );
}
```

- [ ] **Step 7: Run tests + clippy + build**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 8: Commit**

```bash
git add src/app.rs src/state.rs
git commit -m "feat: list overlay — open (l), navigate, render live/closed status"
```

---

## Task 8: Overlay preview, rename, delete

**Files:**
- Modify: `src/app.rs` (`handle_overlay`, `draw_overlay`)
- Modify: `src/state.rs` (add `set_title`)

**Interfaces:**
- Consumes: `OverlayMode` (Task 7), `render_markdown`, `state::{persist_at, read_note, unix_now}`.
- Produces: `pub fn set_title(file: &Path, title: &str)` in `state.rs`; `enter`/`r`/`d` behaviors in the overlay.

- [ ] **Step 1: Write the failing test** (in `state.rs` tests):

```rust
#[test]
fn set_title_updates_the_file() {
    let dir = temp_base("settitle").join("notes");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("w1_t1.json");
    persist_at(&path, &Note { text: "body".into(), ..Default::default() }, "w1:t1", 10);
    set_title(&path, "  New Name  ");
    assert_eq!(read_note(&path).title, "New Name");
    let _ = std::fs::remove_dir_all(&dir);
}
```

Also add an `app.rs` test for the delete/rename key flow:

```rust
#[test]
fn overlay_delete_confirm_removes_row() {
    let mut a = app("body");
    a.overlay = Some(Overlay {
        entries: vec![OverlayEntry { file: "x.json".into(), title: "X".into(),
            updated: 0, status: state::TabStatus::Closed, text: "xx".into(), is_self: false }],
        selected: 0,
        mode: OverlayMode::List,
    });
    a.on_key(key(KeyCode::Char('d')));
    assert!(matches!(a.overlay.as_ref().unwrap().mode, OverlayMode::ConfirmDelete));
    a.on_key(key(KeyCode::Char('n'))); // decline
    assert_eq!(a.overlay.as_ref().unwrap().entries.len(), 1);
    a.on_key(key(KeyCode::Char('d')));
    a.on_key(key(KeyCode::Char('y'))); // confirm — file path doesn't exist, remove_file is best-effort
    assert!(a.overlay.as_ref().unwrap().entries.is_empty(), "row removed after confirm");
}

#[test]
fn overlay_rename_enter_updates_row() {
    let mut a = app("body");
    a.overlay = Some(Overlay {
        entries: vec![OverlayEntry { file: "x.json".into(), title: String::new(),
            updated: 0, status: state::TabStatus::Closed, text: "xx".into(), is_self: false }],
        selected: 0,
        mode: OverlayMode::List,
    });
    a.on_key(key(KeyCode::Char('r')));
    a.on_key(key(KeyCode::Char('Z')));
    a.on_key(key(KeyCode::Enter));
    assert_eq!(a.overlay.as_ref().unwrap().entries[0].title, "Z");
    assert!(matches!(a.overlay.as_ref().unwrap().mode, OverlayMode::List));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test set_title_updates overlay_delete_confirm overlay_rename_enter`
Expected: FAIL — `set_title` undefined; `d`/`r` in overlay do nothing.

- [ ] **Step 3: Add `set_title`** in `state.rs`:

```rust
/// Set a note file's title in place (blank text + blank title would delete it).
pub fn set_title(file: &Path, title: &str) {
    let mut note = read_note(file);
    note.title = title.trim().to_string();
    let tab_id = note.tab_id.clone();
    persist_at(file, &note, &tab_id, unix_now());
}
```

- [ ] **Step 4: Extend `handle_overlay`** — replace its body with the full state machine:

```rust
    fn handle_overlay(&mut self, ov: &mut Overlay, key: KeyEvent) -> bool {
        let last = ov.entries.len().saturating_sub(1);
        match &mut ov.mode {
            OverlayMode::List => match key.code {
                KeyCode::Esc | KeyCode::Char('l') => return false,
                KeyCode::Up | KeyCode::Char('k') => ov.selected = ov.selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => ov.selected = (ov.selected + 1).min(last),
                KeyCode::Enter => {
                    if !ov.entries.is_empty() {
                        ov.mode = OverlayMode::Preview { scroll: 0 };
                    }
                }
                KeyCode::Char('r') => {
                    if let Some(e) = ov.entries.get(ov.selected) {
                        ov.mode = OverlayMode::Rename(e.title.clone());
                    }
                }
                KeyCode::Char('d') => {
                    if !ov.entries.is_empty() {
                        ov.mode = OverlayMode::ConfirmDelete;
                    }
                }
                _ => {}
            },
            OverlayMode::Preview { scroll } => match key.code {
                KeyCode::Esc => ov.mode = OverlayMode::List,
                KeyCode::Up => *scroll = scroll.saturating_sub(1),
                KeyCode::Down => *scroll = scroll.saturating_add(1),
                _ => {}
            },
            OverlayMode::Rename(buf) => match key.code {
                KeyCode::Enter => {
                    let title = buf.trim().to_string();
                    if let Some(e) = ov.entries.get_mut(ov.selected) {
                        state::set_title(&e.file, &title);
                        e.title = title.clone();
                        if e.is_self {
                            self.note.title = title;
                        }
                    }
                    ov.mode = OverlayMode::List;
                }
                KeyCode::Esc => ov.mode = OverlayMode::List,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            },
            OverlayMode::ConfirmDelete => {
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    if let Some(e) = ov.entries.get(ov.selected) {
                        let _ = std::fs::remove_file(&e.file);
                        if e.is_self {
                            self.note.text.clear();
                            self.note.title.clear();
                        }
                    }
                    if ov.selected <= last && !ov.entries.is_empty() {
                        ov.entries.remove(ov.selected);
                    }
                    ov.selected = ov.selected.min(ov.entries.len().saturating_sub(1));
                }
                ov.mode = OverlayMode::List;
            }
        }
        true
    }
```

- [ ] **Step 5: Extend `draw_overlay`** to render the Preview / Rename / ConfirmDelete sub-modes. Replace the final `frame.render_widget(Paragraph::new(lines)...)` with a `match &ov.mode`:

```rust
    match &ov.mode {
        OverlayMode::Preview { scroll } => {
            let text = ov.entries.get(ov.selected).map(|e| e.text.as_str()).unwrap_or("");
            let text_w = usize::from(rect.width).saturating_sub(2).max(1);
            let rendered = render_markdown(text, text_w);
            let s = u16::try_from(*scroll).unwrap_or(u16::MAX);
            frame.render_widget(
                Paragraph::new(rendered)
                    .scroll((s, 0))
                    .block(Block::bordered().title(" Preview   Up/Dn scroll   esc back ")),
                rect,
            );
        }
        OverlayMode::Rename(buf) => {
            frame.render_widget(
                Paragraph::new(format!(" Title: {buf}"))
                    .block(Block::bordered().title(" Rename   Enter save   Esc cancel ")),
                rect,
            );
        }
        OverlayMode::ConfirmDelete => {
            let name = ov.entries.get(ov.selected)
                .map(|e| if e.title.trim().is_empty() { "(untitled)".to_string() } else { e.title.clone() })
                .unwrap_or_default();
            frame.render_widget(
                Paragraph::new(format!(" Delete \"{name}\"? y/N"))
                    .block(Block::bordered().title(" Delete ")),
                rect,
            );
        }
        OverlayMode::List => {
            frame.render_widget(
                Paragraph::new(lines)
                    .block(Block::bordered().title(" All notes   ↑↓ move  enter preview  r rename  d delete  esc ")),
                rect,
            );
        }
    }
```

(Remove the earlier single List-only `render_widget` call so it is not rendered twice.)

- [ ] **Step 6: Run the full suite + clippy**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 7: Commit**

```bash
git add src/app.rs src/state.rs
git commit -m "feat: overlay preview, rename (r), delete (d) with confirm"
```

---

## Task 9: Docs + end-to-end verification

**Files:**
- Modify: `CLAUDE.md` (Layout: note the title/timestamps, overlay keys, empty-note deletion)
- Modify: `README.md` (feature bullets + Persistence section)

- [ ] **Step 1: Update `CLAUDE.md`** `src/state.rs` and `src/app.rs` bullets: mention the v2 fields, blank-note deletion, `list_notes`/`classify_tab`, and the `l` overlay (`enter` preview, `r` rename, `d` delete) plus in-note `r` title. Add a Gotcha: "empty (no text, no title) notes are deleted on save, so toggling notes into a tab and closing without typing leaves no file."

- [ ] **Step 2: Update `README.md`** "Why you'll keep it open" with a titles + list bullet, and the Persistence section with the v2 format `{ text, mode, title, tab_id, created, updated }` and the overlay/cleanup behavior.

- [ ] **Step 3: Build the release binary** (ensure no Notes pane is open first).

Run: `cargo build --release`
Expected: `Finished`.

- [ ] **Step 4: End-to-end drive** in a throwaway session (isolated `HERDR_PLUGIN_STATE_DIR`), per the CLAUDE.md "End-to-end verification" recipe: open the binary in a tab, type text, press `r` and set a title, `Esc`, `q`; confirm the JSON on disk has `title`/`created`/`updated`; open a second tab's note, press `l`, confirm both notes list with `live`/`closed` status; delete one with `d`; confirm its file is gone.

- [ ] **Step 5: Commit**

```bash
git add CLAUDE.md README.md
git commit -m "docs: document titles, timestamps, notes overlay, empty-note cleanup"
```

---

## Self-Review notes

- Spec coverage: v2 format (T1), empty-note deletion (T2), enumeration (T3), age (T4), live/closed status (T5), in-note title `r` (T6), overlay open/nav/status (T7), preview/rename/delete (T8), docs+e2e (T9). All spec sections mapped.
- Backward compat: T1 `pre_v2_file_still_parses_with_defaults` proves old files load.
- Esc-never-quits: T6 and T7 assert it explicitly with overlay/input open.
- Borrow safety: overlay handled via `self.overlay.take()` then `handle_overlay(&mut ov, ..)` so `self` methods (`self.note`, `state::*`) are callable without aliasing `self.overlay`.
