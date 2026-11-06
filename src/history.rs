//! History and remembered options, kept in a SQLite file so that they survive
//! restarts. Losing the file only loses history, so every failure here leaves
//! the program working with an in-memory database.

use crate::search::SearchOptions;
use rusqlite::{params, Connection};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Items kept per kind, not counting locked ones.
const LIMIT: i64 = 1000;

/// What a history list holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Command,
    Folder,
    View,
    Search,
    Replace,
    Mask,
    FindMask,
    FindText,
    Target,
}

impl Kind {
    fn key(self) -> &'static str {
        match self {
            Kind::Command => "command",
            Kind::Folder => "folder",
            Kind::View => "view",
            Kind::Search => "search",
            Kind::Replace => "replace",
            Kind::Mask => "mask",
            Kind::FindMask => "find_mask",
            Kind::FindText => "find_text",
            Kind::Target => "target",
        }
    }
}

/// The history database.
pub struct History {
    db: Connection,
}

/// Where the database lives, following the XDG convention.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::util::home_dir().map(|h| h.join(".local/share")))?;
    Some(base.join("far_linux/history.db"))
}

fn init(db: Connection) -> rusqlite::Result<History> {
    db.execute_batch(
        "PRAGMA busy_timeout = 2000;
         CREATE TABLE IF NOT EXISTS history (
             kind TEXT NOT NULL,
             value TEXT NOT NULL,
             used INTEGER NOT NULL,
             locked INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY (kind, value)
         );
         CREATE TABLE IF NOT EXISTS settings (
             key TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );",
    )?;
    Ok(History { db })
}

