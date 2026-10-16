//! Saved connections and preferences, as JSON in the user's config directory,
//! and the query history next to them.

use std::io::Write;
use std::path::PathBuf;

use dbman_core::ConnectionConfig;
use serde::{Deserialize, Serialize};

use crate::i18n::tr;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub connections: Vec<ConnectionConfig>,
    #[serde(default = "yes")]
    pub dark: bool,
    /// Rows fetched per page in data tabs and per statement in consoles.
    #[serde(default = "page_size")]
    pub page_size: usize,
    /// Where pg_dump, mysqldump and friends live, when not on `PATH`.
    #[serde(default)]
    pub tools_dir: String,
    /// Interface language code ("en", "ru").
    #[serde(default)]
    pub language: String,
    /// Script files opened or saved lately, newest first.
    #[serde(default)]
    pub recent_scripts: Vec<PathBuf>,
}

/// Script files kept in the recent list.
const RECENT: usize = 10;

fn yes() -> bool {
    true
}

fn page_size() -> usize {
    500
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            connections: Vec::new(),
            dark: true,
            page_size: page_size(),
            tools_dir: String::new(),
            language: String::new(),
            recent_scripts: Vec::new(),
        }
    }
}

impl Settings {
    fn path() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("DBMAN_CONFIG") {
            return Some(PathBuf::from(p));
        }
        directories::ProjectDirs::from("", "", "DBMan").map(|d| d.config_dir().join("settings.json"))
    }

    /// Where host keys of SSH servers are remembered.
    pub fn known_hosts_path() -> Option<PathBuf> {
        Self::path().map(|p| p.with_file_name("known_hosts"))
    }

    /// Puts a script file first in the recent list.
    pub fn add_recent(&mut self, path: PathBuf) {
        self.recent_scripts.retain(|p| *p != path);
        self.recent_scripts.insert(0, path);
        self.recent_scripts.truncate(RECENT);
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path().ok_or(tr("no config directory"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let mut copy = self.clone();
        for c in &mut copy.connections {
            if !c.save_password {
                c.password.clear();
                c.ssh.password.clear();
            }
        }
        let json = serde_json::to_string_pretty(&copy).map_err(|e| e.to_string())?;
        // Write then rename, so a crash never leaves a half-written file.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Passwords may be stored here.
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
    }
}

/// One statement run in a console.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub conn: uuid::Uuid,
    #[serde(default)]
    pub database: Option<String>,
    pub sql: String,
    /// When it finished, in seconds since the Unix epoch.
    pub at: i64,
    pub millis: u64,
    /// Rows, "done" or the error, as shown in the console.
    pub summary: String,
    pub ok: bool,
}

/// Statements kept; older ones are dropped.
const HISTORY: usize = 2000;
/// Longer statements are cut when stored.
const HISTORY_SQL: usize = 20_000;

/// The query history of every console, oldest first, kept as JSON lines.
#[derive(Default)]
pub struct History {
    pub entries: Vec<HistoryEntry>,
    path: Option<PathBuf>,
}

impl History {
    fn path() -> Option<PathBuf> {
        Settings::path().map(|p| p.with_file_name("history.jsonl"))
    }

    pub fn load() -> Self {
        let path = Self::path();
        let mut entries: Vec<HistoryEntry> = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
            .unwrap_or_default();
        let extra = entries.len().saturating_sub(HISTORY);
        entries.drain(..extra);
        Self { entries, path }
    }

    /// Adds a statement and appends it to the file; the file is rewritten
    /// when it holds too many.
    pub fn add(&mut self, mut e: HistoryEntry) -> Result<(), String> {
        if let Some((cut, _)) = e.sql.char_indices().nth(HISTORY_SQL) {
            e.sql.truncate(cut);
        }
        self.entries.push(e);
        let Some(path) = &self.path else { return Ok(()) };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let line = |e: &HistoryEntry| serde_json::to_string(e).unwrap_or_default() + "\n";
        if self.entries.len() > HISTORY + HISTORY / 4 {
            let extra = self.entries.len() - HISTORY;
            self.entries.drain(..extra);
            let all: String = self.entries.iter().map(line).collect();
            let tmp = path.with_extension("jsonl.tmp");
            std::fs::write(&tmp, all).map_err(|e| e.to_string())?;
            return std::fs::rename(&tmp, path).map_err(|e| e.to_string());
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Statements may hold data.
            let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        f.write_all(line(self.entries.last().expect("just pushed")).as_bytes()).map_err(|e| e.to_string())
    }

    /// Entries of a connection whose SQL contains `search` (any case), newest first.
    pub fn find<'a>(&'a self, conn: uuid::Uuid, search: &str) -> impl Iterator<Item = &'a HistoryEntry> + 'a {
        let search = search.to_lowercase();
        self.entries
            .iter()
            .rev()
            .filter(move |e| e.conn == conn && (search.is_empty() || e.sql.to_lowercase().contains(&search)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(conn: uuid::Uuid, sql: &str) -> HistoryEntry {
        HistoryEntry { conn, database: None, sql: sql.into(), at: 0, millis: 1, summary: String::new(), ok: true }
    }

    #[test]
    fn history_appends_trims_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let (a, b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let mut h = History { entries: Vec::new(), path: Some(path.clone()) };
        for i in 0..HISTORY + HISTORY / 4 + 1 {
            h.add(entry(if i % 2 == 0 { a } else { b }, &format!("select {i}"))).unwrap();
        }
        assert_eq!(h.entries.len(), HISTORY);
        let back: Vec<HistoryEntry> =
            std::fs::read_to_string(&path).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(back, h.entries);
        // The newest matches of `a` come first.
        let found: Vec<&str> = h.find(a, "SELECT 249").map(|e| e.sql.as_str()).take(2).collect();
        assert_eq!(found, ["select 2498", "select 2496"]);
    }
}
