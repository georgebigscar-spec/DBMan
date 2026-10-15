//! Saved connections and preferences, as JSON in the user's config directory.

use std::path::PathBuf;

use dbman_core::ConnectionConfig;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub connections: Vec<ConnectionConfig>,
    #[serde(default = "yes")]
    pub dark: bool,
    /// Rows fetched per page in data tabs and per statement in consoles.
    #[serde(default = "page_size")]
    pub page_size: usize,
}

fn yes() -> bool {
    true
}

fn page_size() -> usize {
    500
}

impl Default for Settings {
    fn default() -> Self {
        Self { connections: Vec::new(), dark: true, page_size: page_size() }
    }
}

impl Settings {
    fn path() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("DBMAN_CONFIG") {
            return Some(PathBuf::from(p));
        }
        directories::ProjectDirs::from("", "", "DBMan").map(|d| d.config_dir().join("settings.json"))
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path().ok_or("no config directory")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let mut copy = self.clone();
        for c in &mut copy.connections {
            if !c.save_password {
                c.password.clear();
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
