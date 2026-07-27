//! Notes TUI: preview/edit modes over one scrollable markdown note, autosaved
//! to this tab's note file (see state.rs), heartbeating a pane identity
//! token so the launcher can toggle / focus / replace the pane.
//!
//! There is no manual save workflow — everything autosaves — and the only
//! destructive action (`x`, clear the note) sits behind a y/N confirm.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use unicode_width::UnicodeWidthChar;

use crate::markdown::{self, render_markdown, render_markdown_mapped};
use crate::state::{self, METADATA_SOURCE, Mode, Note, PANE_LABEL};
use crate::template;

/// Debounce for the edit-mode autosave.
const AUTOSAVE_AFTER: Duration = Duration::from_secs(2);
/// Identity re-stamp interval (launcher stale threshold is 20s).
const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);

/// Shown in preview when the note is empty: the skeleton `e` would seed,
/// plus the quick-start help. Built from `template::DEFAULT` so the preview
/// cannot advertise a template different from the one that gets written.
fn empty_help() -> String {
    format!(
        "(empty note — press e to start with this template)\n\
         (Status is one line on where this stands; e lands you on it)\n\n{}\n\
         \n  e or Enter  start writing\
         \n  l           all notes\
         \n  q           quit\n\nEverything autosaves and survives restarts.",
        template::DEFAULT
    )
}

/// One row in the list overlay.
struct OverlayEntry {
    file: std::path::PathBuf,
    title: String,
    /// Owning tab id ("" for the pinned global row) — used by go-to-tab (`g`).
    tab_id: String,
    updated: u64,
    status: state::TabStatus,
    text: String,
    is_self: bool,
    /// The pinned `★ Global` row: switches the pane's active note on `enter`
    /// instead of previewing; immune to rename/delete/go-to-tab/filter.
    is_global: bool,
    /// Precomputed row context string ("workspace · agent" / "closed" / "?" /
    /// "global") — see `state::format_context`. Matched against by the filter
    /// (`recompute_visible`) and rendered in the row's right-hand column.
    context: String,
}

/// Sub-mode of the open list overlay.
enum OverlayMode {
    List,
    Filter,
    Preview { scroll: usize },
    Rename(String),
    ConfirmDelete,
}

/// The list overlay: all notes on disk, browsable/manageable over the note.
/// `selected` indexes into `visible`, not `entries` directly, so the filter
/// can narrow which rows are shown without disturbing `entries`.
struct Overlay {
    entries: Vec<OverlayEntry>,
    visible: Vec<usize>,
    selected: usize,
    mode: OverlayMode,
    filter: String,
    /// Row offset of the list viewport, kept so the selected row stays on
    /// screen when the list is longer than the box (updated each draw).
    list_scroll: usize,
}

impl Overlay {
    fn from_entries(entries: Vec<OverlayEntry>) -> Self {
        let mut ov = Overlay {
            entries,
            visible: Vec::new(),
            selected: 0,
            mode: OverlayMode::List,
            filter: String::new(),
            list_scroll: 0,
        };
        ov.recompute_visible();
        ov
    }

    /// Recomputes which entries are shown for the current filter (empty =
    /// all). The pinned global row (if present) is always first and always
    /// visible — a fixed anchor, not a searchable note. Clamps `selected`.
    fn recompute_visible(&mut self) {
        let global_idx = self.entries.iter().position(|e| e.is_global);
        let rows: Vec<state::FilterRow> = self
            .entries
            .iter()
            .map(|e| state::FilterRow { title: &e.title, context: &e.context })
            .collect();
        let mut visible = state::filter_rows(&rows, &self.filter);
        if let Some(gi) = global_idx {
            visible.retain(|&i| i != gi);
            visible.insert(0, gi);
        }
        self.visible = visible;
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
    }

    fn selected_entry(&self) -> Option<&OverlayEntry> {
        self.visible.get(self.selected).map(|&i| &self.entries[i])
    }

    fn selected_entry_mut(&mut self) -> Option<(usize, &mut OverlayEntry)> {
        let idx = *self.visible.get(self.selected)?;
        Some((idx, &mut self.entries[idx]))
    }
}

/// Live tabs plus their session context (workspace + agent), built from one
/// `tab.list` + `workspace.list` + `pane.list` round-trip when the overlay
/// opens. `None` when any call/parse fails (socket unreachable, or running
/// outside herdr) — every row then falls back to Unknown.
struct TabIndex {
    live: std::collections::HashSet<String>,
    ctx: std::collections::HashMap<String, state::RowContext>,
}

fn tab_contexts() -> Option<TabIndex> {
    let tabs = fetch_array("tab.list", "tabs")?;
    let workspaces = fetch_array("workspace.list", "workspaces")?;
    let panes = fetch_array("pane.list", "panes")?;
    Some(build_tab_index(&tabs, &workspaces, &panes))
}

/// Pure builder over the three already-fetched socket arrays — no I/O, so it
/// is unit-tested against captured live responses. Field names verified live
/// on herdr 0.7.4: tabs carry `tab_id`+`workspace_id`, workspaces carry
/// `workspace_id`+`label`, panes carry `tab_id` and (only once an agent is
/// reported) `agent`. Any missing/mistyped field on an item just skips that
/// item — never panics.
fn build_tab_index(
    tabs: &[serde_json::Value],
    workspaces: &[serde_json::Value],
    panes: &[serde_json::Value],
) -> TabIndex {
    let mut ws_label: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for w in workspaces {
        let (Some(id), Some(label)) = (
            w.get("workspace_id").and_then(|v| v.as_str()),
            w.get("label").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        ws_label.insert(id.to_string(), label.to_string());
    }

    let mut agent_by_tab: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for p in panes {
        let Some(tab_id) = p.get("tab_id").and_then(|v| v.as_str()) else { continue };
        let Some(agent) = p.get("agent").and_then(|v| v.as_str()) else { continue };
        if agent == "usage" {
            continue;
        }
        agent_by_tab.entry(tab_id.to_string()).or_insert_with(|| agent.to_string());
    }

    let mut live = std::collections::HashSet::new();
    let mut ctx = std::collections::HashMap::new();
    for t in tabs {
        let Some(tab_id) = t.get("tab_id").and_then(|v| v.as_str()) else { continue };
        live.insert(tab_id.to_string());
        let Some(ws_id) = t.get("workspace_id").and_then(|v| v.as_str()) else { continue };
        let Some(workspace) = ws_label.get(ws_id).cloned() else { continue };
        ctx.insert(
            tab_id.to_string(),
            state::RowContext { workspace, agent: agent_by_tab.get(tab_id).cloned() },
        );
    }
    TabIndex { live, ctx }
}

/// One `method` round-trip, returning `result.<key>` as a JSON array — `None`
/// on any socket/parse failure (best-effort, never panics; the overlay just
/// shows Unknown context for every row when this fails).
fn fetch_array(method: &str, key: &str) -> Option<Vec<serde_json::Value>> {
    let resp = crate::ipc::call_text(method, serde_json::json!({})).ok()?;
    let value: serde_json::Value = serde_json::from_str(resp.trim_start_matches('\u{feff}')).ok()?;
    value.get("result")?.get(key)?.as_array().cloned()
}

/// Which note THIS pane currently shows — its own tab note, or the shared
/// cross-session global note. Toggled by the overlay's pinned `★ Global` row.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum ActiveNote {
    #[default]
    Tab,
    Global,
}

pub struct App {
    note: Note,
    /// The note split into lines while editing.
    lines: Vec<String>,
    row: usize,
    col: usize,
    edit_scroll: usize,
    preview_scroll: usize,
    /// Ordinal into `markdown::checkbox_lines(&note.text)` — which checkbox
    /// the preview cursor sits on. NOT a source line index: the text can
    /// change under it, so it is re-resolved and re-clamped on every use.
    box_cursor: Option<usize>,
    /// One-shot request to scroll the cursor into view on the next draw.
    /// Only set right after `move_box`/`toggle_box` place or move the cursor
    /// — draw clears it once applied — so a cursor merely existing does not
    /// fight manual scrolling (`Up`/`Dn`/`g`/`G`/PgUp/PgDn) on every frame.
    follow_box: bool,
    confirm_clear: bool,
    dirty: bool,
    last_edit: Instant,
    /// Body height from the last draw, for PgUp/PgDn and scroll clamping.
    body_height: usize,
    pane_id: Option<String>,
    last_beat: Instant,
    /// Disabled in unit tests so exercising keys never touches disk.
    persist: bool,
    /// Some(buf) while editing THIS note's title (opened with `r`).
    title_input: Option<String>,
    /// Some while the `l` list overlay is open.
    overlay: Option<Overlay>,
    /// Which note this pane is currently showing.
    active: ActiveNote,
}

impl App {
    pub fn new() -> Self {
        let mut app = Self::with_note(state::load(), true);
        app.pane_id = std::env::var("HERDR_PANE_ID").ok().filter(|id| !id.is_empty());
        app.report_tokens();
        app
    }

    fn with_note(note: Note, persist: bool) -> Self {
        let mut app = Self {
            note,
            lines: Vec::new(),
            row: 0,
            col: 0,
            edit_scroll: 0,
            preview_scroll: 0,
            box_cursor: None,
            follow_box: false,
            confirm_clear: false,
            dirty: false,
            last_edit: Instant::now(),
            body_height: 20,
            pane_id: None,
            last_beat: Instant::now(),
            persist,
            title_input: None,
            overlay: None,
            active: ActiveNote::default(),
        };
        if app.note.mode == Mode::Edit {
            app.enter_edit(false); // restore-from-disk: never seed
        }
        app
    }

    // ----- persistence & heartbeat -------------------------------------

    /// The file THIS pane currently saves/loads from, given `self.active`.
    /// Pure path resolution (env-derived, no I/O) — safe to call in tests.
    fn current_path(&self) -> Option<std::path::PathBuf> {
        match self.active {
            ActiveNote::Tab => state::state_path(),
            ActiveNote::Global => state::global_path(),
        }
    }

    /// True when `self.note` is the pane's own tab note (not the shared
    /// global note) — so an `is_self` overlay row corresponds to the buffer
    /// currently in memory. Guards the self-mutation on delete/rename: acting
    /// on your own tab-note row while viewing the global note must NOT touch
    /// the global buffer (that path silently deleted global.json).
    fn showing_tab_note(&self) -> bool {
        self.active == ActiveNote::Tab
    }

    /// Takes `&mut self` because it stamps the timestamps back onto the live
    /// note: `persist_at` writes them onto a CLONE, so without this the
    /// header's age is frozen at whatever `load()` read at startup — a note
    /// created this session would never show an age at all, and an older one
    /// would keep ageing while you type into it, disagreeing by hours with
    /// the overlay row for the same note (which re-reads from disk).
    fn save(&mut self) {
        if !self.persist {
            return;
        }
        let Some(path) = self.current_path() else { return };
        let tab_id = match self.active {
            ActiveNote::Tab => state::tab_env().unwrap_or_default(),
            ActiveNote::Global => String::new(),
        };
        let now = state::unix_now();
        state::persist_at(&path, &self.note, &tab_id, now);
        // Mirrors persist_at's own stamping rules exactly (created set once,
        // updated every save). A blank note is DELETED rather than written and
        // still gets stamped here — harmless: nothing reads the timestamps of
        // a note with no file, and the next real save overwrites them anyway.
        if self.note.created == 0 {
            self.note.created = now;
        }
        self.note.updated = now;
    }

