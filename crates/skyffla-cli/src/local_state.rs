use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::accept_policy::AutoAcceptPolicy;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownPeerRecord {
    pub peer_name: String,
    pub first_seen_unix: u64,
    pub last_seen_unix: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalState {
    #[serde(default)]
    pub history: Vec<String>,
    #[serde(default)]
    pub auto_accept_policy: AutoAcceptPolicy,
    #[serde(default)]
    pub local_identity_secret_hex: Option<String>,
    #[serde(skip_serializing, default)]
    pub auto_accept_enabled: bool,
    #[serde(default)]
    pub known_peers: HashMap<String, KnownPeerRecord>,
}

fn local_state_dir_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".skyffla"))
}

pub fn local_state_file_path() -> Option<PathBuf> {
    local_state_dir_path().map(|dir| dir.join("state.json"))
}

fn legacy_history_file_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".skyffla_history"))
}

fn legacy_auto_accept_file_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".skyffla_autoaccept"))
}

fn legacy_known_peers_file_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".skyffla_known_peers.json"))
}

pub fn load_local_state(path: &Option<PathBuf>) -> LocalState {
    let Some(path) = path else {
        return LocalState::default();
    };

    match std::fs::read_to_string(path) {
        Ok(contents) => normalize_local_state(serde_json::from_str(&contents).unwrap_or_default()),
        Err(_) => migrate_legacy_local_state(),
    }
}

pub fn update_local_state(path: &Option<PathBuf>, update: impl FnOnce(&mut LocalState)) {
    let Some(path) = path else {
        return;
    };

    let mut state = load_local_state(&Some(path.clone()));
    update(&mut state);
    save_local_state(&Some(path.clone()), &state);
}

pub fn save_local_state(path: &Option<PathBuf>, state: &LocalState) {
    let Some(path) = path else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(contents) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(path, contents);
    }
}

fn migrate_legacy_local_state() -> LocalState {
    let history = legacy_history_file_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|contents| {
            contents
                .lines()
                .map(str::trim_end)
                .filter(|line| !line.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();

    let auto_accept_enabled = legacy_auto_accept_file_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|contents| matches!(contents.trim(), "on" | "true" | "1"))
        .unwrap_or(false);

    let known_peers = legacy_known_peers_file_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default();

    let state = LocalState {
        history,
        auto_accept_policy: if auto_accept_enabled {
            AutoAcceptPolicy::files_and_clipboard()
        } else {
            AutoAcceptPolicy::none()
        },
        local_identity_secret_hex: None,
        auto_accept_enabled: false,
        known_peers,
    };
    save_local_state(&local_state_file_path(), &state);
    state
}

fn normalize_local_state(mut state: LocalState) -> LocalState {
    if state.auto_accept_policy.is_empty() && state.auto_accept_enabled {
        state.auto_accept_policy = AutoAcceptPolicy::files_and_clipboard();
    }
    state.auto_accept_enabled = false;
    state
}

#[cfg(test)]
mod tests {
    use super::{normalize_local_state, LocalState};
    use crate::accept_policy::AutoAcceptPolicy;

    #[test]
    fn legacy_auto_accept_bool_upgrades_to_policy() {
        let state = normalize_local_state(LocalState {
            auto_accept_enabled: true,
            ..LocalState::default()
        });

        assert_eq!(
            state.auto_accept_policy,
            AutoAcceptPolicy::files_and_clipboard()
        );
        assert!(!state.auto_accept_enabled);
    }

    #[test]
    fn explicit_policy_is_preserved() {
        let policy = AutoAcceptPolicy {
            file: true,
            folder: true,
            clipboard: false,
        };
        let state = normalize_local_state(LocalState {
            auto_accept_policy: policy.clone(),
            auto_accept_enabled: true,
            ..LocalState::default()
        });

        assert_eq!(state.auto_accept_policy, policy);
        assert!(!state.auto_accept_enabled);
    }
}
