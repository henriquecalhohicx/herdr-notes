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

use crate::markdown::render_markdown;
use crate::state::{self, METADATA_SOURCE, Mode, Note, PANE_LABEL};

/// Debounce for the edit-mode autosave.
const AUTOSAVE_AFTER: Duration = Duration::from_secs(2);
/// Identity re-stamp interval (launcher stale threshold is 20s).
const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);

/// Shown in preview when the note is empty; doubles as the quick-start help.
const EMPTY_HELP: &str = "(empty note)\n\n  e or Enter        start writing\n  Esc               back to preview (saves)\n  Up/Dn PgUp/PgDn   scroll, g/G top/bottom\n  x                 clear the note (asks first)\n  q                 quit\n\nEverything autosaves and survives restarts.";

/// One row in the list overlay.
struct OverlayEntry {
    file: std::path::PathBuf,
    title: String,
    /// Owning tab id ("" for the pinned global row) — used by go-to-tab (`g`).
    #[allow(dead_code)]
    tab_id: String,
    updated: u64,
    status: state::TabStatus,
    text: String,
    is_self: bool,
    /// The pinned `★ Global` row: switches the pane's active note on `enter`
    /// instead of previewing; immune to rename/delete/go-to-tab/filter.
    is_global: bool,
    /// Precomputed row context string ("workspace · agent" / "closed" / "?" /
    /// "global") — see `state::format_context`. Not yet read anywhere (rendering
    /// lands in Task 6).
    #[allow(dead_code)]
    context: String,
}

/// Sub-mode of the open list overlay.
enum OverlayMode {
    List,
    Preview { scroll: usize },
    Rename(String),
    ConfirmDelete,
}

/// The list overlay: all notes on disk, browsable/manageable over the note.
/// `selected` indexes into `visible`, not `entries` directly, so a filter
/// (Task 5) can narrow which rows are shown without disturbing `entries`.
struct Overlay {
    entries: Vec<OverlayEntry>,
    visible: Vec<usize>,
    selected: usize,
    mode: OverlayMode,
}

impl Overlay {
    fn from_entries(entries: Vec<OverlayEntry>) -> Self {
        let mut ov = Overlay { entries, visible: Vec::new(), selected: 0, mode: OverlayMode::List };
        ov.recompute_visible();
        ov
    }

