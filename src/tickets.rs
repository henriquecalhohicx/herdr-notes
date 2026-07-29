//! Ticket links: the prefix→URL config and the browser launch.

use std::collections::BTreeMap;

/// Hand-edited by the user, beside the note files.
pub const FILE: &str = "tickets.json";

/// Where the config lives: beside the note files, so it follows the same
/// three-tier store resolution. Pure path logic, no I/O — the seam both
/// `Config::load` and the heartbeat's mtime stat share, so the path is
/// spelled once.
pub fn config_path() -> Option<std::path::PathBuf> {
    crate::state::store_dir().map(|dir| dir.join(FILE))
}

/// Issue-key prefix → URL template containing `{key}`. Empty map means the
/// whole feature is dormant: nothing is detected, styled or openable.
#[derive(Clone, Debug, Default)]
pub struct Config {
    map: BTreeMap<String, String>,
}

impl Config {
    /// Forgiving parse, matching the rest of the crate: anything unusable is
    /// dropped silently rather than failing the load. A template with no
    /// `{key}` is dropped too — a keyless URL would open the tracker's home
    /// page and read as success.
    pub fn from_json(s: &str) -> Self {
        let mut map = BTreeMap::new();
        // PS 5.1 writes a UTF-8 BOM; every stdin/file parser in this crate
        // strips it.
        if let Ok(serde_json::Value::Object(obj)) =
            serde_json::from_str::<serde_json::Value>(s.trim_start_matches('\u{feff}'))
        {
            for (prefix, template) in obj {
                if let Some(t) = template.as_str()
                    && !prefix.is_empty()
                    && t.contains("{key}")
                {
                    map.insert(prefix, t.to_string());
                }
            }
        }
        Self { map }
    }

    /// Reads `tickets.json` from `dir`. Injected base dir so tests never touch
    /// the real store, exactly as `state.rs` does it. Test-only now: `load`
    /// goes through `config_path` + a direct read instead (Task 6), so this
    /// has no production caller left.
    #[cfg(test)]
    pub fn load_in(dir: &std::path::Path) -> Self {
        std::fs::read_to_string(dir.join(FILE))
            .map(|s| Self::from_json(&s))
            .unwrap_or_default()
    }

    /// The real load: `tickets.json` in the note store dir, so it follows the
    /// same three-tier resolution the note files use. Collapses EVERY
    /// failure (absent, unreadable) to an empty config — right for a one-time
    /// startup read, where there is no previous good config to protect. The
    /// heartbeat's hot-reload cannot afford that collapse (see `try_load`).
    pub fn load() -> Self {
        match try_load() {
            LoadResult::Loaded(cfg) => cfg,
            LoadResult::Absent | LoadResult::ReadError => Self::default(),
        }
    }

    pub fn has_prefix(&self, prefix: &str) -> bool {
        self.map.contains_key(prefix)
    }