    /// Commit + save the current note, then swap the pane to the other one
    /// (tab <-> global). Called by the overlay's pinned `★ Global` row.
    /// The reload is gated on `persist` so unit tests (persist=false) never
    /// touch the real note files — they can still assert the `active` flip
    /// and the resolved path via `current_path`.
    fn toggle_global(&mut self) {
        self.commit();
        self.save();
        self.active = match self.active {
            ActiveNote::Tab => ActiveNote::Global,
            ActiveNote::Global => ActiveNote::Tab,
        };
        if self.persist {
            self.note = match self.active {
                ActiveNote::Tab => state::load(),
                ActiveNote::Global => state::global_path()
                    .map(|p| state::read_note(&p))
                    .unwrap_or_default(),
            };
        }
        self.note.mode = Mode::Preview;
        // Everything per-DOCUMENT resets: a scroll offset and a checkbox
        // ordinal both mean nothing in the other note. Anything added to this
        // struct that describes a position INSIDE the note belongs here too.
        self.preview_scroll = 0;
        self.clear_box_cursor();
        self.dirty = false;
    }

    /// Copy the edit buffer back into the note.
    fn commit(&mut self) {
        if self.note.mode == Mode::Edit {
            self.note.text = self.lines.join("\n");
        }
    }

    /// Debounced autosave: flush ~2s after the last edit keystroke.
    pub fn maybe_flush(&mut self) {
        if self.dirty && self.last_edit.elapsed() >= AUTOSAVE_AFTER {
            self.commit();
            self.save();
            self.dirty = false;
        }
    }

    /// Final save on the way out.
    pub fn finalize(&mut self) {
        self.commit();
        self.save();
    }

    /// Re-stamp the identity token so the launcher knows this pane is alive.
    /// Cheap (one socket round-trip); the event loop calls this every few
    /// seconds. Silently a no-op outside herdr.
    pub fn heartbeat(&mut self) {
        if self.last_beat.elapsed() < HEARTBEAT_EVERY {
            return;
        }
        self.last_beat = Instant::now();
        self.report_tokens();
    }

    fn report_tokens(&self) {
        let Some(pane_id) = &self.pane_id else { return };
        // Token value MUST be a string; numbers are rejected as invalid_request.
        let now = state::unix_now().to_string();
        let _ = crate::ipc::call_text(
            "pane.report_metadata",
            serde_json::json!({
                "pane_id": pane_id,
                "source": METADATA_SOURCE,
                "title": PANE_LABEL,
                "tokens": { METADATA_SOURCE: now },
            }),
        );
    }

    // ----- keys --------------------------------------------------------

