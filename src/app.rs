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

use crate::markdown::{self, render_markdown};
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

/// What the prompt block needs to know about a pane. Built from one
/// `pane.list` call; every field is optional at the source, so a missing one
/// degrades rather than dropping the pane.
struct PaneInfo {
    /// "" when herdr has not reported an agent on this pane yet — a bare
    /// shell pane carries only `agent_status`.
    agent: String,
    tab_id: String,
    title: Option<String>,
    cwd: Option<String>,
    /// herdr's pane label — the name the user gave this pane. `None` until one
    /// is set: `pane.list` omits the key entirely, which is why a dump taken
    /// before any rename made phase C conclude no such field existed.
    label: Option<String>,
}

impl PaneInfo {
    /// The best human-readable name for this pane: its herdr LABEL when set,
    /// else its terminal title when that actually says something. The ONE
    /// definition, shared by `pane_label`, `pick_title` and `maybe_autotitle`'s
    /// source-1 probe — that probe exists only to decide whether to spawn
    /// `git`, so if it ever drifted from `pick_title`'s copy the branch would
    /// be computed when it should not be, or skipped when it should not be.
    ///
    /// A label deliberately does NOT go through `meaningful_title`. That
    /// rejection list — the generic tool names in `GENERIC_TITLES`, path-shaped
    /// strings, a `.exe` suffix — exists because `terminal_title_stripped` is
    /// machine-set and unreliable. A label is a string the user typed on
    /// purpose, so rejecting `src/app.rs` as path-shaped would be overruling
    /// them.
    fn nice_title(&self) -> Option<String> {
        if let Some(label) = self.label.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
            return Some(label.to_string());
        }
        self.title.as_deref().and_then(|t| meaningful_title(t, &self.agent))
    }
}

type PaneIndex = std::collections::HashMap<String, PaneInfo>;

/// Titles herdr reports that name the tool rather than the work. Compared
/// case-insensitively against the trimmed title.
const GENERIC_TITLES: [&str; 4] = ["claude code", "claude", "codex", "codex cli"];

/// One `pane.list` round-trip. `None` on any call or parse failure — every
/// caller falls back, so the block works offline.
fn pane_index() -> Option<PaneIndex> {
    Some(build_pane_index(&fetch_array("pane.list", "panes")?))
}

