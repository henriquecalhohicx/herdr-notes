//! Persistent note state: one scrollable markdown note PER TAB plus the
//! last-active mode, stored as a small JSON file beside herdr's own config
//! (`%APPDATA%\herdr\notes\<tab-key>.json` on Windows,
//! `$XDG_CONFIG_HOME/herdr/…` elsewhere) so the note survives computer
//! restarts. The key is the `HERDR_TAB_ID` herdr injects into every managed
//! pane (its `:` separator sanitized to `_`); outside herdr (or on an id
//! unsafe for a filename) the pane falls back to the legacy single-note
//! `herdr/notes.json`, and the first tab to load notes MOVES that legacy file
//! into its own slot.
//!
//! Loading is forgiving — a missing, hand-edited, or truncated file falls back
//! to an empty note and never panics. Saving is atomic (temp file + rename)
//! and best-effort: the pane keeps working for the session if persist fails.

use std::path::{Path, PathBuf};

/// Pane label the launcher assigns and the heartbeat re-asserts as the title.
pub const PANE_LABEL: &str = "Notes";

/// Source id for `pane.report_metadata`; its token marks a pane as the Notes
/// pane and doubles as the liveness heartbeat.
pub const METADATA_SOURCE: &str = "herdr-notes";

/// Unix seconds now — the heartbeat clock for the pane identity token.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    #[default]
    Preview,
    Edit,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Preview => "preview",
            Mode::Edit => "edit",
        }
    }
}

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

/// Where notes live. herdr's plugin docs say durable state belongs in
/// `HERDR_PLUGIN_STATE_DIR`, which herdr injects into plugin-run commands
/// (the unix `[[panes]]` entry gets it natively; the Windows launcher passes
/// it through `pane split --env`). A TUI started by hand has neither, so the
/// pre-existing config-dir layout stays as the fallback — and as the
/// migration source when the state dir is empty.
enum StoreBase {
    /// `HERDR_PLUGIN_STATE_DIR`: files live directly in the dir
    /// (`<dir>/<key>.json`, no-tab fallback `<dir>/note.json`).
    PluginState(PathBuf),
    /// Config-dir layout (`<config>/herdr/notes/<key>.json`, legacy
    /// `<config>/herdr/notes.json`).
    Config(PathBuf),
}

fn store_base() -> Option<StoreBase> {
    if let Some(dir) = std::env::var_os("HERDR_PLUGIN_STATE_DIR").filter(|d| !d.is_empty()) {
        return Some(StoreBase::PluginState(PathBuf::from(dir)));
    }
    config_base().map(StoreBase::Config)
}

/// Platform config base (`%APPDATA%` / `$XDG_CONFIG_HOME` / `~/.config`),
/// the convention herdr plugins used before `HERDR_PLUGIN_STATE_DIR` existed.
/// All path logic below takes this as a parameter so tests can inject a temp
/// dir.
fn config_base() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    base
}

/// The tab id herdr injects into every managed pane; the per-tab note key.
/// Empty = unset (running outside herdr).
pub(crate) fn tab_env() -> Option<String> {
    std::env::var("HERDR_TAB_ID").ok().filter(|id| !id.is_empty())
}

/// True when the tab id is safe to embed in a filename. Real herdr tab ids are
/// `<workspace>:<n>` (e.g. "w6:t1"), so the single `:` separator is admitted
/// alongside plain alphanumerics; [`note_key`] sanitizes it to `_`. Everything
/// else — dots, spaces, `-`, `_`, anything path-traversal-shaped — falls back
/// to the legacy path instead.
fn is_filename_safe(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == ':')
}

/// Pre-per-tab single-note file; also the fallback when no (safe)
/// tab id is available.
fn legacy_path_in(base: &Path) -> PathBuf {
    base.join("herdr").join("notes.json")
}

/// The note-FILE identity of a tab id: `Some(key)` when the id gets its own
/// per-tab file, `None` when it falls back to the shared legacy `notes.json`.
/// Panes whose keys are EQUAL load and save the SAME file. This is the identity
/// the launcher's duplicate-instance guard (launch.rs) compares — never raw tab
/// ids — so the guard can't drift from the on-disk layout: unsafe/missing ids
/// all coarsen to one legacy file, the `:` separator is sanitized to `_`
/// (herdr ids never contain `_`, so no collision), and on Windows ASCII case is
/// folded because NTFS filenames are case-insensitive ("W6_T1.json" and
/// "w6_t1.json" are one file).
pub fn note_key(tab_id: Option<&str>) -> Option<String> {
    let id = tab_id.filter(|id| is_filename_safe(id))?;
    let key = id.replace(':', "_");
    #[cfg(windows)]
    let key = key.to_ascii_lowercase();
    Some(key)
}