    /// Recomputes which entries are shown. No filter concept yet (Task 5
    /// adds one) — everything is visible. Clamps `selected` into range.
    fn recompute_visible(&mut self) {
        self.visible = (0..self.entries.len()).collect();
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

    let mut ws_label: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for w in &workspaces {
        let (Some(id), Some(label)) = (
            w.get("workspace_id").and_then(|v| v.as_str()),
            w.get("label").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        ws_label.insert(id.to_string(), label.to_string());
    }

    let mut agent_by_tab: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for p in &panes {
        let Some(tab_id) = p.get("tab_id").and_then(|v| v.as_str()) else { continue };
        let Some(agent) = p.get("agent").and_then(|v| v.as_str()) else { continue };
        if agent == "usage" {
            continue;
        }
        agent_by_tab.entry(tab_id.to_string()).or_insert_with(|| agent.to_string());
    }

    let mut live = std::collections::HashSet::new();
    let mut ctx = std::collections::HashMap::new();
    for t in &tabs {
        let Some(tab_id) = t.get("tab_id").and_then(|v| v.as_str()) else { continue };
        live.insert(tab_id.to_string());
        let Some(ws_id) = t.get("workspace_id").and_then(|v| v.as_str()) else { continue };
        let Some(workspace) = ws_label.get(ws_id).cloned() else { continue };
        ctx.insert(
            tab_id.to_string(),
            state::RowContext { workspace, agent: agent_by_tab.get(tab_id).cloned() },
        );
    }
    Some(TabIndex { live, ctx })
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
            app.enter_edit();
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

    fn save(&self) {
        if !self.persist {
            return;
        }
        let Some(path) = self.current_path() else { return };
        let tab_id = match self.active {
            ActiveNote::Tab => state::tab_env().unwrap_or_default(),
            ActiveNote::Global => String::new(),
        };
        state::persist_at(&path, &self.note, &tab_id, state::unix_now());
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
        self.preview_scroll = 0;
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
            KeyCode::Char('e') | KeyCode::Enter => self.enter_edit(),
            KeyCode::Up => self.preview_scroll = self.preview_scroll.saturating_sub(1),
            KeyCode::Down => self.preview_scroll = self.preview_scroll.saturating_add(1),
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
                let text = state::read_note(&s.file).text;
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
                    text,
                }
            })
            .collect();
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
                    if let Some((idx, e)) = ov.selected_entry_mut() {
                        if self.persist {
                            let _ = std::fs::remove_file(&e.file);
                        }
                        if e.is_self {
                            self.note.text.clear();
                            self.note.title.clear();
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

    fn enter_edit(&mut self) {
        self.lines = self.note.text.split('\n').map(String::from).collect();
        self.row = 0;
        self.col = 0;
        self.edit_scroll = 0;
        self.note.mode = Mode::Edit;
    }

    fn leave_edit(&mut self) {
        self.commit();
        self.note.mode = Mode::Preview;
        self.dirty = false;
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
        }
        frame.render_widget(Paragraph::new(Line::from(title)), title_a);

        let hints = match self.note.mode {
            Mode::Preview => " e edit  r title  l list  Up/Dn scroll  x clear  q quit",
            Mode::Edit => " Esc preview (saves)   Ctrl+S save",
        };
        frame.render_widget(
            Paragraph::new(Span::styled(hints, Style::default().add_modifier(Modifier::DIM))),
            hint_a,
        );

        if self.confirm_clear {
            draw_confirm(frame, area);
        }
        if let Some(ov) = &self.overlay {
            draw_overlay(frame, area, ov);
        }
    }

    /// Renders the preview body; returns a "top-line/total" scroll hint when
    /// the content overflows the pane.
    fn draw_preview(&mut self, frame: &mut Frame, area: Rect) -> Option<String> {
        if self.note.text.trim().is_empty() {
            self.preview_scroll = 0;
            frame.render_widget(
                Paragraph::new(EMPTY_HELP).style(Style::default().add_modifier(Modifier::DIM)),
                area,
            );
            return None;
        }
        // The rightmost column is reserved for the overflow scrollbar so text
        // never sits underneath it.
        let text_w = usize::from(area.width).saturating_sub(1).max(1);
        let lines = render_markdown(&self.note.text, text_w);
        let total = lines.len();
        let max = total.saturating_sub(usize::from(area.height));
        self.preview_scroll = self.preview_scroll.min(max);
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

fn draw_overlay(frame: &mut Frame, area: Rect, ov: &Overlay) {
    // Centering below relies on w <= area.width and h <= area.height — on a
    // very narrow/short pane the clamp(20, 60) / .max(3) floors can otherwise
    // exceed the area and underflow the saturating_sub centering math (a
    // panic on `.saturating_sub` never happens, but plain subtraction would).
    let w = area.width.saturating_sub(4).clamp(20, 60).min(area.width);
    // Preview wants a tall box to read the note; the list sizes to its rows;
    // the one-line rename/confirm prompts stay short.
    let h = match &ov.mode {
        OverlayMode::Preview { .. } => area.height.saturating_sub(2).max(3).min(area.height),
        OverlayMode::List => {
            let content_h = u16::try_from(ov.entries.len() + 2).unwrap_or(u16::MAX);
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
        OverlayMode::List => {
            let now = state::unix_now();
            let mut lines: Vec<Line> = Vec::new();
            for (i, &idx) in ov.visible.iter().enumerate() {
                let e = &ov.entries[idx];
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
                Paragraph::new(lines).block(
                    Block::bordered()
                        .title(" All notes   ↑↓ move  enter preview ")
                        .title_bottom(" r rename  d delete  esc "),
                ),
                rect,
            );
        }
    }
}

fn clen(s: &str) -> usize {
    s.chars().count()
}

fn byte_idx(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(b, _)| b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn app(text: &str) -> App {
        App::with_note(
            Note { text: text.to_string(), mode: Mode::Preview, ..Default::default() },
            false, // never touch the real state file from tests
        )
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
        assert_eq!(a.note.text, "@");
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
        assert_eq!(a.note.text, "z", "flush committed the edit buffer");
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
        a.overlay = Some(Overlay::from_entries(vec![entry("X", state::TabStatus::Closed)]));
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
        a.overlay = Some(Overlay::from_entries(vec![entry("", state::TabStatus::Closed)]));
        a.on_key(key(KeyCode::Char('r')));
        a.on_key(key(KeyCode::Char('Z')));
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.overlay.as_ref().unwrap().entries[0].title, "Z");
        assert!(matches!(a.overlay.as_ref().unwrap().mode, OverlayMode::List));
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
}