/// Pure builder over an already-fetched `panes` array — no I/O, so it is
/// unit-tested against captured live responses. An item with no `pane_id` is
/// the only thing skipped; everything else degrades to a default.
fn build_pane_index(panes: &[serde_json::Value]) -> PaneIndex {
    let mut out = PaneIndex::new();
    for p in panes {
        let Some(pane_id) = p.get("pane_id").and_then(|v| v.as_str()) else { continue };
        out.insert(
            pane_id.to_string(),
            PaneInfo {
                agent: p.get("agent").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                tab_id: p.get("tab_id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                title: p
                    .get("terminal_title_stripped")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                cwd: p.get("cwd").and_then(|v| v.as_str()).map(|s| s.to_string()),
                label: p.get("label").and_then(|v| v.as_str()).map(|s| s.to_string()),
            },
        );
    }
    out
}

/// A terminal title worth showing: trimmed, non-empty, not the tool naming
/// itself (`Claude Code` on an idle pane), and not a filesystem path (a bare
/// shell pane reports its `powershell.exe` path). `None` when it says nothing.
fn meaningful_title(title: &str, agent: &str) -> Option<String> {
    let t = title.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    if GENERIC_TITLES.contains(&lower.as_str()) || lower == agent.trim().to_ascii_lowercase() {
        return None;
    }
    if t.contains('/') || t.contains('\\') || lower.ends_with(".exe") {
        return None;
    }
    Some(t.to_string())
}

/// The heading for a pane's prompt group: the pane's `nice_title()` — its
/// herdr label when set, else its terminal title when meaningful — otherwise
/// `{agent} {pane-suffix}` (`claude p8`) built from data the stored prompt
/// always carries — so a closed pane or an unreachable socket still names
/// its group.
fn pane_label(pane_id: &str, agent: &str, index: Option<&PaneIndex>) -> String {
    if let Some(info) = index.and_then(|i| i.get(pane_id))
        && let Some(title) = info.nice_title()
    {
        return title;
    }
    let suffix = pane_id.rsplit(':').next().unwrap_or(pane_id);
    if agent.trim().is_empty() {
        suffix.to_string()
    } else {
        format!("{agent} {suffix}")
    }
}

/// The title chain: the agent pane's `nice_title()` (its herdr label when
/// set, else its terminal title when meaningful), then the git branch, then
/// the oldest surviving captured prompt. `None` when nothing has resolved
/// yet — the caller retries on the next heartbeat.
fn pick_title(
    agent_pane: Option<&PaneInfo>,
    branch: Option<&str>,
    oldest_prompt: Option<&str>,
) -> Option<String> {
    if let Some(p) = agent_pane
        && let Some(t) = p.nice_title()
    {
        return Some(t);
    }
    // A detached HEAD names nothing.
    if let Some(b) = branch.map(str::trim).filter(|b| !b.is_empty() && *b != "HEAD") {
        return Some(b.to_string());
    }
    oldest_prompt.map(str::trim).filter(|p| !p.is_empty()).map(|p| p.to_string())
}

/// The agent pane `maybe_autotitle` reads from for this tab, among panes on
/// the tab that have reported a non-empty agent.
///
/// The PRIMARY key is whether the pane carries a herdr `label`: a labelled
/// pane always beats an unlabelled one, however the ids sort. A label is the
/// user deliberately naming that pane, and honoring it is this phase's whole
/// promise — "rename a pane, the note's title follows". Selecting by id alone
/// broke that on any multi-pane tab: an idle unlabelled pane with a lower id
/// reports the generic terminal title `Claude Code`, `nice_title` rejects it,
/// source 1 misses, and `maybe_autotitle`'s no-demotion rule then pins the
/// note to the git branch — after which the rename can NEVER reach the note on
/// any later beat. `label` is absent from `pane.list` until a pane is named,
/// so unlabelled is the normal state and this is the common case, not a corner
/// one. A blank/whitespace label does not count, matching `nice_title`.
///
/// The SECONDARY key is the lowest pane id, which is what makes the choice
/// deterministic. `PaneIndex` is a `HashMap`, so iterating `.values().find(...)`
/// picks whichever pane the hash happens to visit first — arbitrary, and
/// varying per process, on exactly the tab shape this feature targets (a 2x2
/// agent grid). Sorting by pane id makes the same tab state always yield the
/// same pane, and so the same title/cwd — including when two panes are both
/// labelled.
///
/// herdr's synthetic `usage` pane carries a real `tab_id` and a non-empty
/// agent, so it is skipped here exactly as `build_tab_index` skips it — and the
/// skip is a filter, so it holds even if that pane somehow carries a label.
/// Picking it would make its title source 1 and — worse — its cwd the one cwd
/// this tab ever gets to spend on `git rev-parse`, since `git_tried` caches the
/// result for that cwd once the spawn returns — so the wrong cwd would
/// permanently claim the tab's one attempt.
fn pick_agent_pane<'a>(index: &'a PaneIndex, tab: &str) -> Option<&'a PaneInfo> {
    index
        .iter()
        .filter(|(_, p)| p.tab_id == tab && !p.agent.trim().is_empty() && p.agent != "usage")
        .map(|(id, p)| (id.as_str(), p))
        // `false` sorts before `true`, so "has a label" ranks ahead of "has none".
        .min_by_key(|(id, p)| {
            (p.label.as_deref().map(str::trim).filter(|l| !l.is_empty()).is_none(), *id)
        })
        .map(|(_, p)| p)
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
    /// Prefix→URL templates for issue keys, loaded ONCE (see `App::new`).
    /// Default-empty in `with_note`, so unit tests never read the store dir.
    tickets: crate::tickets::Config,
    /// Links (ticket keys and URLs) found by the last preview draw, in
    /// document order. The draw is the single scan that both styles the
    /// targets and lists them, so nav and highlight cannot disagree. The loop
    /// always draws before reading a target, so this is never consulted stale.
    link_hits: Vec<markdown::LinkHit>,
    /// Ordinal into `link_hits` — which target `o` would open. Mutually
    /// exclusive with `box_cursor`: one cursor at a time keeps `esc` and
    /// `space` unambiguous.
    link_cursor: Option<usize>,
    /// One-shot scroll-follow, same contract as `follow_box`.
    follow_link: bool,
    /// Browser launches still running, reaped on the heartbeat so unix does not
    /// accumulate a zombie per `o`.
    open_children: Vec<std::process::Child>,
    /// The tab's captured prompts, grouped per agent pane and newest group
    /// first, each with the heading resolved at refresh time. Refreshed on the
    /// heartbeat rather than per draw. Rendered above the note, never part of
    /// the edit buffer.
    prompts: Vec<crate::prompts::PromptGroup>,
    /// Heading per group, index-aligned with `prompts`. Resolved from one
    /// `pane.list` call at refresh time so the draw path stays I/O-free.
    prompt_labels: Vec<String>,
    /// Branch lookups already made, keyed by cwd: `Some(branch)` cached from a
    /// success, `None` cached from a failure. Caching the SUCCESS matters
    /// because the empty-title path re-runs on EVERY heartbeat until
    /// something fills it (`maybe_autotitle`'s no-demotion rule means a note
    /// whose title is already set never asks the branch again at all) — so
    /// without it, a tab that stays untitled for several beats (no pane
    /// candidate yet, no prompt captured yet) would spawn `git` again on
    /// every one of those beats, not just the first. Caching the FAILURE is
    /// what bounds the spawn on the other side: without it a tab that is not
    /// a repo would spawn `git` every 5 seconds for the life of the pane. See
    /// `git_branch` for what a hang costs.
    git_tried: std::collections::HashMap<String, Option<String>>,
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
        // Populate the prompt block NOW rather than leaving it blank until the
        // first heartbeat 5s from now: "come back after an hour and the pane
        // already tells you where you were" is the whole feature, and five
        // seconds of blank block reads as capture being broken. Here rather
        // than in `with_note` so the persist=false unit tests never touch the
        // real store dir (`refresh_prompts` early-returns on !persist anyway —
        // this is belt and braces).
        app.refresh_prompts();
        // Real disk read, so it belongs beside `refresh_prompts` here rather
        // than in `with_note` — the test constructor must stay hermetic.
        app.tickets = crate::tickets::Config::load();
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
            tickets: crate::tickets::Config::default(),
            link_hits: Vec::new(),
            link_cursor: None,
            follow_link: false,
            open_children: Vec::new(),
            prompts: Vec::new(),
            prompt_labels: Vec::new(),
            git_tried: std::collections::HashMap::new(),
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
                ActiveNote::Global => state::load_global(),
            };
        }
        self.note.mode = Mode::Preview;
        // Everything per-DOCUMENT resets: a scroll offset, a checkbox ordinal
        // and a link ordinal all mean nothing in the other note. Anything
        // added to this struct that describes a position INSIDE the note
        // belongs here too.
        self.preview_scroll = 0;
        self.clear_cursors();
        self.link_hits.clear();
        self.dirty = false;
        // The prompt block belongs to the TAB note only, so it has to be
        // dropped going out and rebuilt coming back — immediately, not on the
        // next 5s heartbeat.
        self.refresh_prompts();
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
        // Non-blocking: a browser still running just stays in the list.
        self.open_children.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_)) | Err(_)));
        self.report_tokens();
        // Prompt labels and the auto-title both read the SAME `pane.list`
        // snapshot. Fetching one each meant two round-trips on every beat —
        // and for a tab that never resolves a title (no agent pane, or no
        // prompts yet, both normal states) that doubling never ends. Load the
        // prompt files first (no socket traffic), then decide ONCE whether
        // anything actually needs a snapshot, then share it. `None` — nothing
        // needed it, or the socket is unreachable — is the offline path both
        // consumers already handle.
        self.load_prompts();
        let index = (!self.prompts.is_empty() || self.autotitle_wanted())
            .then(pane_index)
            .flatten();
        self.label_prompts(index.as_ref());
        self.maybe_autotitle(index.as_ref());
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

    /// Re-read the tab's prompt files AND resolve their headings, fetching a
    /// `pane.list` snapshot only when there is something to label. For the
    /// callers that hold no snapshot to share (construction, `toggle_global`);
    /// `heartbeat` drives `load_prompts` + `label_prompts` itself so it can
    /// share one snapshot with `maybe_autotitle`.
    fn refresh_prompts(&mut self) {
        self.load_prompts();
        let index = (!self.prompts.is_empty()).then(pane_index).flatten();
        self.label_prompts(index.as_ref());
    }

    /// Re-read the tab's prompt files into `self.prompts`. Only the tab note
    /// has prompts — the global note is not a tab. Gated on `persist` so unit
    /// tests never touch the real store dir. Deliberately does NO socket I/O:
    /// labelling is a separate step so a caller can look at the loaded groups
    /// before deciding whether a round-trip is worth making.
    /// Clears FIRST (labels too, so the two can never be left mismatched), so
    /// every exit from this function leaves a consistent block. The two
    /// `let … else` arms below are process-env-derived and so constant for the
    /// pane's lifetime, meaning no stale value is reachable today — but an
    /// asymmetric clear is a trap for the next change.
    fn load_prompts(&mut self) {
        self.prompts.clear();
        self.prompt_labels.clear();
        if !self.persist || !self.showing_tab_note() {
            return;
        }
        let Some(dir) = state::store_dir() else { return };
        let Some(key) = state::tab_env().as_deref().and_then(state::id_key) else { return };
        self.prompts = crate::prompts::load_for_tab(&dir, &key);
    }

    /// Resolve one heading per loaded prompt group from an already-fetched
    /// `pane.list` snapshot, index-aligned with `self.prompts`. `None` (socket
    /// unreachable, or the caller judged the round-trip unnecessary) falls
    /// every group back to `{agent} {pane-suffix}`.
    fn label_prompts(&mut self, index: Option<&PaneIndex>) {
        self.prompt_labels = self
            .prompts
            .iter()
            .map(|g| {
                let agent = g.prompts.first().map(|p| p.agent.as_str()).unwrap_or("");
                pane_label(&g.pane, agent, index)
            })
            .collect();
    }

    /// The oldest captured prompt still on disk, across every group. The ring
    /// evicts, so this is the oldest SURVIVING prompt, not necessarily the
    /// first one ever sent.
    fn oldest_prompt_text(&self) -> Option<String> {
        self.prompts
            .iter()
            .flat_map(|g| g.prompts.iter())
            .min_by_key(|p| p.ts)
            .map(|p| p.text.clone())
    }

    /// `git rev-parse --abbrev-ref HEAD` in `cwd`, cached in `git_tried` so
    /// the subprocess runs at most once per cwd for the life of this process
    /// — both a success AND a failure are cached, and a repeat call for the
    /// same cwd returns the cached answer instead of spawning again. A
    /// detached HEAD comes back as `Some("HEAD")` here and is rejected
    /// downstream, by `pick_title` — not here — every time it is asked, cache
    /// hit or not. On Windows the child is spawned with CREATE_NO_WINDOW so a
    /// console never flashes over the TUI.
    ///
    /// `cmd.output()` is a BLOCKING wait with no timeout, run on the event-loop
    /// thread from inside `heartbeat()`. The once-per-cwd bound is what keeps
    /// that survivable, and it is why relaxing that bound is not a free
    /// change: a cwd on a disconnected network share, or a repo with a stuck
    /// index lock, stalls input, drawing AND the heartbeat's identity
    /// re-stamp for as long as `git` hangs. Past 20s of no re-stamp the
    /// launcher classifies this live pane as a corpse; the next toggle takes
    /// the REPLACE path, and `herdr pane close` kills with no signal, losing
    /// whatever is sitting in the dirty debounce buffer. One stall per cwd per
    /// process is the ceiling that keeps that chain from being reachable
    /// repeatedly — anyone loosening it needs to add a timeout first.
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

    /// Whether a title should be derived on this beat. `title_auto` is the
    /// freeze switch — only typing a title with `r` clears it. There is
    /// deliberately no "title is empty" condition: an auto title TRACKS its
    /// source, so renaming a pane updates the note within one heartbeat even
    /// if the branch name had already landed.
    ///
    /// A note with NOTHING in it is still excluded, through the exact
    /// predicate the delete-on-save rule uses (`state::is_blank`, checked by
    /// `persist_at`) rather than a second emptiness test — so the two stay in
    /// lockstep by construction. Deriving a title into an empty note makes it
    /// non-blank, which stops the delete rule firing: a tab you only toggled
    /// Notes into would leave a `{"text":"","title":"main"}` orphan forever
    /// (tab ids are never reused), the `l` dashboard would fill with
    /// identical empty rows, prompt capture's gate 4 ("a note file exists for
    /// this tab") would arm permanently, and an overlay self-delete would be
    /// undone by the very next heartbeat re-deriving the title. `is_blank`
    /// also matches the pristine seed template, which is wanted here:
    /// seeded-but-untyped stays deletable.
    fn autotitle_wanted(&self) -> bool {
        self.persist
            && self.showing_tab_note()
            && self.note.title_auto
            && !state::is_blank(&self.note)
    }

    /// Derive a title for an auto-titled note, and keep re-deriving it on
    /// every beat `autotitle_wanted` allows — renaming a pane's title (or its
    /// branch, or its prompts) updates the note within one heartbeat rather
    /// than freezing at whatever the first successful derivation found.
    /// `title_auto` stays true throughout — it records that the title is
    /// derived, not that one is still pending — and only typing a title with
    /// `r` clears it, which is the sole way to stop this from running again.
    ///
    /// An EXISTING title may only be REPLACED by a pane-derived candidate
    /// (source 1, below) — never demoted to the branch or a captured prompt.
    /// An EMPTY title (trimmed — a whitespace-only title from a hand-edited
    /// or legacy file is "empty" here exactly as `title_auto`'s own
    /// missing-field default treats it, `state::parse`) still runs the full
    /// chain, because filling one is the whole feature. Without that split,
    /// re-deriving every beat can demote a good title on its own, with no
    /// rename involved:
    ///   - `heartbeat` collapses `index` to `None` on any transient
    ///     `pane.list` failure, which silently drops sources 1 and 2 for that
    ///     one beat only; the chain would fall through to whatever prompt
    ///     happens to be oldest, then flip back on the next good beat.
    ///   - an agent going idle reports the generic terminal title `Claude
    ///     Code`, which `nice_title` rejects; without the split the chain
    ///     would fall to the branch, then bounce back to the pane title the
    ///     next time the agent goes busy.
    ///
    /// Both flaps pass the compare-before-write guard below legitimately —
    /// the derived value really did change — so demotion has to be refused
    /// one layer up, not caught by that guard.
    ///
    /// `index` is a `pane.list` snapshot the caller already holds (the
    /// heartbeat shares one with the prompt labels); `None` is the offline
    /// path — sources 1 and 2 are simply unavailable and a captured prompt is
    /// the only one left (and only reachable at all while the title is still
    /// empty).
    fn maybe_autotitle(&mut self, index: Option<&PaneIndex>) {
        if !self.autotitle_wanted() {
            return;
        }
        let Some(tab) = state::tab_env() else { return };
        let agent_pane = index.and_then(|i| pick_agent_pane(i, &tab));
        // Source 1, computed separately from the rest of the chain: it is
        // what decides whether the branch is worth spawning for below, AND
        // (per the no-demotion rule above) the ONLY source ever allowed to
        // replace a title that is already set. Asked through the SAME
        // `nice_title` that `pick_title` uses, so the two can never disagree
        // about whether source 1 hit.
        let source1 = agent_pane.and_then(PaneInfo::nice_title);
        let title = if self.note.title.trim().is_empty() {
            let cwd = agent_pane.and_then(|p| p.cwd.clone());
            // Only compute the branch — and only then bear its process-spawn
            // cost — once source 1 has actually missed: when source 1 hits,
            // `pick_title` never falls through to it and the result would
            // just be thrown away.
            let branch = if source1.is_none() { cwd.and_then(|c| self.git_branch(&c)) } else { None };
            let oldest = self.oldest_prompt_text();
            pick_title(agent_pane, branch.as_deref(), oldest.as_deref())
        } else {
            source1
        };
        // Only write — and only `touch()` — when the derived value actually
        // differs from what is already there. Without this, every heartbeat
        // would dirty the note, the 2s autosave would fire forever, `updated`
        // would keep bumping and the header age would reset to `just now` on
        // a loop, even when nothing about the source has changed.
        if let Some(title) = title
            && title != self.note.title
        {
            self.note.title = title;
            self.touch();
        }
    }

    /// `prompts` zipped with their resolved headings, for the renderer. A
    /// group whose label is missing (labels cleared, prompts not) falls back
    /// to the raw pane id rather than dropping the group.
    fn labelled_prompts(&self) -> Vec<(String, Vec<crate::prompts::Prompt>)> {
        self.prompts
            .iter()
            .enumerate()
            .map(|(i, g)| {
                let label = self.prompt_labels.get(i).cloned().unwrap_or_else(|| g.pane.clone());
                (label, g.prompts.clone())
            })
            .collect()
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
                self.clear_cursors();
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
            KeyCode::Char('j') => {
                self.clear_link_cursor();
                self.move_box(1)
            }
            KeyCode::Char('k') => {
                self.clear_link_cursor();
                self.move_box(-1)
            }
            KeyCode::Char(' ') => {
                self.clear_link_cursor();
                self.toggle_box()
            }
            KeyCode::Char('n') => self.move_link(1),
            KeyCode::Char('N') => self.move_link(-1),
            KeyCode::Char('o') => self.open_ticket(),
            // The only way out of either preview cursor. Without it the
            // highlight is a mode you can enter and not leave — the other
            // exits are all side effects (swap documents, `x` clear, edit the
            // last box/link away). Esc is otherwise unbound here and still
            // must never quit the TUI.
            KeyCode::Esc => self.clear_cursors(),
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
                    // Typing a title freezes it; clearing it hands the note
                    // back to auto-titling on the next heartbeat.
                    self.note.title_auto = self.note.title.is_empty();
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
                            // `set_title` just wrote this same rule to DISK.
                            // The in-memory buffer wins on the next `save()`,
                            // so leaving it stale would either strand a
                            // cleared title as still-manual (auto-titling
                            // never resumes) or claim a typed one is still
                            // derivable — and the next edit writes the stale
                            // value back over the disk one, permanently.
                            self.note.title_auto = self.note.title.is_empty();
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
                            // Same rule as the rename path: a wiped title is
                            // derivable again, in memory as well as on disk.
                            self.note.title_auto = self.note.title.is_empty();
                            self.clear_cursors();
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
    /// (`toggle_global`, `x` clear, overlay self-delete) must call
    /// `clear_cursors`, not this directly — a cursor added later (the link
    /// one did) would otherwise survive a swap this function alone cannot see.
    fn clear_box_cursor(&mut self) {
        self.box_cursor = None;
        self.follow_box = false;
    }

    /// Re-clamps the link ordinal against the hits the last draw found, and
    /// drops it when there are none. Called from the draw, since the hit list
    /// is a draw product and an edit can delete a target.
    fn clamp_link_cursor(&mut self) {
        let n = self.link_hits.len();
        self.link_cursor = match self.link_cursor {
            Some(c) if n > 0 => Some(c.min(n - 1)),
            _ => None,
        };
    }

    fn clear_link_cursor(&mut self) {
        self.link_cursor = None;
        self.follow_link = false;
    }

    /// Drops BOTH preview cursors. Every path that swaps or wipes the buffer
    /// calls this rather than either single clear, so a cursor added later
    /// cannot be missed by a document swap — the recurring bug class in this
    /// crate (see the `toggle_global` / `global.json` gotchas).
    fn clear_cursors(&mut self) {
        self.clear_box_cursor();
        self.clear_link_cursor();
    }

    /// Steps the link cursor over the last draw's hits. From no cursor, `n`
    /// lands on the first target and `N` on the last. Clamps at both ends;
    /// does nothing when the note has no links.
    fn move_link(&mut self, delta: isize) {
        self.clamp_link_cursor();
        let n = self.link_hits.len();
        if n == 0 {
            return; // clamp already dropped the cursor
        }
        self.clear_box_cursor(); // one cursor at a time
        self.link_cursor = Some(match self.link_cursor {
            None if delta > 0 => 0,
            None => n - 1,
            Some(c) => c.saturating_add_signed(delta).min(n - 1),
        });
        self.follow_link = true;
    }

    /// The URL `o` would open right now, or `None`. Separate from `open_ticket`
    /// so the resolution is testable without launching a browser.
    fn pending_open(&self) -> Option<String> {
        let hit = self.link_cursor.and_then(|c| self.link_hits.get(c))?;
        match hit.kind {
            markdown::LinkKind::Ticket => crate::tickets::ticket_url(&self.tickets, &hit.text),
            markdown::LinkKind::Url => Some(hit.text.clone()),
        }
    }

    /// Opens the cursored link. Silent no-op when there is no cursor, no
    /// mapping, or the spawn fails — nothing may print from the TUI.
    fn open_ticket(&mut self) {
        let Some(url) = self.pending_open() else { return };
        if let Some(child) = crate::tickets::open(&url) {
            self.open_children.push(child);
        }
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

        // The full hint line no longer fits a narrow right dock (the user's
        // real dock is ~46 columns), so tokens degrade one at a time by rank
        // instead of jumping between two fixed forms — the old short form
        // had no room for the link hint at all, making `n`/`N` invisible in
        // exactly the pane the feature ships in. `q quit` is rank 0 and
        // never drops; below its own length the terminal clips, same as
        // before. `esc drop` is state-scoped: it is the only exit from
        // whichever cursor is live (checkbox or link), so it is offered only
        // in `HINTS_BOX`/`HINTS_LINK`, not `HINTS_PREVIEW` — advertising it
        // with no cursor live would spend scarce columns on a key that does
        // nothing. `o open` is `HINTS_LINK`-only for the same reason: it only
        // does something once a link cursor exists.
        let hints = match self.note.mode {
            Mode::Preview => {
                let tokens = if self.link_cursor.is_some() {
                    HINTS_LINK
                } else if self.box_cursor.is_some() {
                    HINTS_BOX
                } else {
                    HINTS_PREVIEW
                };
                fit_hints(tokens, usize::from(hint_a.width))
            }
            Mode::Edit => " Esc preview (saves)   Ctrl+S save".to_string(),
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
        // The rightmost column is reserved for the overflow scrollbar so text
        // never sits underneath it.
        let text_w = usize::from(area.width).saturating_sub(1).max(1);
        // Built BEFORE the empty-note branch: a note with a title but no body
        // is not blank, so its file persists, so capture's note-file gate keeps
        // passing and prompts keep accumulating for it. Rendering only the help
        // there would hide them forever and read as capture being broken.
        // `refresh_prompts` already clears `self.prompts` off the tab note in
        // the running app, but a unit test can set `active`/`prompts` directly
        // without going through it, so the render site re-checks
        // `showing_tab_note()` itself rather than trusting that invariant to
        // still hold by the time we draw.
        let block = if self.showing_tab_note() {
            prompt_block(&self.labelled_prompts(), text_w)
        } else {
            Vec::new()
        };
        // The empty-note help used to be a fixed-height special case (forced
        // `preview_scroll = 0`, no hint, no scrollbar) on the assumption the
        // block could never exceed a couple of rows. Grouping removed that
        // bound — a multi-agent tab's block alone can run well past the pane
        // height — so a titled, body-less note in such a tab would push the
        // help off screen with no key able to reach it. Both branches now
        // build `(lines, map)` and share the exact same
        // clamp/scroll/scrollbar/hint tail below; the help rows map to NO
        // source line (same as the block's rows), which is safe here for a
        // stronger reason than in the real-note branch: an empty note has no
        // checkboxes at all (`markdown::checkbox_lines` on empty text is
        // always empty), so `cursor_line()` is unconditionally `None` on this
        // path and the highlight/follow block below is a no-op regardless of
        // what `map` contains.
        let (mut lines, map): (Vec<Line<'static>>, Vec<Option<usize>>) =
            if self.note.text.trim().is_empty() {
                // No text, so no links — and no stale hits left behind for `o`.
                self.link_hits.clear();
                let mut lines = block;
                lines.extend(empty_help().lines().map(|l| {
                    Line::from(Span::styled(l.to_string(), Style::default().add_modifier(Modifier::DIM)))
                }));
                let map = vec![None; lines.len()];
                (lines, map)
            } else {
                let (mut lines, mut map, mut hits) = markdown::render_markdown_links(
                    &self.note.text,
                    text_w,
                    &self.tickets,
                    self.link_cursor,
                );
                // The block's rows map to NO source line, so the checkbox cursor can
                // never land on one and the highlight/scroll-follow keep pointing at
                // real note lines. Edit mode never reaches here.
                if !block.is_empty() {
                    let n = block.len();
                    let mut merged = block;
                    merged.append(&mut lines);
                    lines = merged;
                    let mut merged_map = vec![None; n];
                    merged_map.append(&mut map);
                    map = merged_map;
                    // Hit rows index the FINAL list, so they shift with it —
                    // the block is never scanned for links, only prepended.
                    for hit in &mut hits {
                        hit.row += n;
                    }
                }
                self.link_hits = hits;
                (lines, map)
            };
        // The hit list is a draw product; an edit may have deleted the target
        // the ordinal pointed at.
        self.clamp_link_cursor();
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
        // Same one-shot contract as `follow_box`: only right after `n`/`N`
        // moved the cursor, never merely because a cursor exists — otherwise
        // every other scroll key looks broken while a link is selected.
        if self.follow_link {
            if let Some(row) = self.link_cursor.and_then(|c| self.link_hits.get(c)).map(|h| h.row) {
                let h = usize::from(area.height).max(1);
                if row < self.preview_scroll {
                    self.preview_scroll = row;
                } else if row >= self.preview_scroll + h {
                    self.preview_scroll = row + 1 - h;
                }
            }
            self.follow_link = false;
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

/// Footer hint tokens for one preview state, in DISPLAY order, each with a drop
/// rank: when the line does not fit, the highest rank goes first and ties break
/// on the later slice position. Rank 0 never drops, so `q quit` survives to the
/// floor and only below that does the terminal clip — which is what the six
/// fixed hint strings used to guarantee, at the cost of a step change between
/// two widths and nothing in between.
type Hints = &'static [(&'static str, u8)];

const HINTS_PREVIEW: Hints = &[
    ("e edit", 3),
    ("j/k spc tick", 4),
    ("n/N link", 2),
    ("r title", 6),
    ("l list", 5),
    ("Up/Dn scroll", 7),
    ("x clear", 8),
    ("q quit", 0),
];

/// While a checkbox cursor is live, `esc drop` is the only way out of it, so it
/// outranks everything but `q quit`.
const HINTS_BOX: Hints = &[
    ("e edit", 3),
    ("j/k spc tick", 4),
    ("esc drop", 1),
    ("r title", 6),
    ("l list", 5),
    ("Up/Dn scroll", 7),
    ("x clear", 8),
    ("q quit", 0),
];

/// While a link cursor is live, opening is the point (`o open`) and `esc drop`
/// is the way out; `x clear` is not offered at all — wiping the note under a
/// live link cursor is not a thing anyone reaches for.
const HINTS_LINK: Hints = &[
    ("e edit", 4),
    ("n/N link", 3),
    ("o open", 1),
    ("esc drop", 2),
    ("r title", 6),
    ("l list", 5),
    ("Up/Dn scroll", 7),
    ("q quit", 0),
];

/// Renders `tokens` into a footer line of at most `width` display COLUMNS,
/// dropping by rank until it fits. Greedy by rank rather than optimal packing:
/// a lower-ranked token that would still have fitted is not re-added, which
/// keeps the rule one sentence long and the output predictable.
fn fit_hints(tokens: Hints, width: usize) -> String {
    let mut keep: Vec<(&str, u8)> = tokens.to_vec();
    loop {
        let line = render_hints(&keep);
        if dwidth(&line) <= width {
            return line;
        }
        let Some(pos) = keep
            .iter()
            .enumerate()
            .filter(|(_, (_, rank))| *rank > 0)
            .max_by_key(|(i, (_, rank))| (*rank, *i))
            .map(|(i, _)| i)
        else {
            return line; // only rank 0 left: let the terminal clip, as before
        };
        keep.remove(pos);
    }
}

/// One leading space, two spaces between tokens — the shape the fixed hint
/// strings had.
fn render_hints(keep: &[(&str, u8)]) -> String {
    let joined: Vec<&str> = keep.iter().map(|(t, _)| *t).collect();
    format!(" {}", joined.join("  "))
}

/// The dim per-agent prompt block rendered above the note: one heading per
/// group, its prompts numbered from 1, a blank line between groups, and a
/// rule at the end. Empty groups and an empty list render nothing at all, so
/// the note keeps the space. There is deliberately no single "Last Prompts"
/// heading — the agent's own label is the more informative one, and two
/// heading levels in a five-row block is noise.
fn prompt_block(
    groups: &[(String, Vec<crate::prompts::Prompt>)],
    width: usize,
) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let head = Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM);
    let mut out: Vec<Line<'static>> = Vec::new();
    for (label, prompts) in groups.iter().filter(|(_, p)| !p.is_empty()) {
        if !out.is_empty() {
            out.push(Line::raw(""));
        }
        out.push(Line::from(Span::styled(truncate_w(label, width), head)));
        for (i, p) in prompts.iter().enumerate() {
            // The number and its separator cost 3 columns.
            let body = truncate_w(&p.text, width.saturating_sub(3));
            out.push(Line::from(Span::styled(format!("{}. {body}", i + 1), dim)));
        }
    }
    if !out.is_empty() {
        out.push(Line::from(Span::styled("─".repeat(width), dim)));
    }
    out
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
    // Shared with `state.rs` (`crate::state::ENV_LOCK`): both modules have
    // tests that mutate the same process-global `HERDR_*` vars, and since
    // every test in the crate compiles into one binary running on parallel
    // threads, a lock private to this module would not serialize against
    // `state.rs`'s tests touching the same vars. See its doc comment there
    // for the poisoned-lock convention.
    use crate::state::ENV_LOCK;

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

    fn prompt(ts: u64, text: &str) -> crate::prompts::Prompt {
        crate::prompts::Prompt { ts, pane: "w1:p5".into(), agent: "claude".into(), text: text.into() }
    }

    fn group(label: &str, texts: &[&str]) -> (String, Vec<crate::prompts::Prompt>) {
        let prompts = texts
            .iter()
            .enumerate()
            .map(|(i, t)| crate::prompts::Prompt {
                ts: (100 - i) as u64,
                pane: "w1:p5".into(),
                agent: "claude".into(),
                text: (*t).into(),
            })
            .collect();
        (label.to_string(), prompts)
    }

    /// The env var `state::config_base()` reads. Tests that drive real
    /// persistence must redirect the MIGRATION SOURCE as well as the store
    /// dir: the tab note and (since the global note got the same one-time
    /// migration) `global.json` are MOVED out of the config layout on first
    /// load, so leaving this pointed at the real profile would relocate the
    /// developer's own notes into a temp dir.
    const CONFIG_BASE_VAR: &str = if cfg!(windows) { "APPDATA" } else { "XDG_CONFIG_HOME" };

    /// Set process env vars, returning their previous values for `restore_env`.
    /// Callers MUST hold `ENV_LOCK` — these are process-global.
    fn swap_env(vars: &[(&str, Option<&std::ffi::OsStr>)]) -> Vec<(String, Option<std::ffi::OsString>)> {
        let mut prev = Vec::new();
        for (k, v) in vars {
            prev.push(((*k).to_string(), std::env::var_os(k)));
            // SAFETY: serialized by ENV_LOCK; every caller restores below.
            unsafe {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
        prev
    }

    fn restore_env(prev: Vec<(String, Option<std::ffi::OsString>)>) {
        for (k, v) in prev {
            // SAFETY: serialized by ENV_LOCK.
            unsafe {
                match v {
                    Some(val) => std::env::set_var(&k, val),
                    None => std::env::remove_var(&k),
                }
            }
        }
    }

    #[test]
    fn preview_renders_the_prompt_block_above_the_note() {
        let mut a = app("## Status\nmid-refactor");
        a.prompts = vec![crate::prompts::PromptGroup {
            pane: "w1:p5".into(),
            prompts: vec![prompt(2, "add the rate limiter"), prompt(1, "why is auth flaky")],
        }];
        let screen = rendered(&mut a, 60, 14);
        assert!(screen.contains("add the rate limiter"), "{screen}");
        let block_at = screen.find("add the rate limiter").unwrap();
        let note_at = screen.find("mid-refactor").unwrap();
        assert!(block_at < note_at, "the block sits above the note: {screen}");
    }

    #[test]
    fn the_prompt_block_is_absent_without_prompts() {
        // No groups -> no heading, no rows, and no trailing rule (the only
        // marker `prompt_block` ever emits with no prompts to number); this
        // fixture's note text has no markdown hr of its own to confuse it.
        let mut a = app("## Status\nmid-refactor");
        assert!(!rendered(&mut a, 60, 14).contains('─'));
    }

    #[test]
    fn the_prompt_block_never_shows_on_the_global_note_or_in_edit_mode() {
        let mut a = app("## Status\nmid-refactor");
        a.prompts =
            vec![crate::prompts::PromptGroup { pane: "w1:p5".into(), prompts: vec![prompt(1, "add the rate limiter")] }];
        a.active = ActiveNote::Global;
        assert!(!rendered(&mut a, 60, 14).contains("add the rate limiter"), "global is not a tab");

        let mut b = app("## Status\nmid-refactor");
        b.prompts =
            vec![crate::prompts::PromptGroup { pane: "w1:p5".into(), prompts: vec![prompt(1, "add the rate limiter")] }];
        b.on_key(key(KeyCode::Char('e')));
        assert!(!rendered(&mut b, 60, 14).contains("add the rate limiter"), "the edit buffer is yours alone");
    }

    #[test]
    fn the_prompt_block_shows_above_the_empty_note_help() {
        // A titled, body-less note is NOT blank, so its file persists, so
        // capture's note-file gate passes and prompts keep accumulating for it.
        // The empty-note branch has to render the block too, or the pane shows
        // nothing but the help forever and capture looks broken.
        let mut a = app("");
        a.note.title = "Auth refactor".into();
        a.prompts =
            vec![crate::prompts::PromptGroup { pane: "w1:p5".into(), prompts: vec![prompt(1, "why is auth flaky")] }];
        let screen = rendered(&mut a, 60, 24);
        assert!(screen.contains("why is auth flaky"), "{screen}");
        assert!(screen.contains("(empty note"), "the quick-start help must survive: {screen}");
        let block_at = screen.find("why is auth flaky").unwrap();
        let help_at = screen.find("(empty note").unwrap();
        assert!(block_at < help_at, "the block sits above the help: {screen}");

        // The showing_tab_note() gate applies on this path exactly as it does
        // on the rendered-note one.
        let mut b = app("");
        b.prompts =
            vec![crate::prompts::PromptGroup { pane: "w1:p5".into(), prompts: vec![prompt(1, "why is auth flaky")] }];
        b.active = ActiveNote::Global;
        assert!(
            !rendered(&mut b, 60, 24).contains("why is auth flaky"),
            "the global note is not a tab, empty or not"
        );
    }

    #[test]
    fn the_empty_note_help_stays_reachable_behind_a_tall_prompt_block() {
        // Four agents give a block far taller than RING + 2, so the branch
        // that used to force preview_scroll = 0 would push the help off
        // screen with no key able to reach it.
        let mut a = app("");
        a.note.title = "Titled but bodyless".into();
        a.prompts = (0..4)
            .map(|i| crate::prompts::PromptGroup {
                pane: format!("w1:p{i}"),
                prompts: (0..3)
                    .map(|j| crate::prompts::Prompt {
                        ts: (i * 10 + j) as u64,
                        pane: format!("w1:p{i}"),
                        agent: "claude".into(),
                        text: format!("prompt {i}-{j}"),
                    })
                    .collect(),
            })
            .collect();
        let _ = rendered(&mut a, 60, 12);
        a.on_key(key(KeyCode::Char('G')));
        let screen = rendered(&mut a, 60, 12);
        assert!(a.preview_scroll > 0, "G must scroll the tall empty-note view: {screen}");
    }

    #[test]
    fn the_checkbox_cursor_ignores_the_prompt_block() {
        // This proves j/k/space resolve against `note.text` (via `box_cursor`
        // / `move_box` / `toggle_box`, none of which read `map`) independently
        // of a prompt block being present — NOT that the provenance map is
        // correctly padded. `the_scroll_follow_accounts_for_the_prompt_block_rows`
        // below is what actually exercises the map's two real consumers (the
        // REVERSED highlight and the follow-scroll arithmetic).
        let mut a = app("[ ] first\n[ ] second");
        a.prompts = vec![crate::prompts::PromptGroup {
            pane: "w1:p5".into(),
            prompts: vec![prompt(2, "add the rate limiter"), prompt(1, "why is auth flaky")],
        }];
        let _ = rendered(&mut a, 60, 14);
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(0));
        a.on_key(key(KeyCode::Char(' ')));
        assert_eq!(a.note.text, "[x] first\n[ ] second", "space hit the note's first box, not a prompt row");
        let _ = rendered(&mut a, 60, 14);
    }

    /// Converts a rendered `Line`'s spans back to plain text, mirroring
    /// `markdown::tests::text`.
    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn prompt_block_heads_each_group_with_its_label() {
        let groups = vec![
            group("HM-54271 Importer", &["add the rate limiter", "why is auth flaky"]),
            group("claude pB", &["run the migration"]),
        ];
        let rows: Vec<String> = prompt_block(&groups, 60).iter().map(line_text).collect();
        let joined = rows.join("\n");
        assert!(joined.contains("HM-54271 Importer"), "{joined}");
        assert!(joined.contains("claude pB"), "{joined}");
        assert!(joined.contains("1. add the rate limiter"), "{joined}");
        assert!(joined.contains("2. why is auth flaky"), "{joined}");
        // The discriminating assertion: group two's FIRST row restarts at 1.
        // Continuous cross-group numbering (the bug this guards against)
        // would render this row "3. run the migration" instead, since it is
        // the third prompt overall.
        assert!(
            joined.contains("1. run the migration"),
            "numbering restarts per group, not continues across them: {joined}"
        );
        assert!(
            rows.iter().position(|r| r.contains("HM-54271")).unwrap()
                < rows.iter().position(|r| r.contains("claude pB")).unwrap(),
            "group order is preserved: {joined}"
        );
        // A blank separator sits between the two groups, and nowhere else —
        // not before the first heading.
        assert_eq!(
            rows.first().map(String::as_str),
            Some("HM-54271 Importer"),
            "no leading blank before the first group: {joined}"
        );
        let heading2 = rows.iter().position(|r| r == "claude pB").unwrap();
        assert_eq!(rows[heading2 - 1], "", "a blank line separates the groups: {joined}");
        assert!(!joined.contains("Last Prompts"), "the single heading is gone: {joined}");
    }

    #[test]
    fn prompt_block_is_empty_without_groups() {
        assert!(prompt_block(&[], 60).is_empty());
        assert!(prompt_block(&[("solo".into(), vec![])], 60).is_empty(), "a group with no prompts renders nothing");
    }

    #[test]
    fn prompt_block_truncates_labels_and_bodies_by_display_columns() {
        // Storage truncates by CHAR count, so a 120-char CJK prompt is ~240
        // columns; only this render-side truncation keeps it in the pane. The
        // heading is user-supplied too and gets the same treatment.
        let groups = vec![group(&"文".repeat(80), &[&"文".repeat(80)])];
        for width in [12usize, 30, 60] {
            for line in prompt_block(&groups, width) {
                let text = line_text(&line);
                assert!(dwidth(&text) <= width, "row {text:?} is {} cols, want <= {width}", dwidth(&text));
            }
        }
    }

    #[test]
    fn preview_renders_grouped_prompts_above_the_note() {
        let mut a = app("## Status\nmid-refactor");
        a.prompts = vec![crate::prompts::PromptGroup {
            pane: "w1:p5".into(),
            prompts: vec![crate::prompts::Prompt {
                ts: 2, pane: "w1:p5".into(), agent: "claude".into(), text: "add the rate limiter".into(),
            }],
        }];
        let screen = rendered(&mut a, 60, 14);
        assert!(screen.contains("add the rate limiter"), "{screen}");
        let block_at = screen.find("add the rate limiter").unwrap();
        let note_at = screen.find("mid-refactor").unwrap();
        assert!(block_at < note_at, "the block sits above the note: {screen}");
    }

    #[test]
    fn long_prompts_are_truncated_to_the_pane_width() {
        // A smoke test that the whole draw path does not overflow the pane —
        // NOT evidence that `prompt_block` truncates: `rendered()`'s
        // TestBackend buffer is exactly `w` cells wide and `Paragraph` clips
        // at the edge, so this would pass even with no truncation at all.
        // `prompt_block_truncates_labels_and_bodies_by_display_columns` above
        // is what actually proves the truncation.
        let mut a = app("## Status\nmid-refactor");
        a.prompts =
            vec![crate::prompts::PromptGroup { pane: "w1:p5".into(), prompts: vec![prompt(1, &"z".repeat(200))] }];
        let screen = rendered(&mut a, 30, 14);
        for line in screen.lines() {
            assert!(dwidth(line.trim_end()) <= 30, "row overflows the pane: {line:?}");
        }
    }

    #[test]
    fn the_scroll_follow_accounts_for_the_prompt_block_rows() {
        // The block prepends rows to BOTH `lines` and `map`, so the row index
        // the follow computes is a merged index. If the map padding were
        // dropped or mis-sized, the follow would land short by exactly the
        // block's height. Same note, same cursor, block vs no block.
        let text: String = (0..40).map(|i| format!("[ ] item {i}\n")).collect();

        let mut bare = app(&text);
        for _ in 0..40 {
            bare.on_key(key(KeyCode::Char('j')));
        }
        let _ = rendered(&mut bare, 60, 14);

        let mut with_block = app(&text);
        with_block.prompts = vec![crate::prompts::PromptGroup {
            pane: "w1:p5".into(),
            prompts: vec![prompt(2, "add the rate limiter"), prompt(1, "why is auth flaky")],
        }];
        for _ in 0..40 {
            with_block.on_key(key(KeyCode::Char('j')));
        }
        let _ = rendered(&mut with_block, 60, 14);

        // Matches draw_preview's own `text_w` derivation: area.width - 1 for
        // the scrollbar column, with the 60-wide `rendered()` call above.
        let text_w = usize::from(60u16).saturating_sub(1).max(1);
        let block_rows = prompt_block(&with_block.labelled_prompts(), text_w).len();
        assert!(block_rows > 0, "the fixture must actually produce a block");
        assert_eq!(
            with_block.preview_scroll,
            bare.preview_scroll + block_rows,
            "the follow must offset by exactly the block's height"
        );
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
    fn typing_a_title_freezes_it_and_clearing_re_enables_auto() {
        let mut a = app("body");
        assert!(a.note.title_auto, "an untitled note starts derivable");
        a.on_key(key(KeyCode::Char('r')));
        for c in "HM-1".chars() {
            a.on_key(key(KeyCode::Char(c)));
        }
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.note.title, "HM-1");
        assert!(!a.note.title_auto, "a typed title is frozen");

        a.on_key(key(KeyCode::Char('r')));
        for _ in 0..8 {
            a.on_key(key(KeyCode::Backspace));
        }
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.note.title, "");
        assert!(a.note.title_auto, "clearing hands it back to auto");
    }

    #[test]
    fn escaping_the_title_editor_leaves_the_flag_alone() {
        let mut a = app("body");
        a.note.title = "Mine".into();
        a.note.title_auto = false;
        a.on_key(key(KeyCode::Char('r')));
        a.on_key(key(KeyCode::Char('x')));
        a.on_key(key(KeyCode::Esc));
        assert_eq!(a.note.title, "Mine");
        assert!(!a.note.title_auto);
    }

    fn info(agent: &str, title: Option<&str>, cwd: Option<&str>) -> PaneInfo {
        PaneInfo {
            agent: agent.into(),
            tab_id: "wD:t2".into(),
            title: title.map(|s| s.to_string()),
            cwd: cwd.map(|s| s.to_string()),
            label: None,
        }
    }

    #[test]
    fn pick_title_prefers_a_meaningful_terminal_title() {
        let p = info("claude", Some("HM-54271 Importer"), Some("C:\\repo"));
        assert_eq!(
            pick_title(Some(&p), Some("some-branch"), Some("a prompt")).as_deref(),
            Some("HM-54271 Importer")
        );
    }

    #[test]
    fn pick_title_falls_through_to_the_branch_then_the_prompt() {
        // Generic title -> branch wins.
        let generic = info("claude", Some("Claude Code"), Some("C:\\repo"));
        assert_eq!(
            pick_title(Some(&generic), Some("20260727-team-solutions"), Some("a prompt")).as_deref(),
            Some("20260727-team-solutions")
        );
        // No branch either -> the prompt.
        assert_eq!(pick_title(Some(&generic), None, Some("a prompt")).as_deref(), Some("a prompt"));
        // Nothing at all.
        assert_eq!(pick_title(Some(&generic), None, None), None);
        // No agent pane in the tab: branch and prompt still work.
        assert_eq!(pick_title(None, Some("br"), Some("a prompt")).as_deref(), Some("br"));
        assert_eq!(pick_title(None, None, Some("a prompt")).as_deref(), Some("a prompt"));
        assert_eq!(pick_title(None, None, None), None);
    }

    #[test]
    fn pick_title_rejects_a_detached_head_and_blank_sources() {
        let generic = info("claude", Some("Claude Code"), Some("C:\\repo"));
        assert_eq!(
            pick_title(Some(&generic), Some("HEAD"), Some("a prompt")).as_deref(),
            Some("a prompt"),
            "a detached HEAD is not a name"
        );
        assert_eq!(pick_title(Some(&generic), Some("   "), Some("a prompt")).as_deref(), Some("a prompt"));
        assert_eq!(pick_title(Some(&generic), Some("br"), Some("   ")).as_deref(), Some("br"));
    }

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
        // While a title is still empty, `maybe_autotitle` asks the branch
        // again on every heartbeat (a note whose title is already set never
        // asks at all — see the no-demotion rule there). Without caching the
        // success, an untitled tab with no pane candidate yet would spawn
        // `git` again on every one of those beats, not just the first.
        let mut a = app("body");
        let cwd = std::env::current_dir().unwrap().display().to_string();
        let first = a.git_branch(&cwd);
        assert!(first.is_some(), "the crate root is a git repo");
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

    #[test]
    fn maybe_autotitle_derives_from_the_oldest_prompt_and_is_gated_by_title_state_and_active_note() {
        // The `app()` helper builds with `persist = false`, so
        // `maybe_autotitle`'s FIRST guard (`if !self.persist`) returns before
        // `title_auto`/`title` are ever read — a test built on it would pass
        // whether or not the rest of the function does anything at all.
        // Real coverage needs a real `persist = true` App, following
        // `prompts_load_at_construction_and_on_every_global_toggle`'s harness
        // exactly: a temp store dir, `HERDR_*` env under `ENV_LOCK`, and a
        // dead socket so `pane_index()` returns `None` — with sources 1 and 2
        // (terminal title, git branch) unavailable, a captured prompt is the
        // only reachable source, which is exactly what this exercises.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("notes-autotitle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config-base");
        std::fs::create_dir_all(&cfg).unwrap();

        // The note needs a BODY: an empty one is blank, and a blank note is
        // never auto-titled (it would stop `persist_at` deleting it and leave
        // an orphan file — see
        // `autotitle_never_titles_a_blank_note_so_it_stays_deletable`). This
        // fixture used to be `Note::default()`, which `persist_at` declines to
        // write at all, so every case below ran against a blank buffer.
        state::persist_at(
            &dir.join("w1_t1.json"),
            &Note { text: "a real body".into(), ..Default::default() },
            "w1:t1",
            100,
        );
        state::persist_at(
            &dir.join("global.json"),
            &Note { text: "GLOBAL BODY".into(), ..Default::default() },
            "",
            100,
        );
        let key = state::id_key("w1:t1").unwrap();
        let pane_key = state::id_key("w1:p5").unwrap();
        crate::prompts::append_at(
            &crate::prompts::prompts_file(&dir, &key, &pane_key),
            crate::prompts::Prompt {
                ts: 7,
                pane: "w1:p5".into(),
                agent: "claude".into(),
                text: "why is auth flaky".into(),
            },
        );

        let sock = dir.join("no-such.sock");
        let prev = swap_env(&[
            ("HERDR_PLUGIN_STATE_DIR", Some(dir.as_os_str())),
            ("HERDR_TAB_ID", Some(std::ffi::OsStr::new("w1:t1"))),
            ("HERDR_PANE_ID", None),
            ("HERDR_SOCKET_PATH", Some(sock.as_os_str())),
            (CONFIG_BASE_VAR, Some(cfg.as_os_str())),
        ]);

        let mut a = App::new();
        assert!(a.note.title.trim().is_empty(), "fixture loads untitled");
        assert!(a.note.title_auto, "fixture loads auto");
        assert!(!a.dirty, "construction alone must not dirty the note");

        // Case 1: untitled + auto + a prompt on disk -> derived from it.
        a.maybe_autotitle(None);
        assert_eq!(a.note.title, "why is auth flaky");
        assert!(a.note.title_auto, "title_auto stays true — it records derivation, not pending-ness");
        assert!(a.dirty, "a derived title must autosave, same as any other edit");

        // Case 2: the title IS re-derived every beat now — this is not a
        // "title is set, skip" guard. With `index: None` there is no
        // pane-derived candidate to replace it with (an existing title may
        // only be replaced by one — see `maybe_autotitle`), so nothing is
        // written and the unchanged value must not touch the note either way.
        a.dirty = false;
        a.maybe_autotitle(None);
        assert_eq!(a.note.title, "why is auth flaky", "no pane candidate -> the existing title survives");
        assert!(!a.dirty, "no mutation means no new dirty flag");

        // Case 3: `title_auto = false` (a manually typed title) is untouched
        // even with an empty title — the FIRST guard catches it before the
        // prompt is ever looked at.
        a.note.title.clear();
        a.note.title_auto = false;
        a.dirty = false;
        a.maybe_autotitle(None);
        assert!(a.note.title.trim().is_empty(), "a manual (title_auto=false) note is never auto-titled");
        assert!(!a.dirty);

        // Case 4: the pane showing the GLOBAL note is untouched even when
        // untitled and auto — the global note is not a tab, so it must never
        // pick up a tab's prompt.
        a.note.title_auto = true;
        a.active = ActiveNote::Global;
        a.dirty = false;
        a.maybe_autotitle(None);
        assert!(a.note.title.trim().is_empty(), "the global note is never auto-titled from tab sources");
        assert!(!a.dirty);

        restore_env(prev);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn maybe_autotitle_fills_an_empty_title_from_the_oldest_prompt_and_then_refuses_to_replace_it() {
        // Same harness as
        // `maybe_autotitle_derives_from_the_oldest_prompt_and_is_gated_by_title_state_and_active_note`:
        // temp store, HERDR_* under ENV_LOCK, dead socket so only the prompt
        // source is reachable — there is no `pane.list` anywhere in this
        // test, so `index` is always `None` and a pane-derived candidate
        // (source 1) never exists here. That is deliberate: per the
        // no-demotion rule (`maybe_autotitle`), the branch and the captured
        // prompt may only FILL a title that is still empty, never replace
        // one that is already set — only a pane-derived candidate can do
        // that (covered by the `maybe_autotitle_*_demotes_*`/`*_follows_*`
        // tests below, which supply a real `PaneIndex`).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("notes-autotitle-rederive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config-base");
        std::fs::create_dir_all(&cfg).unwrap();

        state::persist_at(
            &dir.join("w1_t1.json"),
            &Note { text: "a real body".into(), ..Default::default() },
            "w1:t1",
            100,
        );
        let key = state::id_key("w1:t1").unwrap();
        let older_pane_key = state::id_key("w1:p5").unwrap();
        let older_file = crate::prompts::prompts_file(&dir, &key, &older_pane_key);
        crate::prompts::append_at(
            &older_file,
            crate::prompts::Prompt {
                ts: 7,
                pane: "w1:p5".into(),
                agent: "claude".into(),
                text: "why is auth flaky".into(),
            },
        );

        let sock = dir.join("no-such.sock");
        let prev = swap_env(&[
            ("HERDR_PLUGIN_STATE_DIR", Some(dir.as_os_str())),
            ("HERDR_TAB_ID", Some(std::ffi::OsStr::new("w1:t1"))),
            ("HERDR_PANE_ID", None),
            ("HERDR_SOCKET_PATH", Some(sock.as_os_str())),
            (CONFIG_BASE_VAR, Some(cfg.as_os_str())),
        ]);

        let mut a = App::new();
        assert!(a.note.title.trim().is_empty(), "fixture loads untitled");
        assert!(a.note.title_auto, "fixture loads auto");

        // 1. First beat: the title is still empty, so the full chain runs
        // and fills it from the oldest prompt; `dirty` is set.
        a.load_prompts();
        a.maybe_autotitle(None);
        assert_eq!(a.note.title, "why is auth flaky");
        assert!(a.dirty, "a derived title must autosave");

        // 2. Clear `dirty`, beat again with nothing changed: the title is
        // now set, `index` is `None` (no pane candidate), and `dirty` must
        // STAY false — otherwise every heartbeat dirties the note, the 2s
        // autosave fires forever, `updated` keeps bumping and the header age
        // resets to `just now` on a loop.
        a.dirty = false;
        a.load_prompts();
        a.maybe_autotitle(None);
        assert_eq!(a.note.title, "why is auth flaky", "no pane candidate -> the existing title survives");
        assert!(!a.dirty, "no candidate means no write, so no new dirty flag");

        // 3. Append a NEWER prompt whose text differs, in a DIFFERENT pane's
        // file. The title must stay put regardless — with the title already
        // set and no pane candidate reachable, the prompt chain is never
        // even consulted (see the no-demotion rule in `maybe_autotitle`).
        let newer_pane_key = state::id_key("w1:p6").unwrap();
        let newer_file = crate::prompts::prompts_file(&dir, &key, &newer_pane_key);
        crate::prompts::append_at(
            &newer_file,
            crate::prompts::Prompt {
                ts: 50,
                pane: "w1:p6".into(),
                agent: "claude".into(),
                text: "add the rate limiter".into(),
            },
        );
        a.dirty = false;
        a.load_prompts();
        a.maybe_autotitle(None);
        assert_eq!(a.note.title, "why is auth flaky", "an existing title is immune to any prompt-only change");
        assert!(!a.dirty);

        // 4. Remove the older prompt file so the newer one becomes the
        // oldest SURVIVING prompt across the tab — under the old (pre-
        // no-demotion) behavior this would have replaced the title. It must
        // NOT: an existing title may only be replaced by a pane-derived
        // candidate (source 1), never by the branch or a captured prompt,
        // and there is still no pane candidate here (`index: None`).
        std::fs::remove_file(&older_file).unwrap();
        a.load_prompts();
        a.maybe_autotitle(None);
        assert_eq!(a.note.title, "why is auth flaky", "the prompt chain can fill an empty title, never replace one");
        assert!(!a.dirty, "no demotion means no autosave churn");

        restore_env(prev);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn maybe_autotitle_fills_a_whitespace_only_title_the_same_as_an_empty_one() {
        // `state::parse`'s `title_auto` missing-field default is
        // `title.trim().is_empty()` (`src/state.rs`), so a whitespace-only
        // title from a hand-edited or legacy file reads as auto/derivable.
        // `maybe_autotitle`'s empty-vs-set split has to agree: a bare
        // `.is_empty()` there would read "   " as already SET, permanently
        // blocking the branch/prompt from ever filling it (only a
        // pane-derived candidate could, and none exists here).
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = swap_env(&[("HERDR_TAB_ID", Some(std::ffi::OsStr::new("w1:t1")))]);

        let mut a = app("a real body");
        a.persist = true;
        a.note.title = "   ".into();
        a.note.title_auto = true;
        a.prompts = vec![crate::prompts::PromptGroup {
            pane: "w1:p5".into(),
            prompts: vec![crate::prompts::Prompt {
                ts: 1,
                pane: "w1:p5".into(),
                agent: "claude".into(),
                text: "why is auth flaky".into(),
            }],
        }];

        a.maybe_autotitle(None);
        assert_eq!(a.note.title, "why is auth flaky", "a whitespace-only title must still be fillable");
        assert!(a.dirty, "filling a title is a real edit");

        restore_env(prev);
    }

    #[test]
    fn maybe_autotitle_never_demotes_an_existing_title_when_the_index_is_lost() {
        // `heartbeat` collapses `index` to `None` on any transient
        // `pane.list` failure. Without the no-demotion rule, re-derive would
        // fall all the way through to the captured prompt on that one beat —
        // discarding a good pane-derived title — and flip back the next time
        // the socket answers. Only the tab-id env var matters here: no disk,
        // no socket, `index` and `self.prompts` are supplied directly.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = swap_env(&[("HERDR_TAB_ID", Some(std::ffi::OsStr::new("w1:t1")))]);

        let mut a = app("a real body");
        a.persist = true;
        a.note.title = "auth-refactor".into();
        a.note.title_auto = true;
        a.prompts = vec![crate::prompts::PromptGroup {
            pane: "w1:p5".into(),
            prompts: vec![crate::prompts::Prompt {
                ts: 1,
                pane: "w1:p5".into(),
                agent: "claude".into(),
                text: "why is auth flaky".into(),
            }],
        }];

        a.maybe_autotitle(None);
        assert_eq!(a.note.title, "auth-refactor", "a lost index must not demote an existing title");
        assert!(!a.dirty, "no demotion means no autosave churn");

        restore_env(prev);
    }

    #[test]
    fn maybe_autotitle_keeps_a_label_derived_title_when_the_pane_goes_generic() {
        // The same pane, still present in the index, but its terminal title
        // has gone idle-generic (`Claude Code`, rejected by `nice_title`) and
        // it carries no label. Without the no-demotion rule this falls
        // through to the branch/prompt chain and the label-derived title is
        // lost until the agent goes busy again.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = swap_env(&[("HERDR_TAB_ID", Some(std::ffi::OsStr::new("w1:t1")))]);

        let mut idx = PaneIndex::new();
        idx.insert("w1:p5".into(), PaneInfo {
            agent: "claude".into(),
            tab_id: "w1:t1".into(),
            title: Some("Claude Code".into()),
            cwd: None,
            label: None,
        });

        let mut a = app("a real body");
        a.persist = true;
        a.note.title = "auth-refactor".into(); // previously derived from the pane's label
        a.note.title_auto = true;
        a.prompts = vec![crate::prompts::PromptGroup {
            pane: "w1:p5".into(),
            prompts: vec![crate::prompts::Prompt {
                ts: 1,
                pane: "w1:p5".into(),
                agent: "claude".into(),
                text: "why is auth flaky".into(),
            }],
        }];

        a.maybe_autotitle(Some(&idx));
        assert_eq!(a.note.title, "auth-refactor", "a generic terminal title must not demote a label-derived title");
        assert!(!a.dirty);

        restore_env(prev);
    }

    #[test]
    fn maybe_autotitle_still_follows_a_new_pane_label_over_an_existing_title() {
        // The case the user actually asked for: a note titled from the
        // branch (or a prompt), and a pane label subsequently appears. The
        // no-demotion rule only blocks the branch/prompt from REPLACING an
        // existing title — a pane-derived candidate (source 1) must still
        // win, on the very next beat.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = swap_env(&[("HERDR_TAB_ID", Some(std::ffi::OsStr::new("w1:t1")))]);

        let mut idx = PaneIndex::new();
        idx.insert("w1:p5".into(), PaneInfo {
            agent: "claude".into(),
            tab_id: "w1:t1".into(),
            title: Some("Claude Code".into()),
            cwd: None,
            label: Some("checkout-tests".into()),
        });

        let mut a = app("a real body");
        a.persist = true;
        a.note.title = "20260728-team-solutions".into(); // previously derived from the branch
        a.note.title_auto = true;

        a.maybe_autotitle(Some(&idx));
        assert_eq!(a.note.title, "checkout-tests", "a new pane label still replaces an existing non-pane title");
        assert!(a.dirty, "a genuine pane-derived change must autosave");

        restore_env(prev);
    }

    #[test]
    fn maybe_autotitle_does_not_dirty_when_the_pane_label_still_matches_the_title() {
        // The compare-before-write guard (`title != self.note.title`) is
        // what stops a labelled pane with a STABLE label from `touch()`ing
        // the note on every single heartbeat forever — source 1 resolves to
        // the same string every beat, so without the guard `updated` would
        // keep bumping and the header age would reset to `just now` on a
        // loop. None of the other autotitle tests reach this: they either
        // start from an empty title (nothing to compare against) or change
        // the pane's candidate (so the guard sees a genuine difference) —
        // this is the only one where a live, reachable `Some` candidate
        // equals the note's current title.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = swap_env(&[("HERDR_TAB_ID", Some(std::ffi::OsStr::new("w1:t1")))]);

        let mut idx = PaneIndex::new();
        idx.insert("w1:p5".into(), PaneInfo {
            agent: "claude".into(),
            tab_id: "w1:t1".into(),
            title: None,
            cwd: None,
            label: Some("auth-refactor".into()),
        });

        let mut a = app("a real body");
        a.persist = true;
        a.note.title = "auth-refactor".into(); // already derived from this very label
        a.note.title_auto = true;

        a.maybe_autotitle(Some(&idx));
        assert_eq!(a.note.title, "auth-refactor");
        assert!(!a.dirty, "an unchanged pane-derived candidate must not touch() the note");

        restore_env(prev);
    }

    #[test]
    fn one_heartbeat_loads_prompts_and_derives_a_title_from_one_shared_index() {
        // `refresh_prompts` and `maybe_autotitle` used to fetch `pane.list`
        // independently, every 5s, forever — for a tab that never resolves a
        // title (no agent pane, or no prompts yet) that doubling is permanent,
        // not one heartbeat. The heartbeat now fetches at most ONE snapshot
        // and threads it into both. The round-trip COUNT is not observable
        // from here (the socket is deliberately dead); what this pins is that
        // the threaded path still does both jobs in a single beat.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("notes-heartbeat-share-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config-base");
        std::fs::create_dir_all(&cfg).unwrap();

        let key = state::id_key("w1:t1").unwrap();
        state::persist_at(
            &dir.join(format!("{key}.json")),
            &Note { text: "real body".into(), ..Default::default() },
            "w1:t1",
            100,
        );
        let pane_key = state::id_key("w1:p5").unwrap();
        crate::prompts::append_at(
            &crate::prompts::prompts_file(&dir, &key, &pane_key),
            crate::prompts::Prompt {
                ts: 7,
                pane: "w1:p5".into(),
                agent: "claude".into(),
                text: "why is auth flaky".into(),
            },
        );

        let sock = dir.join("no-such.sock");
        let prev = swap_env(&[
            ("HERDR_PLUGIN_STATE_DIR", Some(dir.as_os_str())),
            ("HERDR_TAB_ID", Some(std::ffi::OsStr::new("w1:t1"))),
            ("HERDR_PANE_ID", None),
            ("HERDR_SOCKET_PATH", Some(sock.as_os_str())),
            (CONFIG_BASE_VAR, Some(cfg.as_os_str())),
        ]);

        let mut a = App::new();
        a.note.title.clear();
        a.note.title_auto = true;
        a.prompts.clear();
        a.prompt_labels.clear();
        a.last_beat = Instant::now().checked_sub(HEARTBEAT_EVERY * 2).unwrap();
        a.heartbeat();
        assert_eq!(a.prompts.len(), 1, "the beat reloaded the prompt block");
        assert_eq!(a.prompt_labels.len(), a.prompts.len(), "and labelled it (offline fallback)");
        assert_eq!(a.prompt_labels[0], "claude p5", "no socket -> {{agent}} {{pane-suffix}}");
        assert_eq!(a.note.title, "why is auth flaky", "the same beat derived the title");

        restore_env(prev);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn autotitle_never_titles_a_blank_note_so_it_stays_deletable() {
        // Deriving a title into a note with NOTHING in it makes it non-blank,
        // so `persist_at`'s delete rule stops firing and every tab that merely
        // toggled Notes on leaves a `{"text":"","title":"main"}` orphan behind
        // — permanently, because tab ids are never reused. Same harness as the
        // gate test below: temp store, dead socket, one prompt on disk as the
        // only reachable title source.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("notes-autotitle-blank-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config-base");
        std::fs::create_dir_all(&cfg).unwrap();

        let key = state::id_key("w1:t1").unwrap();
        let pane_key = state::id_key("w1:p5").unwrap();
        crate::prompts::append_at(
            &crate::prompts::prompts_file(&dir, &key, &pane_key),
            crate::prompts::Prompt {
                ts: 7,
                pane: "w1:p5".into(),
                agent: "claude".into(),
                text: "why is auth flaky".into(),
            },
        );

        let sock = dir.join("no-such.sock");
        let prev = swap_env(&[
            ("HERDR_PLUGIN_STATE_DIR", Some(dir.as_os_str())),
            ("HERDR_TAB_ID", Some(std::ffi::OsStr::new("w1:t1"))),
            ("HERDR_PANE_ID", None),
            ("HERDR_SOCKET_PATH", Some(sock.as_os_str())),
            (CONFIG_BASE_VAR, Some(cfg.as_os_str())),
        ]);

        let note_file = dir.join(format!("{key}.json"));
        let mut a = App::new();
        assert!(state::is_blank(&a.note), "fixture: nothing typed, no title");
        assert!(a.note.title_auto);

        a.maybe_autotitle(None);
        assert!(
            a.note.title.trim().is_empty(),
            "an empty note must not be auto-titled — the title alone would keep the file alive"
        );
        assert!(!a.dirty, "no derivation means nothing to autosave");
        a.save();
        assert!(!note_file.exists(), "an untouched tab must leave NO note file on disk");

        // `is_blank` also matches the pristine seed template exactly, so a
        // seeded-but-untyped note is deletable too and must stay untitled.
        a.note.text = crate::template::DEFAULT.to_string();
        a.maybe_autotitle(None);
        assert!(a.note.title.trim().is_empty(), "the pristine seed template counts as blank");
        a.save();
        assert!(!note_file.exists(), "a seeded-but-untyped note still writes no file");

        // The moment there IS something in the note, the title resolves.
        a.note.text = "actual work".into();
        a.maybe_autotitle(None);
        assert_eq!(a.note.title, "why is auth flaky", "a note with content IS titled");
        a.save();
        assert!(note_file.exists(), "and now it persists");

        restore_env(prev);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pick_agent_pane_skips_the_synthetic_usage_pane() {
        // herdr reports a synthetic `usage` pane carrying a real `tab_id`.
        // `build_tab_index` already skips it; picking it here would make ITS
        // terminal title source 1 and — worse — ITS cwd the one cwd the tab
        // ever gets to spend on `git rev-parse` (`git_tried` caches the
        // result once the spawn returns, so the wrong cwd would permanently
        // consume the single attempt).
        let mut idx = PaneIndex::new();
        idx.insert("w1:p1".into(), PaneInfo {
            agent: "usage".into(),
            tab_id: "w1:t1".into(),
            title: Some("Usage".into()),
            cwd: Some("C:\\wrong".into()),
            label: None,
        });
        idx.insert("w1:p3".into(), PaneInfo {
            agent: "claude".into(),
            tab_id: "w1:t1".into(),
            title: Some("Real Work".into()),
            cwd: Some("C:\\repo".into()),
            label: None,
        });
        let p = pick_agent_pane(&idx, "w1:t1").expect("the real agent pane is still a candidate");
        assert_eq!(p.title.as_deref(), Some("Real Work"), "the usage pane sorts lower but must be skipped");
        assert_eq!(p.cwd.as_deref(), Some("C:\\repo"), "and so must its cwd");

        // A tab holding ONLY a usage pane has no agent pane at all.
        let mut only_usage = PaneIndex::new();
        only_usage.insert("w1:p1".into(), PaneInfo {
            agent: "usage".into(),
            tab_id: "w1:t1".into(),
            title: Some("Usage".into()),
            cwd: Some("C:\\wrong".into()),
            label: None,
        });
        assert!(pick_agent_pane(&only_usage, "w1:t1").is_none());
    }

    #[test]
    fn pick_agent_pane_is_deterministic_by_lowest_pane_id() {
        // `PaneIndex` is a `HashMap`, so before this fix, picking a tab's
        // agent pane via `.values().find(...)` visited panes in whatever
        // order the hash happened to put them in — arbitrary, and different
        // per process, on exactly the tab shape this feature targets (two or
        // more agent panes in one tab). Demonstrated empirically below rather
        // than asserted on faith: rebuilding the SAME entries many times and
        // taking the naive first match disagrees with the lowest-pane-id
        // choice across enough trials, in this very run — so a test pinned to
        // one title would have failed against the old code intermittently,
        // not passed by luck of a single build.
        let build = || {
            let mut idx = PaneIndex::new();
            idx.insert("w1:p9".into(), PaneInfo {
                agent: "claude".into(),
                tab_id: "w1:t1".into(),
                title: Some("Zeta".into()),
                cwd: Some("C:\\zeta".into()),
                label: None,
            });
            idx.insert("w1:p2".into(), PaneInfo {
                agent: "codex".into(),
                tab_id: "w1:t1".into(),
                title: Some("Alpha".into()),
                cwd: Some("C:\\alpha".into()),
                label: None,
            });
            idx.insert("w1:p5".into(), PaneInfo {
                agent: "claude".into(),
                tab_id: "w1:t1".into(),
                title: Some("Mid".into()),
                cwd: Some("C:\\mid".into()),
                label: None,
            });
            // A pane on a DIFFERENT tab, and a bare shell pane (no agent yet)
            // on the SAME tab: neither may ever be picked.
            idx.insert("w1:p1".into(), PaneInfo {
                agent: "claude".into(),
                tab_id: "w1:t9".into(),
                title: Some("Other tab".into()),
                cwd: None,
                label: None,
            });
            idx.insert("w1:p0".into(), PaneInfo {
                agent: String::new(),
                tab_id: "w1:t1".into(),
                title: Some("Shell".into()),
                cwd: None,
                label: None,
            });
            idx
        };

        let mut naive_titles = std::collections::HashSet::new();
        for _ in 0..300 {
            let idx = build();
            let naive = idx.values().find(|p| p.tab_id == "w1:t1" && !p.agent.trim().is_empty());
            naive_titles.insert(naive.and_then(|p| p.title.clone()));
        }
        assert!(
            naive_titles.len() > 1,
            "sanity check: the naive `.values().find` selection must actually vary across \
             HashMap instances for this to be a real regression test rather than a coincidence \
             — got {naive_titles:?}"
        );

        for _ in 0..300 {
            let idx = build();
            let picked = pick_agent_pane(&idx, "w1:t1").expect("a candidate exists");
            assert_eq!(picked.title.as_deref(), Some("Alpha"), "lowest pane id (w1:p2) wins, every trial");
        }
    }

    #[test]
    fn pick_agent_pane_prefers_a_labelled_pane_over_a_lower_id() {
        // The tab shape this phase targets: one idle, unlabelled agent pane
        // with a LOWER id than the pane the user deliberately renamed. Picking
        // by id alone returns the idle one, whose terminal title is the generic
        // `Claude Code` — `nice_title` rejects it, source 1 misses, and the
        // no-demotion rule then locks the note to the git branch forever, so
        // the rename can never reach the note on any later beat.
        let mut idx = PaneIndex::new();
        idx.insert("wF:p3".into(), PaneInfo {
            agent: "claude".into(),
            tab_id: "wF:t1".into(),
            title: Some("Claude Code".into()),
            cwd: Some("C:\\repo".into()),
            label: None,
        });
        idx.insert("wF:p7".into(), PaneInfo {
            agent: "claude".into(),
            tab_id: "wF:t1".into(),
            title: Some("Claude Code".into()),
            cwd: Some("C:\\repo".into()),
            label: Some("auth-refactor".into()),
        });
        // Many trials: `PaneIndex` is a `HashMap`, so a selection that happened
        // to be right once must be right every time.
        for _ in 0..100 {
            let p = pick_agent_pane(&idx, "wF:t1").expect("a candidate exists");
            assert_eq!(
                p.nice_title().as_deref(),
                Some("auth-refactor"),
                "the pane the user NAMED wins over the lower id"
            );
        }
        // A label that is only whitespace is not a label — same as `nice_title`
        // treats it — so the lowest id still wins.
        idx.insert("wF:p7".into(), PaneInfo {
            agent: "claude".into(),
            tab_id: "wF:t1".into(),
            title: Some("Claude Code".into()),
            cwd: Some("C:\\repo".into()),
            label: Some("   ".into()),
        });
        let p = pick_agent_pane(&idx, "wF:t1").expect("a candidate exists");
        assert_eq!(p.label.as_deref(), None, "blank label does not count as labelled");
    }

    #[test]
    fn pick_agent_pane_breaks_a_two_label_tie_by_pane_id() {
        // Two labelled panes: the label is the PRIMARY key, but determinism
        // still comes from the pane id, so the same tab state always yields the
        // same title/cwd across processes.
        let build = || {
            let mut idx = PaneIndex::new();
            idx.insert("wF:p9".into(), PaneInfo {
                agent: "claude".into(),
                tab_id: "wF:t1".into(),
                title: None,
                cwd: Some("C:\\zeta".into()),
                label: Some("zeta-work".into()),
            });
            idx.insert("wF:p4".into(), PaneInfo {
                agent: "codex".into(),
                tab_id: "wF:t1".into(),
                title: None,
                cwd: Some("C:\\alpha".into()),
                label: Some("alpha-work".into()),
            });
            idx
        };
        for _ in 0..200 {
            let idx = build();
            let p = pick_agent_pane(&idx, "wF:t1").expect("a candidate exists");
            assert_eq!(p.nice_title().as_deref(), Some("alpha-work"), "lowest id among labelled panes");
        }
    }

    #[test]
    fn pick_agent_pane_still_takes_the_lowest_id_when_nothing_is_labelled() {
        // The pre-label behavior, unchanged: no labels anywhere means the
        // tiebreak IS the selection.
        let build = || {
            let mut idx = PaneIndex::new();
            for (id, title) in [("wF:p8", "Zeta"), ("wF:p2", "Alpha"), ("wF:p5", "Mid")] {
                idx.insert(id.into(), PaneInfo {
                    agent: "claude".into(),
                    tab_id: "wF:t1".into(),
                    title: Some(title.into()),
                    cwd: None,
                    label: None,
                });
            }
            idx
        };
        for _ in 0..200 {
            let idx = build();
            let p = pick_agent_pane(&idx, "wF:t1").expect("a candidate exists");
            assert_eq!(p.title.as_deref(), Some("Alpha"), "lowest pane id, every trial");
        }
    }

    #[test]
    fn pick_agent_pane_skips_a_labelled_usage_pane() {
        // The `usage` skip must survive the label preference: herdr's synthetic
        // pane carrying a label (or a labelled pane id happening to sort low)
        // must never become source 1 or spend the tab's one `git rev-parse`.
        let mut idx = PaneIndex::new();
        idx.insert("wF:p1".into(), PaneInfo {
            agent: "usage".into(),
            tab_id: "wF:t1".into(),
            title: Some("Usage".into()),
            cwd: Some("C:\\wrong".into()),
            label: Some("usage-pane".into()),
        });
        idx.insert("wF:p6".into(), PaneInfo {
            agent: "claude".into(),
            tab_id: "wF:t1".into(),
            title: Some("Real Work".into()),
            cwd: Some("C:\\repo".into()),
            label: None,
        });
        for _ in 0..100 {
            let p = pick_agent_pane(&idx, "wF:t1").expect("the real agent pane is still a candidate");
            assert_eq!(p.nice_title().as_deref(), Some("Real Work"), "a labelled usage pane is still skipped");
            assert_eq!(p.cwd.as_deref(), Some("C:\\repo"), "and so is its cwd");
        }
    }

    #[test]
    fn autotitle_uses_the_oldest_surviving_prompt() {
        // The ring holds RING, so the genuinely-first prompt is gone after
        // enough submissions — the oldest SURVIVING one is what source 3 gives.
        let mut a = app("body");
        a.prompts = vec![crate::prompts::PromptGroup {
            pane: "w1:p5".into(),
            prompts: vec![
                crate::prompts::Prompt { ts: 30, pane: "w1:p5".into(), agent: "claude".into(), text: "newest".into() },
                crate::prompts::Prompt { ts: 10, pane: "w1:p5".into(), agent: "claude".into(), text: "oldest".into() },
            ],
        }];
        assert_eq!(a.oldest_prompt_text().as_deref(), Some("oldest"));
    }

    #[test]
    fn oldest_prompt_text_spans_every_group() {
        let mut a = app("body");
        a.prompts = vec![
            crate::prompts::PromptGroup {
                pane: "w1:p5".into(),
                prompts: vec![crate::prompts::Prompt { ts: 30, pane: "w1:p5".into(), agent: "claude".into(), text: "p5".into() }],
            },
            crate::prompts::PromptGroup {
                pane: "w1:p6".into(),
                prompts: vec![crate::prompts::Prompt { ts: 5, pane: "w1:p6".into(), agent: "claude".into(), text: "p6".into() }],
            },
        ];
        assert_eq!(a.oldest_prompt_text().as_deref(), Some("p6"), "oldest across all groups");
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
    fn overlay_clearing_own_row_title_hands_the_in_memory_note_back_to_auto() {
        // `state::set_title` writes `title_auto = title.is_empty()` to DISK.
        // The in-memory buffer wins on the next `save()`, so leaving it stale
        // means the README's "clear the title to hand it back to auto-titling"
        // silently does nothing, and the next edit writes the stale
        // `title_auto:false` back over the disk value, making it permanent.
        let mut a = app("body");
        a.note.title = "Mine".into();
        a.note.title_auto = false; // hand-typed
        let mut e = entry_with_tab("Mine", state::TabStatus::Live, "w1:t1");
        e.is_self = true;
        a.overlay = Some(Overlay::from_entries(vec![e]));
        a.on_key(key(KeyCode::Char('r')));
        for _ in 0.."Mine".len() {
            a.on_key(key(KeyCode::Backspace));
        }
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.note.title, "", "the buffer's title is cleared");
        assert!(a.note.title_auto, "and the buffer agrees with what set_title wrote to disk");
    }

    #[test]
    fn overlay_renaming_own_row_marks_the_in_memory_note_manual() {
        // The mirror case: a typed title is manual on disk, so the buffer must
        // not keep claiming the note is still derivable.
        let mut a = app("body");
        assert!(a.note.title_auto, "starts derivable");
        let mut e = entry_with_tab("", state::TabStatus::Live, "w1:t1");
        e.is_self = true;
        a.overlay = Some(Overlay::from_entries(vec![e]));
        a.on_key(key(KeyCode::Char('r')));
        a.on_key(key(KeyCode::Char('Z')));
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.note.title, "Z");
        assert!(!a.note.title_auto, "a typed title is manual in memory too");
    }

    #[test]
    fn overlay_deleting_own_row_leaves_the_in_memory_note_auto() {
        let mut a = app("body");
        a.note.title = "Mine".into();
        a.note.title_auto = false;
        let mut e = entry_with_tab("Mine", state::TabStatus::Live, "w1:t1");
        e.is_self = true;
        a.overlay = Some(Overlay::from_entries(vec![e]));
        a.on_key(key(KeyCode::Char('d')));
        a.on_key(key(KeyCode::Char('y')));
        assert_eq!(a.note.title, "");
        assert!(a.note.title_auto, "a wiped note is derivable again, in memory as well as on disk");
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

    fn pane_json(pane_id: &str, tab_id: &str, agent: Option<&str>, title: &str, cwd: &str) -> serde_json::Value {
        let mut v = serde_json::json!({
            "pane_id": pane_id,
            "tab_id": tab_id,
            "terminal_title_stripped": title,
            "cwd": cwd,
        });
        if let Some(a) = agent {
            v["agent"] = serde_json::Value::String(a.to_string());
        }
        v
    }

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

    #[test]
    fn build_pane_index_keeps_agent_panes_and_their_fields() {
        // Shapes captured from a live `pane.list` on herdr 0.7.4.
        let panes = vec![
            pane_json("wD:p8", "wD:t2", Some("claude"), "Claude Code", "C:\\repo"),
            pane_json("wD:pB", "wD:t2", None, "C:\\WINDOWS\\powershell.exe", "C:\\repo"),
        ];
        let idx = build_pane_index(&panes);
        let p8 = idx.get("wD:p8").expect("agent pane indexed");
        assert_eq!(p8.agent, "claude");
        assert_eq!(p8.tab_id, "wD:t2");
        assert_eq!(p8.title.as_deref(), Some("Claude Code"));
        assert_eq!(p8.cwd.as_deref(), Some("C:\\repo"));
        let shell = idx.get("wD:pB").expect("shell panes are indexed too");
        assert_eq!(shell.agent, "", "no agent reported yet");
    }

    #[test]
    fn build_pane_index_skips_items_missing_a_pane_id() {
        let panes = vec![serde_json::json!({"tab_id": "wD:t2", "agent": "claude"})];
        assert!(build_pane_index(&panes).is_empty());
    }

    #[test]
    fn meaningful_title_rejects_generic_names_and_paths() {
        assert_eq!(meaningful_title("HM-54271 Generic Importer", "claude").as_deref(), Some("HM-54271 Generic Importer"));
        assert_eq!(meaningful_title("  spaced  ", "claude").as_deref(), Some("spaced"), "trimmed");
        assert_eq!(meaningful_title("", "claude"), None);
        assert_eq!(meaningful_title("   ", "claude"), None);
        assert_eq!(meaningful_title("Claude Code", "claude"), None);
        assert_eq!(meaningful_title("claude code", "claude"), None, "case-insensitive");
        assert_eq!(meaningful_title("CLAUDE", "claude"), None);
        assert_eq!(meaningful_title("Codex", "codex"), None);
        assert_eq!(meaningful_title("C:\\WINDOWS\\powershell.exe", ""), None, "path-shaped");
        assert_eq!(meaningful_title("/usr/bin/bash", ""), None);
        assert_eq!(meaningful_title("something.exe", ""), None);
        assert_eq!(meaningful_title("SOMETHING.EXE", ""), None, "suffix is case-insensitive");
    }

    #[test]
    fn pane_label_prefers_a_meaningful_title() {
        let panes = vec![pane_json("wD:p8", "wD:t2", Some("claude"), "HM-54271 Importer", "C:\\repo")];
        let idx = build_pane_index(&panes);
        assert_eq!(pane_label("wD:p8", "claude", Some(&idx)), "HM-54271 Importer");
    }

    #[test]
    fn pane_label_falls_back_to_agent_and_pane_suffix() {
        let panes = vec![pane_json("wD:p8", "wD:t2", Some("claude"), "Claude Code", "C:\\repo")];
        let idx = build_pane_index(&panes);
        // Generic title -> fallback.
        assert_eq!(pane_label("wD:p8", "claude", Some(&idx)), "claude p8");
        // Pane closed since capture -> not in the index at all.
        assert_eq!(pane_label("wD:p9", "claude", Some(&idx)), "claude p9");
        // Socket unreachable -> no index at all.
        assert_eq!(pane_label("wD:p8", "claude", None), "claude p8");
        // A pane id with no colon still yields something.
        assert_eq!(pane_label("odd", "claude", None), "claude odd");
        // No agent recorded either.
        assert_eq!(pane_label("wD:p8", "", None), "p8");
    }

    #[test]
    fn prompts_load_at_construction_and_on_every_global_toggle() {
        // The block used to be populated ONLY by the 5s-throttled heartbeat,
        // so it was blank for up to five seconds after the pane opened and
        // again after coming back from the global note — which reads exactly
        // like capture is broken.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("notes-promptrefresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config-base");
        std::fs::create_dir_all(&cfg).unwrap();
        // A tab note, a global note (so the toggle finds one in place), and one
        // captured prompt belonging to this tab.
        state::persist_at(
            &dir.join("w1_t1.json"),
            &Note { text: "TAB BODY".into(), ..Default::default() },
            "w1:t1",
            100,
        );
        state::persist_at(
            &dir.join("global.json"),
            &Note { text: "GLOBAL BODY".into(), ..Default::default() },
            "",
            100,
        );
        let key = state::id_key("w1:t1").unwrap();
        let pane_key = state::id_key("w1:p5").unwrap();
        crate::prompts::append_at(
            &crate::prompts::prompts_file(&dir, &key, &pane_key),
            crate::prompts::Prompt {
                ts: 7,
                pane: "w1:p5".into(),
                agent: "claude".into(),
                text: "why is auth flaky".into(),
            },
        );

        let sock = dir.join("no-such.sock");
        let prev = swap_env(&[
            ("HERDR_PLUGIN_STATE_DIR", Some(dir.as_os_str())),
            ("HERDR_TAB_ID", Some(std::ffi::OsStr::new("w1:t1"))),
            ("HERDR_PANE_ID", None),
            ("HERDR_SOCKET_PATH", Some(sock.as_os_str())),
            (CONFIG_BASE_VAR, Some(cfg.as_os_str())),
        ]);

        // The socket path is deliberately dead (`no-such.sock`), so
        // `pane_index()` returns `None` and every group's label falls back to
        // `{agent} {pane-suffix}` — the same fallback `pane_label` is unit
        // tested against directly elsewhere. Derived rather than hardcoded so
        // this test tracks that fallback's actual shape instead of guessing it.
        let expected_label = pane_label("w1:p5", "claude", None);

        // Construction — not the first heartbeat — populates the block.
        let mut a = App::new();
        assert_eq!(a.note.text, "TAB BODY");
        assert_eq!(
            a.prompts.iter().flat_map(|g| g.prompts.iter()).map(|p| p.text.as_str()).collect::<Vec<_>>(),
            vec!["why is auth flaky"],
            "the prompt block must be populated at construction, not up to 5s later"
        );
        assert_eq!(
            a.prompt_labels,
            vec![expected_label.clone()],
            "labels are resolved at construction too, index-aligned with prompts"
        );

        // Tab -> Global: the global note is not a tab and carries no prompts
        // or labels — both vectors clear symmetrically.
        a.toggle_global();
        assert_eq!(a.active, ActiveNote::Global);
        assert!(a.prompts.is_empty(), "the global note carries no prompts");
        assert!(a.prompt_labels.is_empty(), "the global note carries no labels either");

        // Global -> Tab: the block is back at once, again without a heartbeat.
        a.toggle_global();
        assert_eq!(a.active, ActiveNote::Tab);
        assert_eq!(a.prompts.len(), 1, "returning to the tab note refreshes the block immediately");
        assert_eq!(
            a.prompt_labels,
            vec![expected_label],
            "labels are refreshed and re-aligned on the way back too"
        );

        restore_env(prev);
        let _ = std::fs::remove_dir_all(&dir);
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
        // The config base is redirected too: `state::load_global` migrates a
        // config-layout `global.json` into the state dir on first load, and
        // pointing that at the real profile would MOVE the developer's own
        // global note into this temp dir.
        let cfg = dir.join("config-base");
        std::fs::create_dir_all(&cfg).unwrap();
        let prev = swap_env(&[
            ("HERDR_PLUGIN_STATE_DIR", Some(dir.as_os_str())),
            ("HERDR_TAB_ID", None), // no tab id -> shared note.json
            (CONFIG_BASE_VAR, Some(cfg.as_os_str())),
        ]);

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

        restore_env(prev);
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
    fn the_full_footer_shows_every_token() {
        assert_eq!(
            fit_hints(HINTS_PREVIEW, 79),
            " e edit  j/k spc tick  n/N link  r title  l list  Up/Dn scroll  x clear  q quit"
        );
    }

    #[test]
    fn a_narrow_dock_drops_tokens_by_rank() {
        // 46 columns is a typical right dock. Ranks drop x clear, Up/Dn scroll,
        // r title and l list in that order; what is left fits in 39.
        assert_eq!(fit_hints(HINTS_PREVIEW, 46), " e edit  j/k spc tick  n/N link  q quit");
        // At the 37-column floor the checkbox hint goes too. Greedy by rank,
        // not optimal packing: something shorter could still have fitted, and
        // that is the accepted trade for one simple rule.
        assert_eq!(fit_hints(HINTS_PREVIEW, 37), " e edit  n/N link  q quit");
    }

    #[test]
    fn the_link_state_keeps_o_open_and_esc_drop_to_the_floor() {
        for w in [37, 46, 60] {
            let line = fit_hints(HINTS_LINK, w);
            assert!(line.contains("o open"), "{w}: {line}");
            assert!(line.contains("esc drop"), "{w}: {line}");
            assert!(dwidth(&line) <= w, "{w}: {line}");
        }
    }

    #[test]
    fn every_state_keeps_q_quit_at_every_width() {
        for tokens in [HINTS_PREVIEW, HINTS_BOX, HINTS_LINK] {
            for w in 10..=90 {
                assert!(fit_hints(tokens, w).contains("q quit"), "width {w}");
            }
        }
    }

    #[test]
    fn a_wider_pane_never_shows_fewer_tokens() {
        for tokens in [HINTS_PREVIEW, HINTS_BOX, HINTS_LINK] {
            let mut prev = 0;
            for w in 10..=100 {
                let n = fit_hints(tokens, w).split("  ").count();
                assert!(n >= prev, "width {w} regressed from {prev} to {n}");
                prev = n;
            }
        }
    }

    #[test]
    fn the_footer_advertises_the_link_key_in_a_narrow_dock() {
        // The whole point of this task: at 46 columns the old short form had no
        // room for it, so the feature was invisible.
        let mut a = ticket_app("HM-1\n");
        let screen = rendered(&mut a, 46, 10);
        assert!(screen.contains("n/N link"), "{screen}");
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
    fn toggle_global_drops_the_ticket_cursor() {
        // Same shape as the checkbox cursor above: an ordinal into THIS
        // document's ticket hits means nothing once the buffer swaps.
        let mut a = ticket_app("first HM-1\nsecond HM-2\n");
        rendered(&mut a, 40, 10);
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.link_cursor, Some(0));
        a.toggle_global();
        assert_eq!(a.link_cursor, None, "the cursor is per-document, like preview_scroll");
        assert!(!a.follow_link, "and so is its pending scroll-follow");
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
    fn overlay_self_delete_drops_the_ticket_cursor() {
        let mut a = ticket_app("first HM-1\nsecond HM-2\n");
        rendered(&mut a, 40, 10);
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.link_cursor, Some(0));
        a.overlay = Some(Overlay::from_entries(vec![
            OverlayEntry { is_self: true, ..entry("X", state::TabStatus::Closed) },
        ]));
        a.on_key(key(KeyCode::Char('d')));
        a.on_key(key(KeyCode::Char('y')));
        assert_eq!(a.note.text, "");
        assert_eq!(a.link_cursor, None, "clearing the buffer clears its cursor");
        assert!(!a.follow_link);
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

    // ----- ticket cursor: n/N navigation -----------------------------------

    fn ticket_app(text: &str) -> App {
        let mut a = app(text);
        a.tickets = crate::tickets::Config::from_json(
            r#"{"HM":"https://example.test/browse/{key}"}"#,
        );
        a
    }

    #[test]
    fn n_and_n_upper_walk_the_ticket_cursor_and_clamp() {
        let mut a = ticket_app("first HM-1\nsecond HM-2\n");
        rendered(&mut a, 40, 10);
        assert_eq!(a.link_cursor, None, "no cursor until you ask for one");
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.link_cursor, Some(0));
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.link_cursor, Some(1));
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.link_cursor, Some(1), "clamps at the last ticket");
        a.on_key(key(KeyCode::Char('N')));
        assert_eq!(a.link_cursor, Some(0));
        a.on_key(key(KeyCode::Char('N')));
        assert_eq!(a.link_cursor, Some(0), "clamps at the first ticket");
    }

    #[test]
    fn n_upper_with_no_cursor_lands_on_the_last_hit() {
        // From `None`, `move_link`'s `None => n - 1` arm is only reachable
        // when `N` (delta < 0) is the FIRST key pressed — every other test in
        // this module presses `n` first, so that arm would go unexercised
        // (and a `None => 0` typo there would still pass the suite) without
        // this one starting cold on `N`.
        let mut a = ticket_app("first HM-1\nsecond HM-2\n");
        rendered(&mut a, 40, 10);
        assert_eq!(a.link_cursor, None, "no cursor until you ask for one");
        a.on_key(key(KeyCode::Char('N')));
        assert_eq!(a.link_cursor, Some(1), "N from no cursor lands on the last hit");
    }

    #[test]
    fn n_does_nothing_without_configured_tickets() {
        let mut a = app("HM-1 here"); // no config injected
        rendered(&mut a, 40, 10);
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.link_cursor, None);
        assert!(a.link_hits.is_empty());
    }

    #[test]
    fn the_two_cursors_are_mutually_exclusive() {
        let mut a = ticket_app("[ ] task HM-1\n[ ] other\n");
        rendered(&mut a, 40, 10);
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(0));
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.link_cursor, Some(0));
        assert_eq!(a.box_cursor, None, "n drops the checkbox cursor");
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(0));
        assert_eq!(a.link_cursor, None, "j drops the ticket cursor");
    }

    #[test]
    fn esc_drops_both_cursors() {
        let mut a = ticket_app("[ ] task HM-1\n");
        rendered(&mut a, 40, 10);
        a.on_key(key(KeyCode::Char('n')));
        a.on_key(key(KeyCode::Esc));
        assert_eq!(a.link_cursor, None);
        assert!(!a.follow_link, "and the pending scroll-follow with it");
        assert_eq!(a.box_cursor, None);
    }

    #[test]
    fn clearing_the_note_drops_the_ticket_cursor() {
        // A stale ordinal is harmless only while there is no text under it.
        let mut a = ticket_app("HM-1\n");
        rendered(&mut a, 40, 10);
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.link_cursor, Some(0));
        a.on_key(key(KeyCode::Char('x')));
        a.on_key(key(KeyCode::Char('y')));
        assert_eq!(a.link_cursor, None);
    }

    #[test]
    fn an_edit_that_removes_a_ticket_reclamps_the_cursor() {
        let mut a = ticket_app("HM-1 and HM-2\n");
        rendered(&mut a, 40, 10);
        a.on_key(key(KeyCode::Char('n')));
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.link_cursor, Some(1));
        a.note.text = "HM-1 only\n".to_string();
        rendered(&mut a, 40, 10);
        assert_eq!(a.link_cursor, Some(0), "clamped to the surviving ticket");
        a.note.text = "nothing here\n".to_string();
        rendered(&mut a, 40, 10);
        assert_eq!(a.link_cursor, None);
    }

    #[test]
    fn the_prompt_block_offsets_ticket_rows() {
        // Hit rows index the FINAL preview line list, so they must be shifted
        // past the prompt block the same way `map` is.
        let mut a = ticket_app("HM-1 here\n");
        rendered(&mut a, 40, 20);
        let without = a.link_hits[0].row;
        a.prompts = vec![crate::prompts::PromptGroup {
            pane: "w1:p5".into(),
            prompts: vec![prompt(1, "look at HM-1")],
        }];
        a.prompt_labels = vec!["claude p5".into()];
        rendered(&mut a, 40, 20);
        assert_eq!(a.link_hits.len(), 1, "the block is not scanned for tickets");
        assert!(a.link_hits[0].row > without, "row shifted past the block");
    }

    #[test]
    fn an_empty_note_has_no_hits() {
        let mut a = ticket_app("");
        rendered(&mut a, 40, 10);
        assert!(a.link_hits.is_empty());
        assert_eq!(a.link_cursor, None);
    }

    #[test]
    fn the_ticket_cursor_scrolls_itself_into_view_once() {
        let mut a = ticket_app(&format!("{}HM-1 at the bottom\n", "filler\n".repeat(40)));
        rendered(&mut a, 40, 10);
        assert_eq!(a.preview_scroll, 0);
        a.on_key(key(KeyCode::Char('n')));
        rendered(&mut a, 40, 10);
        assert!(a.preview_scroll > 0, "scrolled to the only ticket");
        assert!(!a.follow_link, "one-shot: cleared after the draw");
        let settled = a.preview_scroll;
        a.on_key(key(KeyCode::Char('g')));
        rendered(&mut a, 40, 10);
        assert_eq!(a.preview_scroll, 0, "manual scrolling is not fought");
        assert!(settled > 0);
    }

    #[test]
    fn the_footer_advertises_the_ticket_keys_while_the_cursor_is_live() {
        let mut a = ticket_app("HM-1\n");
        rendered(&mut a, 90, 10);
        a.on_key(key(KeyCode::Char('n')));
        let screen = rendered(&mut a, 90, 10);
        assert!(screen.contains("o open"), "{screen}");
        assert!(screen.contains("esc drop"));
    }

    #[test]
    fn every_short_footer_form_keeps_the_quit_hint() {
        let mut a = ticket_app("HM-1\n");
        rendered(&mut a, 40, 10);
        a.on_key(key(KeyCode::Char('n')));
        let screen = rendered(&mut a, 40, 10);
        assert!(screen.contains("q quit"), "{screen}");
    }

    #[test]
    fn the_cursor_ordinal_spans_source_lines() {
        // Guards the render's ordinal formula: a per-line reset would highlight
        // the wrong key on any multi-line note.
        let mut a = ticket_app("HM-1 here\n\nHM-2 there\n");
        rendered(&mut a, 40, 10);
        a.on_key(key(KeyCode::Char('n')));
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.link_cursor, Some(1));
        assert_eq!(a.link_hits[1].text, "HM-2");
        assert!(a.link_hits[0].row < a.link_hits[1].row);
    }

    // ----- o opens the cursored ticket in the browser ----------------------

    #[test]
    fn o_without_a_cursor_or_config_does_nothing() {
        // No panic, no child, no output — the whole failure contract.
        let mut a = ticket_app("HM-1\n");
        rendered(&mut a, 40, 10);
        a.on_key(key(KeyCode::Char('o')));
        assert!(a.open_children.is_empty(), "no cursor, nothing to open");

        let mut b = app("HM-1\n"); // no config
        rendered(&mut b, 40, 10);
        b.on_key(key(KeyCode::Char('n')));
        b.on_key(key(KeyCode::Char('o')));
        assert!(b.open_children.is_empty());
    }

    #[test]
    fn o_resolves_the_cursored_key_to_a_url() {
        // The URL, not the spawn, is the tested part: `pending_open` returns
        // what `o` would hand to the browser.
        let mut a = ticket_app("first HM-1\nsecond HM-2\n");
        rendered(&mut a, 40, 10);
        a.on_key(key(KeyCode::Char('n')));
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(
            a.pending_open().as_deref(),
            Some("https://example.test/browse/HM-2")
        );
    }

    #[test]
    fn a_url_in_the_note_is_navigable_and_opens_itself() {
        let mut a = ticket_app("read https://example.test/doc later\n");
        rendered(&mut a, 40, 10);
        assert_eq!(a.link_hits.len(), 1);
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.pending_open().as_deref(), Some("https://example.test/doc"));
    }

    #[test]
    fn one_cursor_walks_tickets_and_urls_in_document_order() {
        let mut a = ticket_app("HM-1 then https://example.test/x\n");
        rendered(&mut a, 60, 10);
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.pending_open().as_deref(), Some("https://example.test/browse/HM-1"));
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.pending_open().as_deref(), Some("https://example.test/x"));
    }
}
