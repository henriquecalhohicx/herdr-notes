//! Ticket links: the prefix→URL config and the browser launch.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;

/// Hand-edited by the user, beside the note files.
pub const FILE: &str = "tickets.json";

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
    /// the real store, exactly as `state.rs` does it.
    pub fn load_in(dir: &Path) -> Self {
        std::fs::read_to_string(dir.join(FILE))
            .map(|s| Self::from_json(&s))
            .unwrap_or_default()
    }

    /// The real load: `tickets.json` in the note store dir, so it follows the
    /// same three-tier resolution the note files use.
    pub fn load() -> Self {
        crate::state::store_dir().map(|d| Self::load_in(&d)).unwrap_or_default()
    }

    pub fn has_prefix(&self, prefix: &str) -> bool {
        self.map.contains_key(prefix)
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// The URL for `key`, or `None` when its prefix is unmapped. Pure — this is the
/// tested seam; `open` itself stays thin and untested.
pub fn ticket_url(cfg: &Config, key: &str) -> Option<String> {
    let (prefix, _) = key.split_once('-')?;
    Some(cfg.map.get(prefix)?.replace("{key}", key))
}

#[cfg(test)]
mod tests {
    use super::{Config, FILE, ticket_url};

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
}