/// Pure path selection: `<base>/herdr/notes/<note-key>.json` for a
/// filename-safe id, the legacy `<base>/herdr/notes.json` otherwise.
/// Built from [`note_key`] so path identity and guard identity always agree.
fn state_path_in(base: &Path, tab_id: Option<&str>) -> PathBuf {
    match note_key(tab_id) {
        Some(key) => base.join("herdr").join("notes").join(format!("{key}.json")),
        None => legacy_path_in(base),
    }
}

/// Path selection for the plugin-state layout: `<dir>/<note-key>.json`, with
/// the shared `<dir>/note.json` for missing/unsafe tab ids.
fn state_dir_path(dir: &Path, tab_id: Option<&str>) -> PathBuf {
    match note_key(tab_id) {
        Some(key) => dir.join(format!("{key}.json")),
        None => dir.join("note.json"),
    }
}

/// State file location for THIS process (env-derived base + tab id).
pub fn state_path() -> Option<PathBuf> {
    let tab = tab_env();
    Some(match store_base()? {
        StoreBase::PluginState(dir) => state_dir_path(&dir, tab.as_deref()),
        StoreBase::Config(base) => state_path_in(&base, tab.as_deref()),
    })
}

pub fn load() -> Note {
    let tab = tab_env();
    match store_base() {
        Some(StoreBase::PluginState(dir)) => {
            load_state_dir(&dir, config_base().as_deref(), tab.as_deref())
        }
        Some(StoreBase::Config(base)) => load_in(&base, tab.as_deref()),
        None => Note::default(),
    }
}

/// Load from the plugin state dir, migrating from the config-dir layout the
/// first time: if this tab's file is missing there, MOVE the config-dir
/// per-tab file (or, failing that, the legacy single note) into place.
/// A failed rename falls back to reading the source without moving it.
fn load_state_dir(dir: &Path, config: Option<&Path>, tab_id: Option<&str>) -> Note {
    let path = state_dir_path(dir, tab_id);
    if !path.exists()
        && let Some(base) = config
    {
        let sources = [state_path_in(base, tab_id), legacy_path_in(base)];
        if let Some(src) = sources.iter().find(|p| p.exists()) {
            let moved = std::fs::create_dir_all(dir).is_ok() && std::fs::rename(src, &path).is_ok();
            if !moved {
                return read_note(src);
            }
        }
    }
    read_note(&path)
}

/// Load with one-time migration: when the per-tab file does not exist
/// yet but the legacy single-note file does, MOVE the legacy file into this
/// tab's slot — the first tab to open notes inherits the old note.
/// If the rename fails the legacy file is read in place (not moved); when both
/// files exist the per-tab one wins and the legacy file is untouched.
fn load_in(base: &Path, tab_id: Option<&str>) -> Note {
    let path = state_path_in(base, tab_id);
    let legacy = legacy_path_in(base);
    if path != legacy && !path.exists() && legacy.exists() {
        let moved = path.parent().is_some_and(|dir| {
            std::fs::create_dir_all(dir).is_ok() && std::fs::rename(&legacy, &path).is_ok()
        });
        if !moved {
            return read_note(&legacy);
        }
    }
    read_note(&path)
}

pub(crate) fn read_note(path: &Path) -> Note {
    std::fs::read_to_string(path).map(|json| parse(&json)).unwrap_or_default()
}

/// The directory holding per-note files for THIS process, or None outside herdr
/// with no config dir. Mirrors `state_path` but yields the containing dir.
#[allow(dead_code)] // consumed by the notes overlay (later task)
pub fn store_dir() -> Option<PathBuf> {
    Some(match store_base()? {
        StoreBase::PluginState(dir) => dir,
        StoreBase::Config(base) => base.join("herdr").join("notes"),
    })
}

/// One row of the notes list.
#[derive(Clone, PartialEq, Eq, Debug)]
#[allow(dead_code)] // consumed by the notes overlay (later task)
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
#[allow(dead_code)] // consumed by the notes overlay (later task)
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
    out.sort_by_key(|s| std::cmp::Reverse(s.updated));
    out
}