    /// Returns true when the app should quit. Esc NEVER quits — it closes the
    /// confirm overlay or leaves edit mode at most.
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        if self.overlay.is_some() {
            self.on_key_overlay(key);
            return false;
        }
        if self.title_input.is_some() {
            self.on_key_title(key);
            return false;
        }
        if self.confirm_clear {
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.note.text.clear();
                self.preview_scroll = 0;
                // A cursor ordinal outlives the text it indexed: benign only
                // while `cursor_line()` returns None on empty text, and no
                // longer benign the moment text comes back (`e` re-seeds).
                self.clear_box_cursor();
                self.save();
            }
            self.confirm_clear = false;
            return false;
        }
        match self.note.mode {
            Mode::Preview => self.on_key_preview(key),
            Mode::Edit => {
                self.on_key_edit(key);
                false
            }
        }
    }

    fn on_key_preview(&mut self, key: KeyEvent) -> bool {
        let page = self.body_height.max(1);
        match key.code {
            KeyCode::Char('q') => {
                self.save();
                return true;
            }
            KeyCode::Char('e') | KeyCode::Enter => self.enter_edit(true),
            KeyCode::Up => self.preview_scroll = self.preview_scroll.saturating_sub(1),
            KeyCode::Down => self.preview_scroll = self.preview_scroll.saturating_add(1),
            KeyCode::Char('j') => self.move_box(1),
            KeyCode::Char('k') => self.move_box(-1),
            KeyCode::Char(' ') => self.toggle_box(),
            // The only way out of the checkbox cursor. Without it the
            // highlight is a mode you can enter and not leave — the other
            // exits are all side effects (swap documents, `x` clear, edit the
            // last box away). Esc is otherwise unbound here and still must
            // never quit the TUI.
            KeyCode::Esc => self.clear_box_cursor(),
            KeyCode::PageUp => self.preview_scroll = self.preview_scroll.saturating_sub(page),
            KeyCode::PageDown => self.preview_scroll = self.preview_scroll.saturating_add(page),
            // g/G because herdr `pane send-keys` rejects Home/End.
            KeyCode::Home | KeyCode::Char('g') => self.preview_scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.preview_scroll = usize::MAX, // clamped in draw
            KeyCode::Char('x') => self.confirm_clear = true,
            KeyCode::Char('r') => self.title_input = Some(self.note.title.clone()),
            KeyCode::Char('l') => self.open_overlay(),
            _ => {}
        }
        false
    }

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

    fn open_overlay(&mut self) {
        let Some(dir) = state::store_dir() else { return };
        let index = tab_contexts();
        let live = index.as_ref().map(|i| &i.live);
        let self_tab = state::tab_env().unwrap_or_default();
        let global = state::global_path();
        let mut entries: Vec<OverlayEntry> = state::list_notes(&dir)
            .into_iter()
            .filter(|s| Some(&s.file) != global.as_ref())
            .map(|s| {
                let status = state::classify_tab(&s.tab_id, live);
                let context = state::format_context(status, index.as_ref().and_then(|i| i.ctx.get(&s.tab_id)));
                OverlayEntry {
                    status,
                    is_self: !self_tab.is_empty() && s.tab_id == self_tab,
                    is_global: false,
                    context,
                    tab_id: s.tab_id,
                    file: s.file,
                    title: s.title,
                    updated: s.updated,
                    text: s.text, // carried by list_notes — no second read
                }
            })
            .collect();
        entries.sort_by_key(|e| (state::sort_rank(e.status), std::cmp::Reverse(e.updated)));
        if let Some(path) = global {
            let note = state::read_note(&path);
            let label = if self.active == ActiveNote::Global {
                "◂ Back to this tab's note".to_string()
            } else {
                "★ Global note".to_string()
            };
            entries.insert(0, OverlayEntry {
                file: path,
                title: label,
                tab_id: String::new(),
                updated: note.updated,
                status: state::TabStatus::Unknown,
                text: note.text,
                is_self: false,
                is_global: true,
                context: "global".to_string(),
            });
        }
        self.overlay = Some(Overlay::from_entries(entries));
    }

    fn on_key_overlay(&mut self, key: KeyEvent) {
        let Some(mut ov) = self.overlay.take() else { return };
        if self.handle_overlay(&mut ov, key) {
            self.overlay = Some(ov);
        }
    }

    /// Returns false when the overlay should close.
    fn handle_overlay(&mut self, ov: &mut Overlay, key: KeyEvent) -> bool {
        let last = ov.visible.len().saturating_sub(1);
        match &mut ov.mode {
            OverlayMode::List => match key.code {
                KeyCode::Esc | KeyCode::Char('l') => return false,
                KeyCode::Up | KeyCode::Char('k') => ov.selected = ov.selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => ov.selected = (ov.selected + 1).min(last),
                KeyCode::Enter => {
                    if ov.selected_entry().is_some_and(|e| e.is_global) {
                        self.toggle_global();
                        return false;
                    }
                    if !ov.visible.is_empty() {
                        ov.mode = OverlayMode::Preview { scroll: 0 };
                    }
                }
                KeyCode::Char('r') => {
                    if let Some(e) = ov.selected_entry()
                        && !e.is_global
                    {
                        ov.mode = OverlayMode::Rename(e.title.clone());
                    }
                }
                KeyCode::Char('d')
                    if !ov.visible.is_empty()
                        && !ov.selected_entry().is_some_and(|e| e.is_global) =>
                {
                    ov.mode = OverlayMode::ConfirmDelete;
                }
                KeyCode::Char('g') => {
                    if let Some(e) = ov.selected_entry()
                        && !e.is_global
                        && e.status == state::TabStatus::Live
                        && !e.tab_id.is_empty()
                    {
                        if self.persist {
                            let _ = crate::ipc::call_text("tab.focus", serde_json::json!({ "tab_id": e.tab_id }));
                        }
                        return false;
                    }
                }
                KeyCode::Char('/') => ov.mode = OverlayMode::Filter,
                _ => {}
            },
            OverlayMode::Filter => match key.code {
                KeyCode::Enter => ov.mode = OverlayMode::List,
                KeyCode::Esc => {
                    ov.filter.clear();
                    ov.recompute_visible();
                    ov.mode = OverlayMode::List;
                }
                KeyCode::Backspace => {
                    ov.filter.pop();
                    ov.recompute_visible();
                }
                KeyCode::Char(c) => {
                    ov.filter.push(c);
                    ov.recompute_visible();
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
                    if let Some((_, e)) = ov.selected_entry_mut() {
                        if self.persist {
                            state::set_title(&e.file, &title);
                        }
                        e.title = title.clone();
                        e.updated = state::unix_now();
                        if e.is_self && self.showing_tab_note() {
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
                    if let Some((idx, e)) = ov.selected_entry_mut() {
                        if self.persist {
                            let _ = std::fs::remove_file(&e.file);
                        }
                        if e.is_self && self.showing_tab_note() {
                            self.note.text.clear();
                            self.note.title.clear();
                            self.clear_box_cursor();
                        }
                        ov.entries.remove(idx);
                    }
                    ov.recompute_visible();
                }
                ov.mode = OverlayMode::List;
            }
        }
        true
    }

    /// Source line of the selected checkbox, re-resolved against the current
    /// text so a stale ordinal can never point at the wrong line.
    fn cursor_line(&self) -> Option<usize> {
        let boxes = markdown::checkbox_lines(&self.note.text);
        boxes.get(self.box_cursor?).map(|(line, _)| *line)
    }

    /// Re-clamps the cursor ordinal against the checkboxes the note has NOW —
    /// an edit may have deleted some — and drops it when there are none left.
    /// The single home of that rule: `move_box` and `leave_edit` both used to
    /// spell it out, and `leave_edit`'s `n - 1` was panic-safe only because
    /// its `(_, 0)` arm happened to come first.
    fn clamp_box_cursor(&mut self) {
        let n = markdown::checkbox_lines(&self.note.text).len();
        self.box_cursor = match self.box_cursor {
            Some(c) if n > 0 => Some(c.min(n - 1)),
            _ => None,
        };
    }

    /// Drops the checkbox cursor and any pending scroll-follow. Both are
    /// per-DOCUMENT state, so every path that swaps or wipes the buffer
    /// (`toggle_global`, `x` clear, overlay self-delete) must call this.
    fn clear_box_cursor(&mut self) {
        self.box_cursor = None;
        self.follow_box = false;
    }

    /// Steps the checkbox cursor. From no cursor, `j` lands on the first box
    /// and `k` on the last. Clamps at both ends; clears when the note has no
    /// checkboxes left.
    fn move_box(&mut self, delta: isize) {
        self.clamp_box_cursor();
        let n = markdown::checkbox_lines(&self.note.text).len();
        if n == 0 {
            return; // clamp already dropped the cursor
        }
        self.box_cursor = Some(match self.box_cursor {
            None if delta > 0 => 0,
            None => n - 1,
            Some(c) => c.saturating_add_signed(delta).min(n - 1),
        });
        self.follow_box = true;
    }

    /// Flips the selected checkbox straight in `note.text`. Preview mode never
    /// touches `lines`, so `commit` has nothing to overwrite this with — the
    /// existing debounce persists it.
    fn toggle_box(&mut self) {
        let Some(line) = self.cursor_line() else { return };
        let Some(text) = markdown::toggle_checkbox(&self.note.text, line) else { return };
        self.note.text = text;
        self.touch();
        self.follow_box = true;
    }

    /// `seed` gates the template: only the INTERACTIVE path (`e`/Enter) may
    /// seed an empty note. Restoring a persisted `mode: "edit"` must not —
    /// `herdr pane close` kills the pane with no signal, so a titled, bodyless
    /// note autosaved mid-edit comes back as Edit, and an unconditional seed
    /// would hand it a body the user never typed and autosave it 2s later.
    fn enter_edit(&mut self, seed: bool) {
        // Lazy seed: a tab you merely toggled Notes into and never edited
        // still writes no file. `dirty` so the seed survives to the next
        // autosave; `is_blank` deletes it again if it stays untouched.
        let seeded = seed && self.note.text.trim().is_empty();
        if seeded {
            self.note.text = template::DEFAULT.to_string();
            self.dirty = true;
            self.last_edit = Instant::now();
        }
        self.lines = self.note.text.split('\n').map(String::from).collect();
        self.row = if seeded {
            // Line 1: the blank line under `## Status`, so the first
            // keystroke IS the status. The template ships no placeholder to
            // replace — edit mode has no line-kill or selection to remove one.
            1.min(self.lines.len().saturating_sub(1))
        } else {
            0
        };
        self.col = 0;
        self.edit_scroll = 0;
        self.note.mode = Mode::Edit;
    }

    fn leave_edit(&mut self) {
        self.commit();
        self.note.mode = Mode::Preview;
        self.dirty = false;
        // The edit may have deleted the box the cursor pointed at.
        self.clamp_box_cursor();
        self.save();
    }

    fn touch(&mut self) {
        self.dirty = true;
        self.last_edit = Instant::now();
    }

    fn on_key_edit(&mut self, key: KeyEvent) {
        // AltGr arrives from Windows as CONTROL|ALT on a plain character
        // (@ { [ ] } \ on German/French/Nordic layouts) — that is text to
        // insert, not a Ctrl shortcut.
        let altgr = key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::ALT);
        if key.modifiers.contains(KeyModifiers::CONTROL) && !altgr {
            if matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S')) {
                self.commit();
                self.save();
                self.dirty = false;
            }
            return;
        }
        let line_len = clen(&self.lines[self.row]);
        match key.code {
            KeyCode::Esc => self.leave_edit(),
            KeyCode::Left => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = clen(&self.lines[self.row]);
                }
            }
            KeyCode::Right => {
                if self.col < line_len {
                    self.col += 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
            }
            KeyCode::Up => {
                if self.row > 0 {
                    self.row -= 1;
                    self.col = self.col.min(clen(&self.lines[self.row]));
                }
            }
            KeyCode::Down => {
                if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = self.col.min(clen(&self.lines[self.row]));
                }
            }
            KeyCode::Home => self.col = 0,
            KeyCode::End => self.col = line_len,
            KeyCode::PageUp => {
                self.row = self.row.saturating_sub(self.body_height.max(1));
                self.col = self.col.min(clen(&self.lines[self.row]));
            }
            KeyCode::PageDown => {
                self.row = (self.row + self.body_height.max(1)).min(self.lines.len() - 1);
                self.col = self.col.min(clen(&self.lines[self.row]));
            }
            KeyCode::Enter => {
                let at = byte_idx(&self.lines[self.row], self.col);
                let rest = self.lines[self.row].split_off(at);
                self.lines.insert(self.row + 1, rest);
                self.row += 1;
                self.col = 0;
                self.touch();
            }
            KeyCode::Backspace => {
                if self.col > 0 {
                    let at = byte_idx(&self.lines[self.row], self.col - 1);
                    self.lines[self.row].remove(at);
                    self.col -= 1;
                    self.touch();
                } else if self.row > 0 {
                    let tail = self.lines.remove(self.row);
                    self.row -= 1;
                    self.col = clen(&self.lines[self.row]);
                    self.lines[self.row].push_str(&tail);
                    self.touch();
                }
            }
            KeyCode::Delete => {
                if self.col < line_len {
                    let at = byte_idx(&self.lines[self.row], self.col);
                    self.lines[self.row].remove(at);
                    self.touch();
                } else if self.row + 1 < self.lines.len() {
                    let tail = self.lines.remove(self.row + 1);
                    self.lines[self.row].push_str(&tail);
                    self.touch();
                }
            }
            KeyCode::Tab => {
                let at = byte_idx(&self.lines[self.row], self.col);
                self.lines[self.row].insert_str(at, "  ");
                self.col += 2;
                self.touch();
            }
            KeyCode::Char(c) => {
                let at = byte_idx(&self.lines[self.row], self.col);
                self.lines[self.row].insert(at, c);
                self.col += 1;
                self.touch();
            }
            _ => {}
        }
    }

    // ----- drawing -----------------------------------------------------

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let [title_a, body_a, hint_a] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
                .areas(area);
        self.body_height = usize::from(body_a.height);

        // Body first: the preview reports a scroll hint for the title line.
        let (mode, scroll_hint) = match self.note.mode {
            Mode::Preview => ("preview", self.draw_preview(frame, body_a)),
            Mode::Edit => {
                self.draw_edit(frame, body_a);
                ("edit", None)
            }
        };

        // The pane border already says "Notes" (metadata title) — repeating it
        // here read as a duplicate, so the header carries only the note's own
        // title (or the live title editor) plus mode + scroll.
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
            title.push(Span::styled(
                format!(" [{mode}]"),
                Style::default().fg(Color::Cyan),
            ));
            if self.active == ActiveNote::Global {
                title.push(Span::raw(" —"));
                title.push(Span::styled(
                    " ★ Global",
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ));
            } else if !self.note.title.trim().is_empty() {
                title.push(Span::raw(" —"));
                title.push(Span::styled(
                    format!(" {}", self.note.title),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
            }
            if let Some(hint) = scroll_hint {
                title.push(Span::styled(
                    format!("  {hint}"),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            // Last, and ALL-OR-NOTHING: the header is a 1-row Paragraph with
            // no wrap, so relying on the terminal to clip from the right
            // renders a meaningless fragment ("2h ag", or a bare "2" that
            // reads as a count). Measure what is already assembled in display
            // COLUMNS (the title can hold CJK) and drop the whole token when
            // it does not fit — same "pick a variant that fits" rule as the
            // footer hints below.
            if self.note.updated > 0 {
                let age = state::format_age(state::unix_now().saturating_sub(self.note.updated));
                let token = format!("  {age} ago");
                let used: usize = title.iter().map(|s| dwidth(&s.content)).sum();
                if used + dwidth(&token) <= usize::from(title_a.width) {
                    title.push(Span::styled(token, Style::default().add_modifier(Modifier::DIM)));
                }
            }
        }
        frame.render_widget(Paragraph::new(Line::from(title)), title_a);

        // The full hint line no longer fits a narrow right dock, and it ends
        // in `q quit` — exactly what clipping would eat. Each form is picked
        // by width so `q quit` survives down to the shortest one's own length
        // (37 columns bare, 39 with the cursor hint); below that it clips.
        // `esc drop` appears only while a checkbox cursor is live: it is the
        // only exit from that cursor, and advertising it unconditionally
        // would spend scarce columns on a key that does nothing. It costs
        // `l list` its place in the narrow cursor form — while you are stuck
        // in a cursor, the way out beats the way to the dashboard.
        const PREVIEW_HINTS: &str =
            " e edit  j/k spc tick  r title  l list  Up/Dn scroll  x clear  q quit";
        const PREVIEW_HINTS_SHORT: &str = " e edit  j/k spc tick  l list  q quit";
        const PREVIEW_HINTS_CURSOR: &str =
            " e edit  j/k spc tick  esc drop  r title  l list  Up/Dn scroll  x clear  q quit";
        const PREVIEW_HINTS_CURSOR_SHORT: &str = " e edit  j/k spc tick  esc drop  q quit";
        let hints = match self.note.mode {
            Mode::Preview => {
                let (full, short) = if self.box_cursor.is_some() {
                    (PREVIEW_HINTS_CURSOR, PREVIEW_HINTS_CURSOR_SHORT)
                } else {
                    (PREVIEW_HINTS, PREVIEW_HINTS_SHORT)
                };
                if usize::from(hint_a.width) >= full.chars().count() { full } else { short }
            }
            Mode::Edit => " Esc preview (saves)   Ctrl+S save",
        };
        frame.render_widget(
            Paragraph::new(Span::styled(hints, Style::default().add_modifier(Modifier::DIM))),
            hint_a,
        );

        if self.confirm_clear {
            draw_confirm(frame, area);
        }
        if let Some(ov) = &mut self.overlay {
            draw_overlay(frame, area, ov);
        }
    }

    /// Renders the preview body; returns a "top-line/total" scroll hint when
    /// the content overflows the pane.
    fn draw_preview(&mut self, frame: &mut Frame, area: Rect) -> Option<String> {
        if self.note.text.trim().is_empty() {
            self.preview_scroll = 0;
            frame.render_widget(
                Paragraph::new(empty_help()).style(Style::default().add_modifier(Modifier::DIM)),
                area,
            );
            return None;
        }
        // The rightmost column is reserved for the overflow scrollbar so text
        // never sits underneath it.
        let text_w = usize::from(area.width).saturating_sub(1).max(1);
        let (mut lines, map) = render_markdown_mapped(&self.note.text, text_w);
        let total = lines.len();
        let max = total.saturating_sub(usize::from(area.height));
        if let Some(src) = self.cursor_line() {
            // Highlight EVERY row of the selected item — a wrapped checkbox
            // spans several and would otherwise look half-selected. This runs
            // unconditionally whenever a cursor exists; only the scrolling
            // below is one-shot.
            for (i, line) in lines.iter_mut().enumerate() {
                if map.get(i).copied().flatten() == Some(src) {
                    line.style = line.style.add_modifier(Modifier::REVERSED);
                }
            }
            // Scroll follows the cursor, mirroring the overlay's list clamp —
            // but only on a fresh move/toggle (`follow_box`), not on every
            // draw. Otherwise a cursor merely existing would fight manual
            // scrolling (Up/Dn/g/G/PgUp/PgDn) on the very next frame.
            if self.follow_box {
                if let Some(first) = map.iter().position(|m| *m == Some(src)) {
                    let h = usize::from(area.height).max(1);
                    if first < self.preview_scroll {
                        self.preview_scroll = first;
                    } else if first >= self.preview_scroll + h {
                        self.preview_scroll = first + 1 - h;
                    }
                }
                self.follow_box = false;
            }
        }
        self.preview_scroll = clamp_scroll(self.preview_scroll, total, usize::from(area.height));
        let scroll = u16::try_from(self.preview_scroll).unwrap_or(u16::MAX);
        frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), area);
        draw_scrollbar(frame, area, max, self.preview_scroll);
        (max > 0).then(|| format!("{}/{total}", self.preview_scroll + 1))
    }

    fn draw_edit(&mut self, frame: &mut Frame, area: Rect) {
        let h = usize::from(area.height).max(1);
        // Rightmost column reserved for the overflow scrollbar.
        let w = usize::from(area.width).saturating_sub(1).max(2);
        if self.row < self.edit_scroll {
            self.edit_scroll = self.row;
        }
        if self.row >= self.edit_scroll + h {
            self.edit_scroll = self.row + 1 - h;
        }
        // Horizontal shift keeps the cursor visible on overlong lines. The
        // shift is found in display COLUMNS (wide CJK/emoji chars count 2),
        // otherwise the REVERSED cursor cell could sit past the pane edge.
        let widths: Vec<usize> =
            self.lines[self.row].chars().map(|c| c.width().unwrap_or(0)).collect();
        let cursor_w = widths.get(self.col).copied().unwrap_or(1).max(1);
        let mut h_off = 0;
        let mut visible: usize = widths[..self.col].iter().sum();
        while visible + cursor_w > w && h_off < self.col {
            visible -= widths[h_off];
            h_off += 1;
        }
        let mut lines: Vec<Line> = Vec::new();
        for (i, line) in self.lines.iter().enumerate().skip(self.edit_scroll).take(h) {
            let chars: Vec<char> = line.chars().skip(h_off).collect();
            if i == self.row {
                let col = self.col - h_off;
                let before: String = chars.iter().take(col).collect();
                let at: String = chars.get(col).map_or(" ".to_string(), |c| c.to_string());
                let after: String = chars.iter().skip(col + 1).collect();
                lines.push(Line::from(vec![
                    Span::raw(before),
                    Span::styled(at, Style::default().add_modifier(Modifier::REVERSED)),
                    Span::raw(after),
                ]));
            } else {
                lines.push(Line::from(chars.into_iter().collect::<String>()));
            }
        }
        frame.render_widget(Paragraph::new(lines), area);
        draw_scrollbar(frame, area, self.lines.len().saturating_sub(h), self.edit_scroll);
    }
}

