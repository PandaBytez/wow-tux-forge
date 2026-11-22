use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    pub addons_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledAddon {
    pub tag: String,
    pub asset_name: String,
    pub folders: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    pub addons: BTreeMap<String, InstalledAddon>,
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("wow-tux-forge")
        .join("config.json")
}

pub fn state_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("wow-tux-forge")
        .join("state.json")
}

fn load_json<T: DeserializeOwned + Default>(path: &Path) -> T {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_json<T: Serialize>(path: &Path, val: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create dir: {e}"))?;
    }
    let json = serde_json::to_string_pretty(val).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|e| format!("Failed to write {}: {e}", path.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("Failed to save {}: {e}", path.display()))
}

pub fn load_config() -> Config {
    load_json(&config_path())
}

pub fn save_config(cfg: &Config) -> Result<(), String> {
    save_json(&config_path(), cfg)
}

pub fn load_state() -> State {
    load_json(&state_path())
}

pub fn save_state(state: &State) -> Result<(), String> {
    save_json(&state_path(), state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_state_atomically() {
        let tmp = std::env::temp_dir().join(format!("wow-tux-forge-storage-{}.json", std::process::id()));
        let mut state = State::default();
        state.addons.insert(
            "Foo/Bar".to_string(),
            InstalledAddon {
                tag: "v1.0.0".to_string(),
                asset_name: "Bar.zip".to_string(),
                folders: vec!["Bar".to_string()],
            },
        );
        save_json(&tmp, &state).unwrap();
        let loaded: State = load_json(&tmp);
        assert_eq!(loaded.addons.len(), 1);
        assert_eq!(loaded.addons["Foo/Bar"].tag, "v1.0.0");
        assert!(!tmp.with_extension("json.tmp").exists());
        let _ = fs::remove_file(&tmp);
    }
}
