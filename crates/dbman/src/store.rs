//! Saved connections and preferences, as JSON in the user's config directory,
//! and the query history next to them. Saved passwords go to the system
//! keychain when there is one, and into the JSON file otherwise.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use dbman_core::ConnectionConfig;
use serde::{Deserialize, Serialize};

use crate::i18n::{tr, trf};

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
        let mut s: Self = Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if let Some(k) = keychain() {
            s.read_secrets(k);
        }
        s
    }

    /// Fills in the passwords kept in `store`.
    pub fn read_secrets(&mut self, store: &dyn Secrets) {
        for c in &mut self.connections {
            let id = c.id.to_string();
            if !c.save_password {
                continue;
            }
            let Ok(Some(json)) = store.get(&id) else { continue };
            let Ok(saved) = serde_json::from_str::<Saved>(&json) else { continue };
            // A password still in the file is newer: it moves to the keychain on the next save.
            if c.password.is_empty() && c.ssh.password.is_empty() {
                c.password = saved.password;
                c.ssh.password = saved.ssh;
            }
            WRITTEN.lock().unwrap().insert(id, json);
        }
    }

    /// Writes the settings; passwords go to the system keychain when there is one.
    /// `Ok(Some(warning))` when a password could not go there and stayed in the file.
    pub fn save(&self) -> Result<Option<String>, String> {
        self.save_with(keychain())
    }

    pub fn save_with(&self, store: Option<&dyn Secrets>) -> Result<Option<String>, String> {
        let path = Self::path().ok_or(tr("no config directory"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let mut copy = self.clone();
        let mut warning = None;
        for c in &mut copy.connections {
            if !c.save_password {
                c.password.clear();
                c.ssh.password.clear();
            }
            if let Some(store) = store {
                match store_secret(store, c) {
                    Ok(()) => {
                        c.password.clear();
                        c.ssh.password.clear();
                    }
                    Err(e) => {
                        warning = Some(trf(
                            "The system keychain did not take the password of {name}, so it is kept in settings.json: {error}",
                            &[("name", &c.display_name()), ("error", &e)],
                        ))
                    }
                }
            }
        }
        if let Some(store) = store {
            // Connections that were deleted.
            let ids: Vec<String> = copy.connections.iter().map(|c| c.id.to_string()).collect();
            let gone: Vec<String> = WRITTEN.lock().unwrap().keys().filter(|k| !ids.contains(k)).cloned().collect();
            for id in gone {
                if store.delete(&id).is_ok() {
                    WRITTEN.lock().unwrap().remove(&id);
                }
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
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        Ok(warning)
    }
}

/// The passwords of one connection, as one keychain item.
#[derive(Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    password: String,
    #[serde(default)]
    ssh: String,
}

/// What was last written to the keychain, by connection id, so unchanged passwords are not written again.
static WRITTEN: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);

/// Puts a connection's passwords into `store`, or removes them when there are none to keep.
fn store_secret(store: &dyn Secrets, c: &ConnectionConfig) -> Result<(), String> {
    let id = c.id.to_string();
    let empty = c.password.is_empty() && c.ssh.password.is_empty();
    if !c.save_password || empty {
        if WRITTEN.lock().unwrap().remove(&id).is_some() {
            store.delete(&id)?;
        }
        return Ok(());
    }
    let json = serde_json::to_string(&Saved { password: c.password.clone(), ssh: c.ssh.password.clone() })
        .map_err(|e| e.to_string())?;
    if WRITTEN.lock().unwrap().get(&id) == Some(&json) {
        return Ok(());
    }
    store.set(&id, &json)?;
    WRITTEN.lock().unwrap().insert(id, json);
    Ok(())
}

/// Where saved passwords are kept.
pub trait Secrets: Send + Sync {
    fn get(&self, id: &str) -> Result<Option<String>, String>;
    fn set(&self, id: &str, secret: &str) -> Result<(), String>;
    fn delete(&self, id: &str) -> Result<(), String>;
}

/// The name of DBMan's items in the system keychain.
const SERVICE: &str = "DBMan";

/// The system keychain: Keychain on macOS, Credential Manager on Windows, the Secret Service elsewhere.
struct Keychain;

impl Secrets for Keychain {
    fn get(&self, id: &str) -> Result<Option<String>, String> {
        match keyring::Entry::new(SERVICE, id).and_then(|e| e.get_password()) {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn set(&self, id: &str, secret: &str) -> Result<(), String> {
        keyring::Entry::new(SERVICE, id).and_then(|e| e.set_password(secret)).map_err(|e| e.to_string())
    }

    fn delete(&self, id: &str) -> Result<(), String> {
        match keyring::Entry::new(SERVICE, id).and_then(|e| e.delete_credential()) {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// The system keychain, unless there is none or `DBMAN_KEYCHAIN=off`.
pub fn keychain() -> Option<&'static dyn Secrets> {
    static AVAILABLE: LazyLock<bool> = LazyLock::new(|| {
        let off = std::env::var("DBMAN_KEYCHAIN").is_ok_and(|v| matches!(v.as_str(), "0" | "off" | "no" | "false"));
        // Reading a missing item shows whether the store answers at all.
        !off && keyring::Entry::store_status().is_ok() && Keychain.get("availability-check").is_ok()
    });
    if *AVAILABLE { Some(&Keychain) } else { None }
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

    #[derive(Default)]
    struct Memory(Mutex<HashMap<String, String>>, bool);

    impl Secrets for Memory {
        fn get(&self, id: &str) -> Result<Option<String>, String> {
            Ok(self.0.lock().unwrap().get(id).cloned())
        }
        fn set(&self, id: &str, secret: &str) -> Result<(), String> {
            if self.1 {
                return Err("locked".into());
            }
            self.0.lock().unwrap().insert(id.into(), secret.into());
            Ok(())
        }
        fn delete(&self, id: &str) -> Result<(), String> {
            self.0.lock().unwrap().remove(id);
            Ok(())
        }
    }

    #[test]
    fn passwords_go_to_the_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        // SAFETY: only this test reads DBMAN_CONFIG in this test binary.
        unsafe { std::env::set_var("DBMAN_CONFIG", &path) };
        let mut a = ConnectionConfig::new(dbman_core::DbKind::Postgres);
        a.password = "db secret".into();
        a.ssh.password = "ssh secret".into();
        let mut b = ConnectionConfig::new(dbman_core::DbKind::MySql);
        b.password = "not kept".into();
        b.save_password = false;
        let settings = Settings { connections: vec![a.clone(), b], ..Default::default() };
        let store = Memory::default();
        assert_eq!(settings.save_with(Some(&store)), Ok(None));

        let file = std::fs::read_to_string(&path).unwrap();
        assert!(!file.contains("secret") && !file.contains("not kept"), "{file}");
        assert_eq!(store.0.lock().unwrap().len(), 1);

        // Loading fills them back in.
        let mut back: Settings = serde_json::from_str(&file).unwrap();
        back.read_secrets(&store);
        assert_eq!(back.connections[0].password, "db secret");
        assert_eq!(back.connections[0].ssh.password, "ssh secret");
        assert_eq!(back.connections[1].password, "");

        // A deleted connection's item goes away.
        back.connections.remove(0);
        back.save_with(Some(&store)).unwrap();
        assert!(store.0.lock().unwrap().is_empty());

        // A keychain that refuses keeps the password in the file, with a warning.
        let locked = Memory(Mutex::default(), true);
        let warning = settings.save_with(Some(&locked)).unwrap();
        assert!(warning.is_some_and(|w| w.contains("kept in settings.json")));
        assert!(std::fs::read_to_string(&path).unwrap().contains("db secret"));
    }

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