/// Vertical scrollbar on the right edge; hidden when everything fits.
fn draw_scrollbar(frame: &mut Frame, area: Rect, max_scroll: usize, position: usize) {
    if max_scroll == 0 {
        return;
    }
    let mut state = ScrollbarState::new(max_scroll).position(position.min(max_scroll));
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight),
        area,
        &mut state,
    );
}

fn draw_confirm(frame: &mut Frame, area: Rect) {
    let w = 30.min(area.width);
    let h = 3.min(area.height);
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(" Clear the note? y/N").block(Block::bordered().title(" Clear ")),
        rect,
    );
}

fn draw_overlay(frame: &mut Frame, area: Rect, ov: &mut Overlay) {
    // Centering below relies on w <= area.width and h <= area.height — on a
    // very narrow/short pane the clamp(20, 60) / .max(3) floors can otherwise
    // exceed the area and underflow the saturating_sub centering math (a
    // panic on `.saturating_sub` never happens, but plain subtraction would).
    let w = area.width.saturating_sub(4).clamp(20, 60).min(area.width);
    // Preview wants a tall box to read the note; the list sizes to its rows;
    // the one-line rename/confirm prompts stay short.
    let h = match &ov.mode {
        OverlayMode::Preview { .. } => area.height.saturating_sub(2).max(3).min(area.height),
        OverlayMode::List | OverlayMode::Filter => {
            let content_h = u16::try_from(ov.visible.len() + 2).unwrap_or(u16::MAX);
            area.height.saturating_sub(2).min(content_h).max(3).min(area.height)
        }
        OverlayMode::Rename(_) | OverlayMode::ConfirmDelete => 3.min(area.height).max(1),
    };
    let rect = Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    };

    // Pre-render clamps that mutate `ov` (kept out of the read-only render
    // match below, which borrows `ov` immutably). `inner_rows` is the text
    // height inside the box's top/bottom borders.
    let inner_rows = usize::from(rect.height).saturating_sub(2);
    if matches!(ov.mode, OverlayMode::List | OverlayMode::Filter) {
        ov.list_scroll = list_window(ov.list_scroll, ov.selected, ov.visible.len(), inner_rows);
    } else if matches!(ov.mode, OverlayMode::Preview { .. }) {
        let text_w = usize::from(rect.width).saturating_sub(2).max(1);
        let total = ov.selected_entry().map_or(0, |e| render_markdown(&e.text, text_w).len());
        if let OverlayMode::Preview { scroll } = &mut ov.mode {
            *scroll = clamp_scroll(*scroll, total, inner_rows);
        }
    }

    frame.render_widget(Clear, rect);
    match &ov.mode {
        OverlayMode::Preview { scroll } => {
            let text = ov.selected_entry().map(|e| e.text.as_str()).unwrap_or("");
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
            let name = ov.selected_entry()
                .map(|e| if e.title.trim().is_empty() { "(untitled)".to_string() } else { e.title.clone() })
                .unwrap_or_default();
            frame.render_widget(
                Paragraph::new(format!(" Delete \"{name}\"? y/N"))
                    .block(Block::bordered().title(" Delete ")),
                rect,
            );
        }
        OverlayMode::List | OverlayMode::Filter => {
            let now = state::unix_now();
            let mut lines: Vec<Line> = Vec::new();
            let inner_width = usize::from(rect.width).saturating_sub(2);
            // Only the viewport window is rendered; `list_scroll` was set above
            // to keep `selected` on screen. `i` stays the visible-index so the
            // selection marker/style lines up.
            for (i, &idx) in ov.visible.iter().enumerate().skip(ov.list_scroll).take(inner_rows) {
                let e = &ov.entries[idx];
                let marker = if i == ov.selected { ">" } else { " " };
                let self_mark = if e.is_self { "*" } else { " " };
                let name = if e.title.trim().is_empty() { "(untitled)" } else { &e.title };
                let age = if e.updated == 0 { "—".to_string() } else { state::format_age(now.saturating_sub(e.updated)) };
                // Counted per draw off the row's own text (only visible rows
                // are drawn) rather than cached, so an in-overlay rename or
                // delete can never leave a stale count behind.
                let (done, total) = markdown::checkbox_counts(&e.text);
                let progress = if total > 0 { format!("  {done}/{total}") } else { String::new() };
                // Fit the right column to the columns it may actually have,
                // dropping whole tokens (progress, then context) — otherwise
                // it eats the title at real dock widths and can itself end
                // mid-token.
                let right = fit_right(
                    &e.context,
                    &progress,
                    &age,
                    right_budget(marker, self_mark, name, inner_width),
                );
                let text = format_row(marker, self_mark, name, &right, inner_width);
                let base = if e.is_global {
                    Color::Cyan
                } else {
                    match e.status {
                        state::TabStatus::Live => Color::Green,
                        state::TabStatus::Closed | state::TabStatus::Unknown => Color::DarkGray,
                    }
                };
                let mut style = Style::default().fg(base);
                if i == ov.selected {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                lines.push(Line::styled(text, style));
            }
            // Gate on the actual row set, not `lines` — a box too short to hold
            // any windowed row (inner_rows == 0) leaves `lines` empty without
            // the list being empty, and must not claim "(no notes)".
            if ov.visible.is_empty() {
                lines.push(Line::from("(no notes)"));
            }
            let title_top = if matches!(ov.mode, OverlayMode::Filter) {
                format!(" All notes   filter: {}_ ", ov.filter)
            } else if ov.filter.is_empty() {
                " All notes   ↑↓ move  enter preview ".to_string()
            } else {
                format!(" All notes   filter: {} ", ov.filter)
            };
            frame.render_widget(
                Paragraph::new(lines).block(
                    Block::bordered()
                        .title(title_top)
                        .title_bottom(" r rename  d delete  g goto  / filter  esc "),
                ),
                rect,
            );
        }
    }
}

fn clen(s: &str) -> usize {
    s.chars().count()
}

/// Clamp a scroll offset so it can't run past the last screenful: the deepest
/// useful offset is `total - viewport` (0 when everything fits). Shared by the
/// main-note preview and the overlay's per-note preview so neither can scroll
/// into blank space below the content.
fn clamp_scroll(scroll: usize, total: usize, viewport: usize) -> usize {
    scroll.min(total.saturating_sub(viewport))
}

/// New viewport offset for a list of `len` rows showing `rows` at once: keeps
/// `selected` visible while moving the window as little as possible from
/// `prev`. Returns 0 when everything fits (`len <= rows`) or `rows == 0`.
/// Never scrolls past the end (no trailing blank rows below the last item).
fn list_window(prev: usize, selected: usize, len: usize, rows: usize) -> usize {
    if rows == 0 || len <= rows {
        return 0;
    }
    let max_off = len - rows;
    let mut off = prev.min(max_off);
    if selected < off {
        off = selected;
    } else if selected >= off + rows {
        off = selected + 1 - rows;
    }
    off.min(max_off)
}

fn byte_idx(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(b, _)| b)
}