impl History {
    /// Opens the file. Commands can hold secrets, so only the owner can read it.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir);
        }
        let db = Connection::open(path)?;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        init(db)
    }

    /// A database that disappears on exit, used when the file cannot be opened.
    pub fn in_memory() -> Self {
        let db = Connection::open_in_memory().expect("in-memory SQLite always opens");
        init(db).expect("schema is valid")
    }

    /// Opens the default file, falling back to memory.
    pub fn open_default() -> Self {
        default_path()
            .and_then(|path| Self::open(&path).ok())
            .unwrap_or_else(Self::in_memory)
    }

    /// Records a use, moving an existing value to the most recent position.
    pub fn add(&self, kind: Kind, value: &str) {
        if value.is_empty() {
            return;
        }
        let key = kind.key();
        let _ = self.db.execute(
            "INSERT INTO history (kind, value, used)
             VALUES (?1, ?2, (SELECT COALESCE(MAX(used), 0) + 1 FROM history))
             ON CONFLICT (kind, value) DO UPDATE SET used = excluded.used",
            params![key, value],
        );
        let _ = self.db.execute(
            "DELETE FROM history WHERE kind = ?1 AND locked = 0 AND value NOT IN (
                 SELECT value FROM history WHERE kind = ?1 ORDER BY used DESC LIMIT ?2
             )",
            params![key, LIMIT],
        );
    }

    /// Lists values, newest first.
    pub fn list(&self, kind: Kind) -> Vec<String> {
        let Ok(mut query) = self
            .db
            .prepare_cached("SELECT value FROM history WHERE kind = ?1 ORDER BY used DESC")
        else {
            return Vec::new();
        };
        let rows = query.query_map(params![kind.key()], |row| row.get(0));
        rows.map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// Lists values with their locked state, newest first.
    pub fn items(&self, kind: Kind) -> Vec<(String, bool)> {
        let Ok(mut query) = self
            .db
            .prepare_cached("SELECT value, locked FROM history WHERE kind = ?1 ORDER BY used DESC")
        else {
            return Vec::new();
        };
        let rows = query.query_map(params![kind.key()], |row| {
            Ok((row.get(0)?, row.get::<_, i64>(1)? != 0))
        });
        rows.map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// Locks or unlocks a value. Locked values are not pushed out or cleared.
    pub fn toggle_lock(&self, kind: Kind, value: &str) {
        let _ = self.db.execute(
            "UPDATE history SET locked = 1 - locked WHERE kind = ?1 AND value = ?2",
            params![kind.key(), value],
        );
    }

    /// The options and text of the last search, shared by editor and viewer.
    pub fn load_search(&self) -> SearchOptions {
        SearchOptions {
            text: self.last(Kind::Search).unwrap_or_default(),
            replace: self.last(Kind::Replace).unwrap_or_default(),
            case_sensitive: self.flag("search.case", false),
            whole_words: self.flag("search.words", false),
            fuzzy: self.flag("search.fuzzy", false),
            regex: self.flag("search.regex", false),
            preserve_style: self.flag("search.style", false),
        }
    }

    /// Remembers a search that was just started.
    pub fn save_search(&self, o: &SearchOptions, replacing: bool) {
        self.add(Kind::Search, &o.text);
        if replacing {
            self.add(Kind::Replace, &o.replace);
            self.set_flag("search.style", o.preserve_style);
        }
        self.set_flag("search.case", o.case_sensitive);
        self.set_flag("search.words", o.whole_words);
        self.set_flag("search.fuzzy", o.fuzzy);
        self.set_flag("search.regex", o.regex);
    }

    /// Reads a remembered text option.
    pub fn setting(&self, key: &str) -> Option<String> {
        self.db
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .ok()
    }

    /// Remembers a text option.
    pub fn set_setting(&self, key: &str, value: &str) {
        let _ = self.db.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        );
    }

    /// The most recent value, if any.
    pub fn last(&self, kind: Kind) -> Option<String> {
        self.list(kind).into_iter().next()
    }

    /// Removes one value.
    pub fn remove(&self, kind: Kind, value: &str) {
        let _ = self.db.execute(
            "DELETE FROM history WHERE kind = ?1 AND value = ?2",
            params![kind.key(), value],
        );
    }

    /// Removes all values of a kind, except locked ones.
    pub fn clear(&self, kind: Kind) {
        let _ = self.db.execute(
            "DELETE FROM history WHERE kind = ?1 AND locked = 0",
            params![kind.key()],
        );
    }

    /// Reads a remembered true or false option.
    pub fn flag(&self, key: &str, default: bool) -> bool {
        self.db
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .map_or(default, |value| value == "1")
    }

    /// Remembers a true or false option.
    pub fn set_flag(&self, key: &str, value: bool) {
        let _ = self.db.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, if value { "1" } else { "0" }],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_first_and_reuse_moves_to_the_front() {
        let h = History::in_memory();
        h.add(Kind::Search, "one");
        h.add(Kind::Search, "two");
        h.add(Kind::Search, "one");
        h.add(Kind::Search, "");
        assert_eq!(h.list(Kind::Search), ["one", "two"]);
        assert_eq!(h.last(Kind::Search).as_deref(), Some("one"));
    }

    #[test]
    fn locked_items_survive_clearing_and_the_limit() {
        let h = History::in_memory();
        h.add(Kind::Search, "kept");
        h.toggle_lock(Kind::Search, "kept");
        h.add(Kind::Search, "other");
        for i in 0..LIMIT + 2 {
            h.add(Kind::Search, &format!("n{i}"));
        }
        assert!(h.list(Kind::Search).contains(&"kept".to_string()));
        h.clear(Kind::Search);
        assert_eq!(h.items(Kind::Search), [("kept".to_string(), true)]);
        h.toggle_lock(Kind::Search, "kept");
        h.clear(Kind::Search);
        assert!(h.list(Kind::Search).is_empty());
    }

    #[test]
    fn search_options_are_remembered() {
        let h = History::in_memory();
        let o = SearchOptions {
            text: "needle".into(),
            replace: "pin".into(),
            regex: true,
            case_sensitive: true,
            ..Default::default()
        };
        h.save_search(&o, true);
        let back = h.load_search();
        assert_eq!(back.text, "needle");
        assert_eq!(back.replace, "pin");
        assert!(back.regex && back.case_sensitive && !back.fuzzy);
    }

    #[test]
    fn kinds_are_separate() {
        let h = History::in_memory();
        h.add(Kind::Search, "x");
        assert!(h.list(Kind::Command).is_empty());
        assert_eq!(h.last(Kind::Replace), None);
    }

    #[test]
    fn values_with_quotes_and_unicode_are_stored_exactly() {
        let h = History::in_memory();
        let value = "it's; DROP TABLE history -- ж\n";
        h.add(Kind::Command, value);
        assert_eq!(h.list(Kind::Command), [value]);
    }

    #[test]
    fn old_items_are_pushed_out_at_the_limit() {
        let h = History::in_memory();
        for i in 0..LIMIT + 5 {
            h.add(Kind::Command, &format!("cmd{i}"));
        }
        let list = h.list(Kind::Command);
        assert_eq!(list.len() as i64, LIMIT);
        assert_eq!(list[0], format!("cmd{}", LIMIT + 4));
        assert!(!list.contains(&"cmd0".to_string()));
    }

    #[test]
    fn remove_and_clear() {
        let h = History::in_memory();
        h.add(Kind::Mask, "a");
        h.add(Kind::Mask, "b");
        h.remove(Kind::Mask, "a");
        assert_eq!(h.list(Kind::Mask), ["b"]);
        h.clear(Kind::Mask);
        assert!(h.list(Kind::Mask).is_empty());
    }

    #[test]
    fn data_survives_reopening_and_the_file_is_private() {
        let dir = std::env::temp_dir().join(format!("far_hist_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("sub/history.db");
        {
            let h = History::open(&path).unwrap();
            h.add(Kind::Search, "kept");
            h.set_flag("case", true);
        }
        let h = History::open(&path).unwrap();
        assert_eq!(h.list(Kind::Search), ["kept"]);
        assert!(h.flag("case", false));
        assert!(!h.flag("missing", false));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_a_crash() {
        let path = std::env::temp_dir().join(format!("far_hist_bad_{}", std::process::id()));
        fs::write(
            &path,
            "this is not a database, and it is long enough to be read",
        )
        .unwrap();
        assert!(History::open(&path).is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn text_settings_are_kept_and_replaced() {
        let h = History::in_memory();
        assert_eq!(h.setting("key"), None);
        h.set_setting("key", "one");
        h.set_setting("key", "two");
        assert_eq!(h.setting("key").as_deref(), Some("two"));
    }
}