/// Forgiving parse: any missing/garbled field falls back to the default, so a
/// hand-edited or truncated file can never wedge the pane.
pub fn parse(json: &str) -> Note {
    let value: serde_json::Value = match serde_json::from_str(json.trim_start_matches('\u{feff}')) {
        Ok(v) => v,
        Err(_) => return Note::default(),
    };
    let text = value
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let mode = match value.get("mode").and_then(|v| v.as_str()) {
        Some("edit") => Mode::Edit,
        _ => Mode::Preview,
    };
    let title = value.get("title").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let tab_id = value.get("tab_id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let created = value.get("created").and_then(|v| v.as_u64()).unwrap_or(0);
    let updated = value.get("updated").and_then(|v| v.as_u64()).unwrap_or(0);
    Note { text, mode, title, tab_id, created, updated }
}

/// The JSON that goes on disk: `{ "text": …, "mode": "preview"|"edit" }`.
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

/// A note with no text and no title carries nothing worth a file.
pub fn is_blank(note: &Note) -> bool {
    note.text.trim().is_empty() && note.title.trim().is_empty()
}

/// Whether the tab that owns a note is still open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)] // consumed by the notes overlay (later task)
pub enum TabStatus {
    Live,
    Closed,
    Unknown,
}

/// Classify a note's owner tab against the set of live tab ids. `None` (socket
/// unreachable) or an empty owner id is Unknown; otherwise Live iff present.
#[allow(dead_code)] // consumed by the notes overlay (later task)
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

/// Human age from a "seconds ago" delta: just now / Nm / Nh / Nd / Nw.
#[allow(dead_code)] // consumed by the notes overlay (later task)
pub fn format_age(secs_ago: u64) -> String {
    match secs_ago {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m", secs_ago / 60),
        3600..=86_399 => format!("{}h", secs_ago / 3600),
        86_400..=604_799 => format!("{}d", secs_ago / 86_400),
        _ => format!("{}w", secs_ago / 604_800),
    }
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

/// Set a note file's title in place (blank text + blank title would delete it).
pub fn set_title(file: &Path, title: &str) {
    let mut note = read_note(file);
    note.title = title.trim().to_string();
    let tab_id = note.tab_id.clone();
    persist_at(file, &note, &tab_id, unix_now());
}

/// Atomic best-effort persist: write a temp file, fsync it, then rename over
/// the real one (std's rename replaces existing files on Windows too). The
/// fsync BEFORE the rename matters: without it a crash or power loss can make
/// the rename durable ahead of the data, leaving an empty/truncated file the
/// forgiving loader would silently turn into an empty note.
pub fn save(note: &Note) {
    let Some(path) = state_path() else { return };
    persist_at(&path, note, &tab_env().unwrap_or_default(), unix_now());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_text_and_mode() {
        let note = Note { text: "# one\n\ntwo `lines`\n".into(), mode: Mode::Edit, ..Default::default() };
        assert_eq!(parse(&to_json(&note)), note);
        let preview = Note { text: String::new(), mode: Mode::Preview, ..Default::default() };
        assert_eq!(parse(&to_json(&preview)), preview);
    }

    #[test]
    fn corrupt_or_missing_input_falls_back_to_empty_note() {
        assert_eq!(parse("garbage"), Note::default());
        assert_eq!(parse(""), Note::default());
        assert_eq!(parse("{}"), Note::default());
        assert_eq!(parse("{\"text\":123}"), Note::default());
        assert_eq!(parse("{\"text\":\"keep\",\"mode\":7}").text, "keep");
        assert_eq!(Note::default().text, "");
        assert_eq!(Note::default().mode, Mode::Preview);
    }

    #[test]
    fn bom_from_powershell_pipe_is_stripped() {
        let note = Note { text: "hi".into(), mode: Mode::Preview, ..Default::default() };
        let json = format!("\u{feff}{}", to_json(&note));
        assert_eq!(parse(&json), note);
    }

    #[test]
    fn unknown_mode_falls_back_to_preview() {
        assert_eq!(parse("{\"text\":\"a\",\"mode\":\"bogus\"}").mode, Mode::Preview);
        assert_eq!(parse("{\"text\":\"a\",\"mode\":\"edit\"}").mode, Mode::Edit);
    }

    /// Fresh per-test base dir under the OS temp dir — path logic takes the
    /// base as a parameter precisely so tests never touch the real APPDATA.
    fn temp_base(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("notes-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("herdr")).unwrap();
        dir
    }

    fn write_note(path: &Path, text: &str) {
        std::fs::write(path, to_json(&Note { text: text.into(), mode: Mode::Preview, ..Default::default() })).unwrap();
    }

    #[test]
    fn state_path_keys_on_safe_tab_ids_only() {
        let base = Path::new("base");
        // A tab id (`w6:t1`) sanitizes its `:` separator into the filename.
        assert_eq!(
            state_path_in(base, Some("w6:t1")),
            base.join("herdr").join("notes").join("w6_t1.json")
        );
        // A bare alphanumeric id still keys directly.
        assert_eq!(
            state_path_in(base, Some("w6")),
            base.join("herdr").join("notes").join("w6.json")
        );
        // Unset (outside herdr) and genuinely unsafe ids use the legacy path.
        let legacy = legacy_path_in(base);
        assert_eq!(state_path_in(base, None), legacy);
        for bad in ["", "../evil", "a b", "-w6", "w6.json", "w6_t1"] {
            assert_eq!(state_path_in(base, Some(bad)), legacy, "unsafe id {bad:?}");
        }
    }

    #[test]
    fn note_key_mirrors_file_identity() {
        assert_eq!(note_key(Some("w6")), Some("w6".to_string()));
        // A tab id's `:` is sanitized to `_` (herdr ids never contain `_`, so
        // this can't collide with any real id).
        assert_eq!(note_key(Some("w6:t1")), Some("w6_t1".to_string()));
        // Every id without its own file shares ONE key (None = legacy file).
        assert_eq!(note_key(None), None);
        for bad in ["", "../evil", "a b", "-w6", "w6.json", "w6_t1"] {
            assert_eq!(note_key(Some(bad)), None, "unsafe id {bad:?}");
        }
        // NTFS is case-insensitive: "W6:T1" and "w6:t1" hit the same file on
        // Windows, so their keys (and filenames) must fold together there.
        #[cfg(windows)]
        {
            assert_eq!(note_key(Some("W6:T1")), Some("w6_t1".to_string()));
            let base = Path::new("base");
            assert_eq!(
                state_path_in(base, Some("W6:T1")),
                state_path_in(base, Some("w6:t1"))
            );
        }
        #[cfg(not(windows))]
        assert_eq!(note_key(Some("W6:T1")), Some("W6_T1".to_string()));
    }

    #[test]
    fn first_load_moves_the_legacy_note_into_the_tab_slot() {
        let base = temp_base("migrate");
        write_note(&legacy_path_in(&base), "old note");
        assert_eq!(load_in(&base, Some("w6")).text, "old note");
        assert!(!legacy_path_in(&base).exists(), "legacy file was moved, not copied");
        assert!(state_path_in(&base, Some("w6")).exists());
        // Second load reads the migrated file; nothing left to migrate.
        assert_eq!(load_in(&base, Some("w6")).text, "old note");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn per_tab_file_wins_over_a_lingering_legacy_file() {
        let base = temp_base("both");
        let tab_path = state_path_in(&base, Some("w6"));
        std::fs::create_dir_all(tab_path.parent().unwrap()).unwrap();
        write_note(&tab_path, "mine");
        write_note(&legacy_path_in(&base), "stale");
        assert_eq!(load_in(&base, Some("w6")).text, "mine");
        assert!(legacy_path_in(&base).exists(), "legacy file untouched when both exist");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn plugin_state_dir_layout_migrates_from_the_config_layout() {
        let base = temp_base("statedir");
        let dir = base.join("plugin-state");
        // Per-tab file moves over from the config layout on first load.
        let cfg_ws = state_path_in(&base, Some("w6"));
        std::fs::create_dir_all(cfg_ws.parent().unwrap()).unwrap();
        write_note(&cfg_ws, "from config");
        assert_eq!(load_state_dir(&dir, Some(&base), Some("w6")).text, "from config");
        assert!(!cfg_ws.exists(), "moved, not copied");
        assert!(dir.join("w6.json").exists());
        assert_eq!(load_state_dir(&dir, Some(&base), Some("w6")).text, "from config");
        // No tab id: shared note.json, migrating the config legacy file.
        write_note(&legacy_path_in(&base), "legacy");
        assert_eq!(load_state_dir(&dir, Some(&base), None).text, "legacy");
        assert!(dir.join("note.json").exists());
        // Nothing anywhere is still just an empty note.
        assert_eq!(load_state_dir(&dir, Some(&base), Some("w9")), Note::default());
        let _ = std::fs::remove_dir_all(&base);
    }

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

    #[test]
    fn unset_tab_id_reads_the_legacy_file_in_place() {
        let base = temp_base("legacy");
        write_note(&legacy_path_in(&base), "global");
        assert_eq!(load_in(&base, None).text, "global");
        assert!(legacy_path_in(&base).exists(), "no migration without a tab id");
        let _ = std::fs::remove_dir_all(&base);
        // Nothing on disk at all (any key) is still just an empty note.
        let empty = temp_base("empty");
        assert_eq!(load_in(&empty, Some("w9")), Note::default());
        assert_eq!(load_in(&empty, None), Note::default());
        let _ = std::fs::remove_dir_all(&empty);
    }

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

    #[test]
    fn classify_tab_maps_live_closed_unknown() {
        use std::collections::HashSet;
        let live: HashSet<String> = ["w1:t1".to_string()].into_iter().collect();
        assert_eq!(classify_tab("w1:t1", Some(&live)), TabStatus::Live);
        assert_eq!(classify_tab("w1:t9", Some(&live)), TabStatus::Closed);
        assert_eq!(classify_tab("", Some(&live)), TabStatus::Unknown, "no owner id");
        assert_eq!(classify_tab("w1:t1", None), TabStatus::Unknown, "socket unavailable");
    }

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
}