/// Display-column width of a string (unicode-width, not char count — CJK and
/// emoji are double-width).
fn dwidth(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Truncates `s` to at most `max` display columns without splitting a wide
/// char in half.
fn truncate_w(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

/// One overlay row, padded to exactly `inner_width` display columns: a
/// 1-space margin on each side, `{marker}{self_mark}{name}` on the left (name
/// truncated to fit, but never below `NAME_MIN`), `right` (context, progress
/// and age — already fitted by `fit_right`) pinned to the right edge, the
/// gap between padded with spaces. An over-long `right` is still truncated
/// here as a safety net. Fits exactly within `inner_width` for any
/// input wide enough to hold the margins plus the `marker`+`self_mark`
/// prefix; can only exceed `inner_width` when `inner_width` is too small to
/// hold even that (a handful of columns) — never panics.
/// Display columns the NAME keeps before the right-hand segment may claim any
/// budget. The dashboard's job is to say which note is which, so the title
/// never collapses: without a floor a 40-column right dock (inner width 34)
/// left it ONE column once context + progress + age were in. Eight is the
/// shortest stem that still tells titles apart ("Release…"), and it is small
/// enough that the same 34-column row still fits `workspace · agent` + age
/// alongside it — a bigger floor pushes the session context off entirely.
const NAME_MIN: usize = 8;

/// Columns `format_row` will grant the right-hand segment, after the two
/// margins, the marker prefix, a 1-space gap and the name's floor. Exposed so
/// the caller can choose WHICH right-hand tokens survive (see `fit_right`)
/// instead of letting `format_row` truncate one mid-token.
fn right_budget(marker: &str, self_mark: &str, name: &str, inner_width: usize) -> usize {
    let prefix_w = dwidth(marker) + dwidth(self_mark);
    inner_width
        .saturating_sub(2) // the row's 1-space margins
        .saturating_sub(prefix_w + 1) // marker/self mark + the gap before `right`
        .saturating_sub(NAME_MIN.min(dwidth(name))) // the name's floor (never more than it needs)
}

/// The row's right-hand segment degraded to fit `budget`, in WHOLE tokens:
/// context + progress + age, then without the progress count, then the age
/// alone, then nothing. Truncating instead would leave a fragment that reads
/// as a different value — `acme-app · claude  2` looks like a count or an age.
fn fit_right(context: &str, progress: &str, age: &str, budget: usize) -> String {
    [
        format!("{context}{progress}  {age}"),
        format!("{context}  {age}"),
        age.to_string(),
    ]
    .into_iter()
    .map(|s| s.trim_start().to_string())
    .find(|s| dwidth(s) <= budget)
    .unwrap_or_default()
}

fn format_row(marker: &str, self_mark: &str, name: &str, right: &str, inner_width: usize) -> String {
    let budget = inner_width.saturating_sub(2);
    let prefix_w = dwidth(marker) + dwidth(self_mark);
    // The name's floor comes out of the budget BEFORE the right segment gets
    // any of it (`right_budget`), which also reserves the prefix and the
    // minimum gap — so left + gap + right can never exceed the budget.
    let right = truncate_w(right, right_budget(marker, self_mark, name, inner_width));
    let right_w = dwidth(&right);
    let gap_min = usize::from(right_w > 0);
    let name_budget = budget.saturating_sub(prefix_w + right_w + gap_min);
    let name = truncate_w(name, name_budget);
    let left = format!("{marker}{self_mark}{name}");
    let left_w = dwidth(&left);
    let gap = budget.saturating_sub(left_w + right_w);
    format!(" {left}{}{right} ", " ".repeat(gap))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the env-mutating tests (they set process-global HERDR_* vars).
    /// No non-env test reads these, so this only guards env tests against each
    /// other.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Renders `app` once into a fixed-size TestBackend and returns the screen
    /// as text (row-major, newline per row) for substring assertions.
    fn rendered(app: &mut App, w: u16, h: u16) -> String {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..h {
            for x in 0..w {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    fn app(text: &str) -> App {
        App::with_note(
            Note { text: text.to_string(), mode: Mode::Preview, ..Default::default() },
            false, // never touch the real state file from tests
        )
    }

    #[test]
    fn header_shows_the_note_age() {
        let mut a = app("body");
        a.note.updated = state::unix_now().saturating_sub(2 * 60 * 60);
        let screen = rendered(&mut a, 60, 8);
        assert!(screen.contains("2h ago"), "{screen}");
    }

    #[test]
    fn header_omits_age_for_a_note_with_no_timestamp() {
        let mut a = app("body");
        a.note.updated = 0; // a v1 file, before `updated` existed
        let screen = rendered(&mut a, 60, 8);
        assert!(!screen.contains("ago"), "{screen}");
    }

    #[test]
    fn edit_roundtrip_insert_newline_backspace() {
        let mut a = app("ab");
        a.on_key(key(KeyCode::Enter)); // enter edit mode
        assert_eq!(a.note.mode, Mode::Edit);
        a.on_key(key(KeyCode::End));
        a.on_key(key(KeyCode::Char('c')));
        a.on_key(key(KeyCode::Enter)); // newline
        a.on_key(key(KeyCode::Char('d')));
        a.on_key(key(KeyCode::Esc)); // back to preview, committing
        assert_eq!(a.note.mode, Mode::Preview);
        assert_eq!(a.note.text, "abc\nd");

        a.on_key(key(KeyCode::Char('e')));
        a.on_key(key(KeyCode::Down));
        a.on_key(key(KeyCode::End));
        a.on_key(key(KeyCode::Backspace)); // delete 'd'
        a.on_key(key(KeyCode::Backspace)); // join lines
        a.on_key(key(KeyCode::Esc));
        assert_eq!(a.note.text, "abc");
    }

    #[test]
    fn first_edit_seeds_the_template() {
        let mut a = app("");
        a.on_key(key(KeyCode::Char('e')));
        assert_eq!(a.note.text, crate::template::DEFAULT);
        // The Status section ships EMPTY: the cursor lands on the blank line
        // directly under `## Status`, so the first keystroke IS the status.
        // (A placeholder there would cost End + 29 Backspaces to remove —
        // edit mode has no line-kill, word-delete or selection.)
        assert_eq!(a.row, 1, "cursor on the blank line under ## Status");
        assert_eq!(a.lines[0], "## Status");
        assert_eq!(a.lines[a.row], "", "nothing to delete before typing");
        assert_eq!(a.col, 0);
        assert!(a.dirty, "the seed must reach disk on the next flush");
    }

    #[test]
    fn restoring_edit_mode_from_disk_does_not_seed_the_template() {
        // `herdr pane close` kills the pane with no signal, so a note
        // autosaved while editing keeps `mode: "edit"` on disk. Re-opening it
        // must NOT seed a body the user never typed (and never asked to save).
        let a = App::with_note(
            Note { text: String::new(), title: "Titled".into(), mode: Mode::Edit, ..Default::default() },
            false,
        );
        assert_eq!(a.note.text, "", "restore must not seed the template");
        assert!(!a.dirty, "and must not queue an autosave of a body nobody typed");
    }

    #[test]
    fn edit_does_not_seed_over_existing_text() {
        let mut a = app("already written");
        a.on_key(key(KeyCode::Char('e')));
        assert_eq!(a.note.text, "already written");
    }

    #[test]
    fn entering_edit_on_existing_text_opens_at_the_top() {
        // Landing on the template's blank status line belongs to the seed path
        // only — a real note must still open at row 0, whatever it contains.
        let mut a = app("first line\n<html>\nthird");
        a.on_key(key(KeyCode::Char('e')));
        assert_eq!(a.row, 0);
        assert_eq!(a.col, 0);
    }

    #[test]
    fn empty_preview_shows_the_template_skeleton() {
        let mut a = app("");
        let screen = rendered(&mut a, 60, 24);
        assert!(screen.contains("## Status"), "{screen}");
        assert!(screen.contains("## Next"), "{screen}");
        assert!(!screen.contains("one line: where this stands"), "no placeholder inside the template: {screen}");
        assert!(
            screen.contains("one line on where this stands"),
            "the help copy still says what the empty Status section is for: {screen}"
        );
    }

    #[test]
    fn esc_never_quits_and_q_quits_only_in_preview() {
        let mut a = app("x");
        assert!(!a.on_key(key(KeyCode::Esc)), "Esc in preview must not quit");
        a.on_key(key(KeyCode::Char('e')));
        assert!(!a.on_key(key(KeyCode::Esc)), "Esc in edit leaves edit, not the app");
        assert_eq!(a.note.mode, Mode::Preview);
        // 'q' typed while editing is just a character.
        a.on_key(key(KeyCode::Char('e')));
        assert!(!a.on_key(key(KeyCode::Char('q'))));
        a.on_key(key(KeyCode::Esc));
        assert_eq!(a.note.text, "qx");
        assert!(a.on_key(key(KeyCode::Char('q'))), "q in preview quits");
    }

    #[test]
    fn clear_requires_confirmation() {
        let mut a = app("keep me");
        a.on_key(key(KeyCode::Char('x')));
        assert!(a.confirm_clear);
        assert!(!a.on_key(key(KeyCode::Esc)), "Esc closes the overlay, not the app");
        assert_eq!(a.note.text, "keep me");
        a.on_key(key(KeyCode::Char('x')));
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.note.text, "keep me", "declined clear keeps the note");
        assert!(!a.confirm_clear);
        a.on_key(key(KeyCode::Char('x')));
        a.on_key(key(KeyCode::Char('y')));
        assert_eq!(a.note.text, "", "confirmed clear empties the note");
    }

    #[test]
    fn preview_scroll_keys_move_and_clamp_at_top() {
        let mut a = app("line\nline\nline");
        a.on_key(key(KeyCode::Up));
        assert_eq!(a.preview_scroll, 0, "scrolling above the top clamps");
        a.on_key(key(KeyCode::Down));
        a.on_key(key(KeyCode::Down));
        assert_eq!(a.preview_scroll, 2);
        a.on_key(key(KeyCode::Char('g')));
        assert_eq!(a.preview_scroll, 0);
        a.on_key(key(KeyCode::Char('G')));
        assert_eq!(a.preview_scroll, usize::MAX, "jump to end; draw clamps to content");
    }

    #[test]
    fn altgr_chars_insert_but_ctrl_shortcuts_do_not() {
        let mut a = app("");
        a.on_key(key(KeyCode::Char('e')));
        // AltGr = CONTROL|ALT on Windows: a printable char, must insert.
        a.on_key(KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        // Plain Ctrl+char stays a shortcut, never inserts.
        a.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        a.on_key(key(KeyCode::Esc));
        // Entering edit on an empty note seeds the template and lands the
        // cursor on the empty line under `## Status`, so the inserted char
        // becomes the status. Built from DEFAULT so it can't drift from it.
        let expected = crate::template::DEFAULT.replacen("## Status\n", "## Status\n@", 1);
        assert_eq!(a.note.text, expected);
    }

    #[test]
    fn debounced_autosave_commits_the_buffer() {
        let mut a = app("");
        a.on_key(key(KeyCode::Char('e')));
        a.on_key(key(KeyCode::Char('z')));
        assert!(a.dirty);
        a.last_edit = Instant::now() - AUTOSAVE_AFTER;
        a.maybe_flush();
        assert!(!a.dirty);
        // Entering edit on an empty note seeds the template first, so the
        // typed char lands on the empty line under `## Status`.
        let expected = crate::template::DEFAULT.replacen("## Status\n", "## Status\nz", 1);
        assert_eq!(a.note.text, expected, "flush committed the edit buffer");
    }

    #[test]
    fn startup_in_edit_mode_loads_the_buffer() {
        let mut a = App::with_note(Note { text: "a\nb".into(), mode: Mode::Edit, ..Default::default() }, false);
        assert_eq!(a.lines, vec!["a".to_string(), "b".to_string()]);
        a.on_key(key(KeyCode::Esc));
        assert_eq!(a.note.text, "a\nb", "leaving edit commits losslessly");
    }

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

    #[test]
    fn overlay_delete_confirm_removes_row() {
        let mut a = app("body");
        a.overlay = Some(Overlay::from_entries(vec![
            OverlayEntry { is_self: true, ..entry("X", state::TabStatus::Closed) },
        ]));
        a.on_key(key(KeyCode::Char('d')));
        assert!(matches!(a.overlay.as_ref().unwrap().mode, OverlayMode::ConfirmDelete));
        a.on_key(key(KeyCode::Char('n'))); // decline
        assert_eq!(a.overlay.as_ref().unwrap().entries.len(), 1);
        a.on_key(key(KeyCode::Char('d')));
        a.on_key(key(KeyCode::Char('y'))); // confirm — file path doesn't exist, remove_file is best-effort
        assert!(a.overlay.as_ref().unwrap().entries.is_empty(), "row removed after confirm");
        // Normal case (active == Tab, the default): deleting your own tab-note
        // row still clears the in-memory buffer, same as before the guard.
        assert_eq!(a.note.text, "", "own tab-note row delete still clears the buffer on the tab-note path");
        assert_eq!(a.note.title, "", "own tab-note row delete still clears the title on the tab-note path");
    }

    #[test]
    fn overlay_rename_enter_updates_row() {
        let mut a = app("body");
        a.overlay = Some(Overlay::from_entries(vec![
            OverlayEntry { is_self: true, ..entry("", state::TabStatus::Closed) },
        ]));
        a.on_key(key(KeyCode::Char('r')));
        a.on_key(key(KeyCode::Char('Z')));
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.overlay.as_ref().unwrap().entries[0].title, "Z");
        assert!(matches!(a.overlay.as_ref().unwrap().mode, OverlayMode::List));
        // Normal case (active == Tab, the default): renaming your own
        // tab-note row still updates the in-memory buffer, same as before.
        assert_eq!(a.note.title, "Z", "own tab-note row rename still updates the buffer on the tab-note path");
    }

    #[test]
    fn overlay_opens_navigates_and_closes() {
        let mut a = app("body");
        a.overlay = Some(Overlay::from_entries(vec![
            OverlayEntry { is_self: true, ..entry("A", state::TabStatus::Live) },
            entry("", state::TabStatus::Closed),
        ]));
        a.on_key(key(KeyCode::Down));
        assert_eq!(a.overlay.as_ref().unwrap().selected, 1);
        a.on_key(key(KeyCode::Down)); // clamps at last
        assert_eq!(a.overlay.as_ref().unwrap().selected, 1);
        a.on_key(key(KeyCode::Up));
        assert_eq!(a.overlay.as_ref().unwrap().selected, 0);
        assert!(!a.on_key(key(KeyCode::Esc)), "Esc closes overlay, never quits");
        assert!(a.overlay.is_none());
    }

    #[test]
    fn from_entries_seeds_visible_as_identity_and_clamps_selection() {
        let ov = Overlay::from_entries(vec![
            entry("A", state::TabStatus::Live),
            entry("B", state::TabStatus::Closed),
        ]);
        assert_eq!(ov.visible, vec![0, 1]);
        assert_eq!(ov.selected_entry().unwrap().title, "A");
    }

    fn entry_with_tab(title: &str, status: state::TabStatus, tab_id: &str) -> OverlayEntry {
        OverlayEntry {
            file: format!("{title}.json").into(),
            title: title.to_string(),
            tab_id: tab_id.to_string(),
            updated: 0,
            status,
            text: String::new(),
            is_self: false,
            is_global: false,
            context: String::new(),
        }
    }

    fn entry(title: &str, status: state::TabStatus) -> OverlayEntry {
        entry_with_tab(title, status, "")
    }

    fn global_row(label: &str) -> OverlayEntry {
        OverlayEntry {
            file: "global.json".into(),
            title: label.to_string(),
            tab_id: String::new(),
            updated: 0,
            status: state::TabStatus::Unknown,
            text: String::new(),
            is_self: false,
            is_global: true,
            context: "global".into(),
        }
    }

    #[test]
    fn global_row_enter_switches_active_and_closes_overlay() {
        let mut a = app("tab body");
        assert_eq!(a.active, ActiveNote::Tab);
        a.overlay = Some(Overlay::from_entries(vec![global_row("★ Global note")]));
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.active, ActiveNote::Global, "enter on the global row switches active");
        assert!(a.overlay.is_none(), "switching closes the overlay");

        a.overlay = Some(Overlay::from_entries(vec![global_row("◂ Back to this tab's note")]));
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.active, ActiveNote::Tab, "entering it again switches back");
    }

    #[test]
    fn current_path_follows_active() {
        let mut a = app("body");
        assert_eq!(a.current_path(), state::state_path(), "Tab -> tab path");
        a.active = ActiveNote::Global;
        assert_eq!(a.current_path(), state::global_path(), "Global -> global path");
    }

    #[test]
    fn global_row_ignores_rename_and_delete() {
        let mut a = app("body");
        a.overlay = Some(Overlay::from_entries(vec![global_row("★ Global note")]));
        a.on_key(key(KeyCode::Char('r')));
        assert!(matches!(a.overlay.as_ref().unwrap().mode, OverlayMode::List), "r is a no-op on the global row");
        a.on_key(key(KeyCode::Char('d')));
        assert!(matches!(a.overlay.as_ref().unwrap().mode, OverlayMode::List), "d is a no-op on the global row");
    }

    #[test]
    fn filter_narrows_rows_live_as_you_type() {
        let mut a = app("body");
        a.overlay = Some(Overlay::from_entries(vec![
            entry("Release Notes", state::TabStatus::Closed),
            entry("Scratch", state::TabStatus::Closed),
        ]));
        a.on_key(key(KeyCode::Char('/')));
        assert!(matches!(a.overlay.as_ref().unwrap().mode, OverlayMode::Filter));
        a.on_key(key(KeyCode::Char('r')));
        a.on_key(key(KeyCode::Char('e')));
        assert_eq!(a.overlay.as_ref().unwrap().visible, vec![0], "narrows to the matching row live");
        a.on_key(key(KeyCode::Enter));
        assert!(matches!(a.overlay.as_ref().unwrap().mode, OverlayMode::List), "Enter commits, returns to List");
        assert_eq!(a.overlay.as_ref().unwrap().visible, vec![0], "filter stays applied");
    }

    #[test]
    fn filter_esc_clears_and_restores_all_rows() {
        let mut a = app("body");
        a.overlay = Some(Overlay::from_entries(vec![
            entry("Release Notes", state::TabStatus::Closed),
            entry("Scratch", state::TabStatus::Closed),
        ]));
        a.on_key(key(KeyCode::Char('/')));
        a.on_key(key(KeyCode::Char('z')));
        assert!(a.overlay.as_ref().unwrap().visible.is_empty(), "no row matches 'z'");
        a.on_key(key(KeyCode::Esc));
        assert!(matches!(a.overlay.as_ref().unwrap().mode, OverlayMode::List));
        assert_eq!(a.overlay.as_ref().unwrap().visible, vec![0, 1], "Esc clears the filter");
    }

    #[test]
    fn filter_never_hides_the_pinned_global_row() {
        let mut a = app("body");
        a.overlay = Some(Overlay::from_entries(vec![
            global_row("★ Global note"),
            entry("Release Notes", state::TabStatus::Closed),
        ]));
        a.on_key(key(KeyCode::Char('/')));
        a.on_key(key(KeyCode::Char('z'))); // matches nothing
        assert_eq!(a.overlay.as_ref().unwrap().visible, vec![0], "global row stays pinned even with 0 matches");
    }

    #[test]
    fn format_row_pads_to_exact_inner_width() {
        let row = format_row(">", "*", "My Note", "spec-droid · claude  2h", 40);
        assert_eq!(dwidth(&row), 40);
        assert!(row.starts_with(" >*My Note"));
        assert!(row.trim_end().ends_with("2h"));
    }

    #[test]
    fn format_row_truncates_wide_char_names_by_display_width() {
        let row = format_row(" ", " ", "文文文文文文文文文文文文文文文文文文文文", "closed  5d", 30);
        assert_eq!(dwidth(&row), 30, "CJK double-width chars must not overflow the row");
    }

    #[test]
    fn format_row_never_exceeds_width_when_right_fills_budget() {
        for (m, s, name, right, w) in [
            (">", "*", "Note", "workspace-name - claude  2h", 20usize),
            (">", "*", "X", "abcdefgh", 10),
            (" ", " ", "anything", "spec-droid · claude  5d", 24),
        ] {
            let row = format_row(m, s, name, right, w);
            assert!(dwidth(&row) <= w, "row {row:?} = {} cols, want <= {w}", dwidth(&row));
        }
    }

    #[test]
    fn g_on_live_row_closes_overlay() {
        let mut a = app("body");
        a.overlay = Some(Overlay::from_entries(vec![entry_with_tab("Live Tab", state::TabStatus::Live, "w1:t1")]));
        a.on_key(key(KeyCode::Char('g')));
        assert!(a.overlay.is_none(), "g on a live row closes the overlay");
    }

    #[test]
    fn g_on_closed_or_global_row_is_a_noop() {
        let mut a = app("body");
        a.overlay = Some(Overlay::from_entries(vec![entry_with_tab("Closed Tab", state::TabStatus::Closed, "w1:t2")]));
        a.on_key(key(KeyCode::Char('g')));
        assert!(a.overlay.is_some(), "g on a closed row is a no-op, overlay stays open");

        let mut b = app("body");
        b.overlay = Some(Overlay::from_entries(vec![global_row("★ Global note")]));
        b.on_key(key(KeyCode::Char('g')));
        assert!(b.overlay.is_some(), "g on the global row is a no-op, overlay stays open");
    }

    #[test]
    fn deleting_own_tab_row_while_on_global_does_not_touch_global_buffer() {
        let mut a = app("GLOBAL BODY");
        a.active = ActiveNote::Global; // pane is showing the global note
        a.note.title = "Global Title".into();
        let mut e = entry_with_tab("Mine", state::TabStatus::Live, "w1:t1");
        e.is_self = true; // the row IS this pane's own tab note (by file identity)
        a.overlay = Some(Overlay::from_entries(vec![e]));
        a.on_key(key(KeyCode::Char('d')));
        a.on_key(key(KeyCode::Char('y'))); // confirm delete
        assert_eq!(a.note.text, "GLOBAL BODY", "global buffer text must survive deleting a tab-note row");
        assert_eq!(a.note.title, "Global Title", "global buffer title must survive");
    }

    #[test]
    fn renaming_own_tab_row_while_on_global_does_not_touch_global_buffer() {
        let mut a = app("GLOBAL BODY");
        a.active = ActiveNote::Global;
        a.note.title = "Global Title".into();
        let mut e = entry_with_tab("Mine", state::TabStatus::Live, "w1:t1");
        e.is_self = true;
        a.overlay = Some(Overlay::from_entries(vec![e]));
        a.on_key(key(KeyCode::Char('r')));
        a.on_key(key(KeyCode::Char('Z')));
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.note.title, "Global Title", "global buffer title must not be overwritten by a tab-row rename");
    }

    #[test]
    fn overlay_rows_show_todo_progress() {
        let mut a = app("body");
        let mut e = entry_with_tab("Busy Tab", state::TabStatus::Live, "w1:t1");
        e.text = "[ ] one\n[x] two\n[x] three".into();
        a.overlay = Some(Overlay::from_entries(vec![e]));
        let screen = rendered(&mut a, 70, 14);
        assert!(screen.contains("2/3"), "{screen}");
    }

    #[test]
    fn overlay_rows_omit_progress_when_the_note_has_no_boxes() {
        let mut a = app("body");
        let mut e = entry_with_tab("Prose Tab", state::TabStatus::Live, "w1:t1");
        e.text = "no tasks here".into();
        a.overlay = Some(Overlay::from_entries(vec![e]));
        let screen = rendered(&mut a, 70, 14);
        assert!(!screen.contains("0/0"), "{screen}");
    }

    #[test]
    fn overlay_row_with_progress_still_fits_the_box() {
        let row = format_row(">", "*", "A Very Long Note Title Indeed", "spec-droid · claude  2/3  2h", 40);
        assert_eq!(dwidth(&row), 40);
    }

    #[test]
    fn recompute_visible_clamps_selection_when_filter_narrows_below_it() {
        let mut ov = Overlay::from_entries(vec![
            entry("Alpha", state::TabStatus::Closed),
            entry("Beta", state::TabStatus::Closed),
            entry("Alfredo", state::TabStatus::Closed),
        ]);
        // Select the last row, then apply a filter that drops it out of view.
        ov.selected = 2;
        ov.filter = "al".into(); // matches "Alpha" + "Alfredo" (case-insensitive), not "Beta"
        ov.recompute_visible();
        assert_eq!(ov.visible, vec![0, 2], "only the two 'al' rows remain");
        assert_eq!(ov.selected, 1, "selection clamps to the last surviving visible row");
        assert_eq!(ov.selected_entry().unwrap().title, "Alfredo");
        // Widen back to everything: selection stays in range, no panic.
        ov.filter.clear();
        ov.recompute_visible();
        assert_eq!(ov.visible, vec![0, 1, 2]);
        assert_eq!(ov.selected, 1, "clamp never grows the selection back on its own");
    }

    #[test]
    fn build_tab_index_maps_live_workspace_and_agent_from_real_shapes() {
        // Captured verbatim from herdr 0.7.4 socket responses (the fields the
        // overlay reads): a live tab in workspace "acme-app" with a reported
        // claude agent, plus a second live tab with no agent (bare shell — the
        // `agent` key is simply absent), plus a "usage" pane that must be
        // ignored, plus a workspace whose label must resolve by id.
        let tabs: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
                {"tab_id":"w1:t1","workspace_id":"w1","label":"1","number":1},
                {"tab_id":"w1:t2","workspace_id":"w1","label":"2","number":2},
                {"tab_id":"w2:t1","workspace_id":"w2","label":"1"}
            ]"#,
        )
        .unwrap();
        let workspaces: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
                {"workspace_id":"w1","label":"acme-app","number":1},
                {"workspace_id":"w2","label":"acme-api"}
            ]"#,
        )
        .unwrap();
        let panes: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
                {"pane_id":"w1:p1","tab_id":"w1:t1","agent":"claude","agent_status":"working"},
                {"pane_id":"w1:p2","tab_id":"w1:t2","agent_status":"unknown"},
                {"pane_id":"w2:p1","tab_id":"w2:t1","agent":"usage"}
            ]"#,
        )
        .unwrap();

        let idx = build_tab_index(&tabs, &workspaces, &panes);

        // Every tab in tab.list is live.
        assert!(idx.live.contains("w1:t1"));
        assert!(idx.live.contains("w1:t2"));
        assert!(idx.live.contains("w2:t1"));
        assert_eq!(idx.live.len(), 3);

        // Live tab with a reported agent -> "workspace · agent".
        let c1 = idx.ctx.get("w1:t1").unwrap();
        assert_eq!(c1.workspace, "acme-app");
        assert_eq!(c1.agent.as_deref(), Some("claude"));
        assert_eq!(state::format_context(state::TabStatus::Live, Some(c1)), "acme-app · claude");

        // Bare-shell tab (no `agent` field) -> workspace only, agent None.
        let c2 = idx.ctx.get("w1:t2").unwrap();
        assert_eq!(c2.workspace, "acme-app");
        assert_eq!(c2.agent, None);
        assert_eq!(state::format_context(state::TabStatus::Live, Some(c2)), "acme-app");

        // The "usage" pane's agent is ignored: w2:t1 resolves its workspace but
        // carries no agent.
        let c3 = idx.ctx.get("w2:t1").unwrap();
        assert_eq!(c3.workspace, "acme-api");
        assert_eq!(c3.agent, None, "the usage agent must not populate context");
    }

    #[test]
    fn build_tab_index_never_panics_on_malformed_items() {
        // Missing fields, wrong types, empty arrays — every bad item is skipped,
        // never a panic (the socket-best-effort contract).
        let tabs: Vec<serde_json::Value> = serde_json::from_str(
            r#"[{"tab_id":123},{"workspace_id":"w1"},{"tab_id":"w1:t1","workspace_id":"wX"}]"#,
        )
        .unwrap();
        let workspaces: Vec<serde_json::Value> =
            serde_json::from_str(r#"[{"workspace_id":"w1"},{"label":"orphan"}]"#).unwrap();
        let idx = build_tab_index(&tabs, &workspaces, &[]);
        // "w1:t1" is live (valid tab_id) but its workspace "wX" has no label,
        // so it gets no context entry — still no panic.
        assert!(idx.live.contains("w1:t1"));
        assert!(idx.ctx.is_empty(), "no resolvable workspace label -> no context, not a crash");
    }

    #[test]
    fn toggle_global_round_trips_tab_and_global_notes_on_disk() {
        // The only test that drives real persistence: point the env-derived
        // store at a temp dir so toggle_global's save()/load() hit throwaway
        // files, never the real APPDATA. Serialized because it mutates
        // process-global env; no other test reads these vars.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let dir = std::env::temp_dir().join(format!("notes-toggle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prev_state = std::env::var_os("HERDR_PLUGIN_STATE_DIR");
        let prev_tab = std::env::var_os("HERDR_TAB_ID");
        // SAFETY: single-threaded within this serialized test; restored below.
        unsafe {
            std::env::set_var("HERDR_PLUGIN_STATE_DIR", &dir);
            std::env::remove_var("HERDR_TAB_ID"); // no tab id -> shared note.json
        }

        // persist=true: exercise the real save/load path.
        let mut a = App::with_note(Note { text: "TAB BODY".into(), ..Default::default() }, true);
        assert_eq!(a.active, ActiveNote::Tab);

        // Tab -> Global: tab note is saved, the (empty) global note loads.
        a.toggle_global();
        assert_eq!(a.active, ActiveNote::Global);
        assert_eq!(a.note.text, "", "empty global note on first switch");
        a.note.text = "GLOBAL BODY".into();

        // Global -> Tab: global note is saved, the tab note reloads intact.
        a.toggle_global();
        assert_eq!(a.active, ActiveNote::Tab);
        assert_eq!(a.note.text, "TAB BODY", "tab note survived the round-trip");

        // Both files exist as SEPARATE documents with the right contents.
        assert_eq!(state::read_note(&dir.join("note.json")).text, "TAB BODY");
        assert_eq!(state::read_note(&dir.join("global.json")).text, "GLOBAL BODY");

        // Restore env and clean up.
        unsafe {
            match prev_state {
                Some(v) => std::env::set_var("HERDR_PLUGIN_STATE_DIR", v),
                None => std::env::remove_var("HERDR_PLUGIN_STATE_DIR"),
            }
            if let Some(v) = prev_tab {
                std::env::set_var("HERDR_TAB_ID", v);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_window_keeps_selection_visible_and_never_overshoots() {
        // Everything fits -> no scroll.
        assert_eq!(list_window(0, 4, 5, 5), 0);
        assert_eq!(list_window(0, 0, 3, 10), 0);
        assert_eq!(list_window(9, 2, 4, 4), 0, "len <= rows resets to 0 even from a stale prev");
        // rows == 0 (degenerate) -> 0, no panic.
        assert_eq!(list_window(3, 3, 10, 0), 0);
        // Selection below the window scrolls it down just enough to show it.
        assert_eq!(list_window(0, 7, 20, 5), 3, "selected 7 becomes the last of a 5-row window (3..8)");
        // Selection above the window scrolls up to it.
        assert_eq!(list_window(10, 4, 20, 5), 4, "selected jumps to the top of the window");
        // Selection already inside the window: offset unchanged.
        assert_eq!(list_window(3, 5, 20, 5), 3, "selected 5 is within 3..8, window doesn't move");
        // Never scrolls past the end: max offset is len - rows.
        assert_eq!(list_window(100, 19, 20, 5), 15, "clamped to len - rows, no trailing blanks");
    }

    #[test]
    fn clamp_scroll_caps_at_last_screenful() {
        assert_eq!(clamp_scroll(0, 10, 5), 0);
        assert_eq!(clamp_scroll(3, 10, 5), 3, "within range: unchanged");
        assert_eq!(clamp_scroll(99, 10, 5), 5, "capped at total - viewport");
        assert_eq!(clamp_scroll(99, 4, 10), 0, "content fits in the viewport -> top");
        assert_eq!(clamp_scroll(2, 0, 0), 0, "degenerate empty content, no panic");
    }

    #[test]
    fn empty_overlay_renders_no_notes_but_a_short_box_never_lies() {
        // Genuinely empty list -> "(no notes)".
        let mut a = app("body");
        a.overlay = Some(Overlay::from_entries(vec![]));
        assert!(
            rendered(&mut a, 40, 12).contains("(no notes)"),
            "an empty list must say so"
        );

        // Non-empty list in a box too short to fit any windowed row
        // (inner_rows == 0 at height 2) must NOT falsely claim "(no notes)".
        let mut b = app("body");
        b.overlay = Some(Overlay::from_entries(vec![entry("Real Note", state::TabStatus::Closed)]));
        assert!(
            !rendered(&mut b, 40, 2).contains("(no notes)"),
            "a real note list must never render (no notes), even when the box is too short to show rows"
        );
    }

    #[test]
    fn open_overlay_builds_pinned_global_first_with_text_and_no_second_read() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("notes-openov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Seed the plugin-state dir directly with three note files + the global.
        state::persist_at(&dir.join("w1_t1.json"),
            &Note { text: "alpha body".into(), title: "Alpha".into(), ..Default::default() }, "w1:t1", 100);
        state::persist_at(&dir.join("w1_t2.json"),
            &Note { text: "beta body".into(), title: "Beta".into(), ..Default::default() }, "w1:t2", 300);
        state::persist_at(&dir.join("global.json"),
            &Note { text: "the shared master note".into(), ..Default::default() }, "", 200);

        let prev_state = std::env::var_os("HERDR_PLUGIN_STATE_DIR");
        let prev_tab = std::env::var_os("HERDR_TAB_ID");
        let prev_sock = std::env::var_os("HERDR_SOCKET_PATH");
        // SAFETY: serialized by ENV_LOCK; restored below. Point the socket at a
        // bogus path so tab_contexts() can't reach a live herdr (the default
        // fallback would otherwise hit the user's real session) -> None ->
        // every row is Unknown, deterministically offline.
        unsafe {
            std::env::set_var("HERDR_PLUGIN_STATE_DIR", &dir);
            std::env::remove_var("HERDR_TAB_ID");
            std::env::set_var("HERDR_SOCKET_PATH", dir.join("no-such.sock"));
        }

        let mut a = App::with_note(Note::default(), false);
        a.open_overlay();
        let ov = a.overlay.as_ref().expect("overlay opened");

        // Global note is the pinned first row, and the ONLY global row.
        assert!(ov.entries[0].is_global, "global row pinned first");
        assert_eq!(ov.entries.iter().filter(|e| e.is_global).count(), 1);
        // It carries the global file's text (read once) and is excluded from
        // the plain note rows.
        assert_eq!(ov.entries[0].text, "the shared master note");
        let regular: Vec<_> = ov.entries.iter().filter(|e| !e.is_global).collect();
        assert_eq!(regular.len(), 2, "two tab notes, global not double-counted");
        assert!(regular.iter().all(|e| !e.text.is_empty()),
            "each row already carries its text (single read, no re-read in open_overlay)");
        // Offline (no socket): every regular row is Unknown context.
        assert!(regular.iter().all(|e| e.status == state::TabStatus::Unknown));

        unsafe {
            match prev_state {
                Some(v) => std::env::set_var("HERDR_PLUGIN_STATE_DIR", v),
                None => std::env::remove_var("HERDR_PLUGIN_STATE_DIR"),
            }
            if let Some(v) = prev_tab {
                std::env::set_var("HERDR_TAB_ID", v);
            }
            match prev_sock {
                Some(v) => std::env::set_var("HERDR_SOCKET_PATH", v),
                None => std::env::remove_var("HERDR_SOCKET_PATH"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn j_k_walk_the_checkbox_cursor_and_clamp() {
        let mut a = app("## Next\n[ ] one\ntext\n[ ] two");
        assert_eq!(a.box_cursor, None, "no cursor until you ask for one");
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(0));
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(1));
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(1), "clamps at the last box");
        a.on_key(key(KeyCode::Char('k')));
        assert_eq!(a.box_cursor, Some(0));
        a.on_key(key(KeyCode::Char('k')));
        assert_eq!(a.box_cursor, Some(0), "clamps at the first box");
    }

    #[test]
    fn k_from_no_cursor_starts_at_the_last_box() {
        let mut a = app("[ ] one\n[ ] two");
        a.on_key(key(KeyCode::Char('k')));
        assert_eq!(a.box_cursor, Some(1));
    }

    #[test]
    fn space_toggles_the_selected_box() {
        let mut a = app("[ ] one\n[ ] two");
        a.on_key(key(KeyCode::Char('j')));
        a.on_key(key(KeyCode::Char('j')));
        a.on_key(key(KeyCode::Char(' ')));
        assert_eq!(a.note.text, "[ ] one\n[x] two");
        assert!(a.dirty, "the toggle must reach disk on the next flush");
        a.on_key(key(KeyCode::Char(' ')));
        assert_eq!(a.note.text, "[ ] one\n[ ] two", "toggles back");
    }

    #[test]
    fn space_with_no_cursor_is_a_noop() {
        let mut a = app("[ ] one");
        a.on_key(key(KeyCode::Char(' ')));
        assert_eq!(a.note.text, "[ ] one");
        assert!(!a.dirty);
    }

    #[test]
    fn checkbox_keys_are_noops_without_checkboxes() {
        let mut a = app("just prose\nmore prose");
        for k in ['j', 'k', ' '] {
            a.on_key(key(KeyCode::Char(k)));
        }
        assert_eq!(a.box_cursor, None);
        assert_eq!(a.note.text, "just prose\nmore prose");
    }

    #[test]
    fn cursor_clamps_when_an_edit_removes_checkboxes() {
        let mut a = app("[ ] one\n[ ] two");
        a.on_key(key(KeyCode::Char('j')));
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(1));
        a.note.text = "[ ] one".into(); // an edit deleted the second box
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(0), "clamped to the surviving box");
        a.note.text = "no boxes left".into();
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, None);
    }

    #[test]
    fn leaving_edit_drops_a_cursor_with_nothing_to_point_at() {
        let mut a = app("[ ] one");
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(0));
        a.on_key(key(KeyCode::Char('e')));
        a.lines = vec!["prose only".into()];
        a.on_key(key(KeyCode::Esc));
        assert_eq!(a.box_cursor, None);
    }

    #[test]
    fn cursor_scrolls_itself_into_view() {
        // 30 boxes in a 10-row body: the last one is far below the fold.
        let text: String = (0..30).map(|i| format!("[ ] item {i}\n")).collect();
        let mut a = app(&text);
        for _ in 0..30 {
            a.on_key(key(KeyCode::Char('j')));
        }
        let _ = rendered(&mut a, 40, 12); // 12 rows - header - hint = 10 body rows
        // Cursor clamps to the last box (index 29). Every line here is a
        // single unwrapped checkbox row, so its rendered row equals its
        // source line (29) — the follow must land it inside the visible
        // window, not merely produce some non-zero scroll.
        assert!(
            a.preview_scroll <= 29 && 29 < a.preview_scroll + a.body_height,
            "cursor's row (29) must be inside the visible window [{}, {}): scroll={}",
            a.preview_scroll,
            a.preview_scroll + a.body_height,
            a.preview_scroll
        );
        // The offset is also exactly derivable: first=29, h=10 (12 rows minus
        // the 1-row header and 1-row hint) -> scroll = 29 + 1 - 10 = 20.
        assert_eq!(a.preview_scroll, 20, "draw must scroll the cursor into view");
    }

    #[test]
    fn manual_scrolling_survives_a_live_checkbox_cursor() {
        // The follow is one-shot: it must not drag the viewport back to the
        // cursor on every frame, or Up/Dn/g/G become unusable after one `j`.
        let text: String = (0..30)
            .map(|i| format!("[ ] item {i}\n"))
            .chain((0..30).map(|i| format!("prose {i}\n")))
            .collect();
        let mut a = app(&text);
        a.on_key(key(KeyCode::Char('j'))); // cursor on the first box
        let _ = rendered(&mut a, 40, 12);
        a.on_key(key(KeyCode::Char('G'))); // jump to the bottom
        let after = rendered(&mut a, 40, 12);
        assert!(a.preview_scroll > 0, "G must not be undone by the follow: {after}");
        assert!(a.box_cursor.is_some(), "scrolling away does not drop the cursor");
    }

    #[test]
    fn preview_footer_falls_back_to_the_short_form_when_narrow() {
        let mut a = app("body");
        assert!(rendered(&mut a, 90, 8).contains("Up/Dn scroll"), "wide pane shows the full hints");
        let narrow = rendered(&mut a, 40, 8);
        assert!(narrow.contains("j/k spc tick"), "the new binding survives truncation: {narrow}");
        assert!(narrow.contains("q quit"), "quit must never be the thing that gets clipped: {narrow}");
    }

    #[test]
    fn esc_drops_the_checkbox_cursor() {
        // A mode you can enter must be a mode you can leave. Esc is the only
        // free key in preview and it must still never quit the TUI.
        let mut a = app("[ ] one\n[ ] two");
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(0));
        assert!(!a.on_key(key(KeyCode::Esc)), "Esc in preview must not quit");
        assert_eq!(a.box_cursor, None, "Esc drops the cursor");
        assert!(!a.follow_box, "and the pending scroll-follow with it");
        // Harmless with no cursor set.
        assert!(!a.on_key(key(KeyCode::Esc)));
        assert_eq!(a.box_cursor, None);
        // The note itself is untouched — Esc cancels the cursor, not an edit.
        assert_eq!(a.note.text, "[ ] one\n[ ] two");
    }

    #[test]
    fn the_footer_advertises_esc_only_while_a_cursor_is_live() {
        let mut a = app("[ ] one");
        assert!(!rendered(&mut a, 90, 8).contains("esc drop"), "no cursor, no hint");
        a.on_key(key(KeyCode::Char('j')));
        let wide = rendered(&mut a, 90, 8);
        assert!(wide.contains("esc drop"), "cursor live, hint shown: {wide}");
        // The hint must not cost `q quit` its place at a real dock width.
        let narrow = rendered(&mut a, 40, 8);
        assert!(narrow.contains("esc drop"), "{narrow}");
        assert!(narrow.contains("q quit"), "{narrow}");
    }

    // ----- per-document state must not survive a document swap or a wipe ---

    #[test]
    fn toggle_global_drops_the_checkbox_cursor() {
        // `box_cursor` is an ordinal into THIS document's checkboxes. Carried
        // across the swap it highlights an arbitrary box in the other note —
        // one that `space` (a pager habit) would then silently tick.
        let mut a = app("[ ] one\n[ ] two\n[ ] three");
        a.on_key(key(KeyCode::Char('j')));
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(1));
        a.toggle_global();
        assert_eq!(a.box_cursor, None, "the cursor is per-document, like preview_scroll");
        assert!(!a.follow_box, "and so is its pending scroll-follow");
    }

    #[test]
    fn clearing_the_note_drops_the_checkbox_cursor() {
        let mut a = app("[ ] one\n[ ] two");
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(0));
        a.on_key(key(KeyCode::Char('x')));
        a.on_key(key(KeyCode::Char('y')));
        assert_eq!(a.note.text, "");
        assert_eq!(a.box_cursor, None, "a stale ordinal must not come back when text does");
        assert!(!a.follow_box);
    }

    #[test]
    fn overlay_self_delete_drops_the_checkbox_cursor() {
        let mut a = app("[ ] one\n[ ] two");
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(0));
        a.overlay = Some(Overlay::from_entries(vec![
            OverlayEntry { is_self: true, ..entry("X", state::TabStatus::Closed) },
        ]));
        a.on_key(key(KeyCode::Char('d')));
        a.on_key(key(KeyCode::Char('y')));
        assert_eq!(a.note.text, "");
        assert_eq!(a.box_cursor, None, "clearing the buffer clears its cursor");
        assert!(!a.follow_box);
    }

    #[test]
    fn seeding_after_a_clear_does_not_resurrect_a_stale_cursor() {
        // x -> e (seeds boxes again) -> Esc re-clamps: a leftover ordinal
        // would hand the user a cursor they never asked for.
        let mut a = app("[ ] one\n[ ] two");
        a.on_key(key(KeyCode::Char('j')));
        a.on_key(key(KeyCode::Char('x')));
        a.on_key(key(KeyCode::Char('y')));
        a.on_key(key(KeyCode::Char('e'))); // seeds the template (has one box)
        a.on_key(key(KeyCode::Esc));
        assert_eq!(a.box_cursor, None, "no cursor until the user asks for one");
    }

    #[test]
    fn save_stamps_updated_so_the_header_age_refreshes() {
        // persist_at stamps a CLONE; without mirroring it back, a note created
        // this session never shows an age at all, and an older one keeps
        // ageing while you type into it (the overlay, which re-reads from
        // disk, then disagrees with the header by hours).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("notes-stamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prev_state = std::env::var_os("HERDR_PLUGIN_STATE_DIR");
        let prev_tab = std::env::var_os("HERDR_TAB_ID");
        // SAFETY: serialized by ENV_LOCK; restored below.
        unsafe {
            std::env::set_var("HERDR_PLUGIN_STATE_DIR", &dir);
            std::env::remove_var("HERDR_TAB_ID");
        }

        let mut a = App::with_note(Note { text: "fresh".into(), ..Default::default() }, true);
        assert_eq!(a.note.updated, 0, "a brand-new note starts unstamped");
        a.finalize();
        assert!(a.note.updated > 0, "save must stamp the live note, not only the clone it writes");
        assert!(a.note.created > 0, "created is stamped on the first write too");
        assert_eq!(a.note.updated, state::read_note(&dir.join("note.json")).updated,
            "in-memory and on-disk timestamps must agree");
        let screen = rendered(&mut a, 60, 8);
        assert!(screen.contains("just now ago"), "the header shows an age right after the first save: {screen}");

        unsafe {
            match prev_state {
                Some(v) => std::env::set_var("HERDR_PLUGIN_STATE_DIR", v),
                None => std::env::remove_var("HERDR_PLUGIN_STATE_DIR"),
            }
            if let Some(v) = prev_tab {
                std::env::set_var("HERDR_TAB_ID", v);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn header_drops_the_age_whole_rather_than_clipping_it() {
        // The header is a 1-row Paragraph with no wrap, so a too-long age is
        // cell-truncated: "2h ago" renders as a bare "2" or "2h ag".
        let mut a = app("body");
        a.note.title = "Sprint Notes".into(); // no digits, so "2h" can only come from the age
        a.note.updated = state::unix_now().saturating_sub(2 * 60 * 60);
        for w in 16..=44u16 {
            let screen = rendered(&mut a, w, 8);
            let header = screen.lines().next().unwrap().to_string();
            let whole = header.contains("2h ago");
            assert!(whole || !header.contains("2h"), "partial age at width {w}: {header:?}");
            assert!(whole || !header.contains("ag"), "partial age at width {w}: {header:?}");
        }
        assert!(
            rendered(&mut a, 44, 8).lines().next().unwrap().contains("2h ago"),
            "a wide enough header still shows the age"
        );
    }

    // ----- overlay row layout ---------------------------------------------

    #[test]
    fn fit_right_drops_progress_then_context_instead_of_truncating() {
        let ctx = "acme-app · claude"; // 17 columns
        assert_eq!(fit_right(ctx, "  2/3", "2h", 40), "acme-app · claude  2/3  2h");
        assert_eq!(fit_right(ctx, "  2/3", "2h", 21), "acme-app · claude  2h", "progress goes first");
        assert_eq!(fit_right(ctx, "  2/3", "2h", 10), "2h", "context goes second");
        assert_eq!(fit_right(ctx, "  2/3", "2h", 1), "", "nothing fits -> nothing shown");
        assert_eq!(fit_right("", "", "2h", 10), "2h", "no context, no stray padding");
    }

    #[test]
    fn format_row_keeps_a_usable_name_at_narrow_widths() {
        // A 40-column right dock gives the overlay box inner_width 34; the old
        // budget order left the title 1 column there (and 0 below it).
        for inner in [24usize, 28, 30, 32, 34] {
            let row = format_row(">", "*", "Release Notes", "acme-app · claude  2/3  2h", inner);
            assert_eq!(dwidth(&row), inner, "row must still fill exactly: {row:?}");
            assert!(
                row.contains("Releas"),
                "the name must stay readable at inner_width {inner}: {row:?}"
            );
        }
    }

    #[test]
    fn overlay_row_shows_both_a_name_and_context_at_a_40_column_dock() {
        let mut a = app("body");
        let mut e = entry_with_tab("Release Notes", state::TabStatus::Live, "w1:t1");
        e.text = "[ ] one\n[x] two\n[x] three".into();
        e.context = "acme-app · claude".into();
        e.updated = state::unix_now().saturating_sub(2 * 60 * 60);
        a.overlay = Some(Overlay::from_entries(vec![e]));
        let screen = rendered(&mut a, 40, 14);
        assert!(screen.contains("Release"), "the name must not collapse to a column or two: {screen}");
        assert!(screen.contains("acme-app"), "session context still fits: {screen}");
        assert!(!screen.contains("2/3"), "the progress count is the first thing dropped: {screen}");
    }
}