    /// Test-only now: Task 2 dropped the last production call (`markdown`'s
    /// `emit` used to skip the link pass on an empty config; URLs need no
    /// config, so that fast path now gates on `LinkCtx::enabled` instead).
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// The outcome of one attempt to read `tickets.json`, distinguishing "no
/// file" — a real, non-error state, the feature legitimately dormant — from
/// "the file exists but the read failed" — an AV scan, a sharing violation
/// while an editor rewrites it, a network store hiccup, always transient.
/// `Config::load` collapses both to an empty config (right for a one-time
/// startup read, nothing to protect); the heartbeat's hot-reload cannot make
/// that same collapse, because it also decides whether to stamp the new
/// mtime — stamping it on a transient read failure would turn the feature
/// off and leave it dormant until the user next touches the file, since
/// nothing else would ever make the mtime "move" again.
#[derive(Debug)]
pub enum LoadResult {
    /// No file at this path (including when the store dir itself could not
    /// be resolved).
    Absent,
    /// Read and parsed — parsing itself never fails, see `Config::from_json`.
    Loaded(Config),
    /// The file exists but reading it failed. The caller must not treat this
    /// as "the config is now empty" and must not remember the mtime it was
    /// chasing when this happened.
    ReadError,
}

/// One attempt to read `tickets.json`, classified per `LoadResult`. The seam
/// the heartbeat's hot-reload decides against, kept separate from `load` so
/// each caller can want a different thing from the same read.
pub fn try_load() -> LoadResult {
    let Some(path) = config_path() else { return LoadResult::Absent };
    match std::fs::read_to_string(&path) {
        Ok(s) => LoadResult::Loaded(Config::from_json(&s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => LoadResult::Absent,
        Err(_) => LoadResult::ReadError,
    }
}

/// The URL for `key`, or `None` when its prefix is unmapped. Pure — this is the
/// tested seam; `open` itself stays thin and untested.
pub fn ticket_url(cfg: &Config, key: &str) -> Option<String> {
    let (prefix, _) = key.split_once('-')?;
    Some(cfg.map.get(prefix)?.replace("{key}", key))
}

/// The platform's URL handler, as a `Command` so it can be asserted without
/// launching anything. `rundll32` rather than `cmd /c start` on Windows: `cmd`
/// flashes a console over the TUI and its quoting mangles URLs containing `&`.
fn launch_command(url: &str) -> std::process::Command {
    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("rundll32.exe");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else {
        std::process::Command::new("xdg-open")
    };
    cmd.arg(url);
    cmd
}

/// Hands `url` to the platform browser. `spawn`, never `output`: a blocking
/// wait here sits on the event-loop thread and would freeze input, drawing AND
/// the 5s identity re-stamp — past 20s the launcher calls this live pane a
/// corpse and REPLACEs it, and `pane close` kills with no signal, taking the
/// dirty debounce buffer. Returns the child so the caller can reap it (unix
/// would otherwise leave a zombie per open); `None` on any failure, silently.
pub fn open(url: &str) -> Option<std::process::Child> {
    let mut cmd = launch_command(url);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.spawn().ok()
}

#[cfg(test)]
mod tests {
    use super::{Config, FILE, LoadResult, config_path, launch_command, ticket_url, try_load};

    #[test]
    fn a_well_formed_map_loads() {
        let cfg = Config::from_json(r#"{"HM":"https://example.test/browse/{key}"}"#);
        assert!(cfg.has_prefix("HM"));
        assert!(!cfg.is_empty());
        assert_eq!(
            ticket_url(&cfg, "HM-54561").as_deref(),
            Some("https://example.test/browse/HM-54561")
        );
    }

    #[test]
    fn a_template_without_the_placeholder_is_dropped() {
        // Opening a keyless URL is worse than doing nothing: the user would
        // land on a tracker home page and think the feature worked.
        let cfg = Config::from_json(r#"{"HM":"https://example.test/browse/"}"#);
        assert!(!cfg.has_prefix("HM"));
        assert!(cfg.is_empty());
    }

    #[test]
    fn junk_input_degrades_to_an_empty_map() {
        for src in ["", "not json", "[]", "null", r#"{"HM":42}"#, r#"{"":"x/{key}"}"#] {
            let cfg = Config::from_json(src);
            assert!(cfg.is_empty(), "{src:?} should yield an empty map");
        }
    }

    #[test]
    fn a_bom_prefixed_file_still_parses() {
        // herdr panes run Windows PowerShell 5.1, whose `Set-Content -Encoding
        // UTF8` writes a BOM. A hand-written config is exactly the file that
        // gets created that way.
        let cfg = Config::from_json("\u{feff}{\"HM\":\"https://example.test/{key}\"}");
        assert!(cfg.has_prefix("HM"));
    }

    #[test]
    fn an_unmapped_prefix_has_no_url() {
        let cfg = Config::from_json(r#"{"HM":"https://example.test/{key}"}"#);
        assert_eq!(ticket_url(&cfg, "CR-3171"), None);
        assert_eq!(ticket_url(&cfg, "nonsense"), None);
    }

    #[test]
    fn a_missing_file_is_an_empty_map() {
        let dir = std::env::temp_dir().join(format!("notes-tickets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(Config::load_in(&dir).is_empty());

        std::fs::write(dir.join(FILE), r#"{"TT":"https://example.test/{key}"}"#).unwrap();
        assert!(Config::load_in(&dir).has_prefix("TT"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_config_path_sits_beside_the_notes() {
        // Same store dir as the note files, so it follows the same three tiers.
        let _guard = crate::state::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("notes-cfgpath-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Restore whatever this var held BEFORE the test, matching the
        // swap/restore convention used everywhere else in the crate
        // (`app.rs`'s `swap_env`/`restore_env`) rather than an unconditional
        // `remove_var` — a bare remove is only correct when nothing else in
        // the same test binary ever needs this var set, which is not
        // something a single test may assume.
        let prev = std::env::var_os("HERDR_PLUGIN_STATE_DIR");
        // SAFETY: serialized by ENV_LOCK; restored below.
        unsafe { std::env::set_var("HERDR_PLUGIN_STATE_DIR", &dir) };
        assert_eq!(config_path(), Some(dir.join(FILE)));
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            match prev {
                Some(val) => std::env::set_var("HERDR_PLUGIN_STATE_DIR", val),
                None => std::env::remove_var("HERDR_PLUGIN_STATE_DIR"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn try_load_distinguishes_absent_from_unreadable() {
        // The classification the heartbeat's hot-reload decides against
        // (Important 3): "no file" must stamp the new mtime (a real,
        // non-error state); a genuine read failure must not, or the retry
        // that would otherwise fire next beat never happens. Forcing a real
        // I/O error portably (no permissions API, works the same on Windows
        // and Unix): put a DIRECTORY where the file is expected. `metadata`
        // on it succeeds — its mtime would look like "the file changed" —
        // but `read_to_string` fails, which is exactly the shape a sharing
        // violation or a network hiccup takes from this function's point of
        // view.
        let _guard = crate::state::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("notes-tickets-seam-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::var_os("HERDR_PLUGIN_STATE_DIR");
        // SAFETY: serialized by ENV_LOCK; restored below.
        unsafe { std::env::set_var("HERDR_PLUGIN_STATE_DIR", &dir) };

        assert!(matches!(try_load(), LoadResult::Absent), "no file at all is Absent, not an error");

        std::fs::create_dir_all(dir.join(FILE)).unwrap();
        assert!(
            matches!(try_load(), LoadResult::ReadError),
            "a path that exists but cannot be read as a file is ReadError, not Absent"
        );

        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            match prev {
                Some(val) => std::env::set_var("HERDR_PLUGIN_STATE_DIR", val),
                None => std::env::remove_var("HERDR_PLUGIN_STATE_DIR"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_launch_command_matches_the_platform() {
        let cmd = launch_command("https://example.test/browse/HM-1");
        let program = cmd.get_program().to_string_lossy().to_string();
        let args: Vec<String> =
            cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        if cfg!(windows) {
            assert_eq!(program, "rundll32.exe");
            assert_eq!(args[0], "url.dll,FileProtocolHandler");
        } else if cfg!(target_os = "macos") {
            assert_eq!(program, "open");
        } else {
            assert_eq!(program, "xdg-open");
        }
        // The URL is a single argv entry: no shell, so nothing in it is
        // interpreted (an `&` in a query string, for one).
        assert_eq!(args.last().unwrap(), "https://example.test/browse/HM-1");
    }
}
