use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
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

pub fn load_local_state(path: &Option<PathBuf>) -> Result<LocalState> {
    let Some(path) = path else {
        return Ok(LocalState::default());
    };

    with_local_state_lock(path, || load_local_state_unlocked(path))
}

pub fn update_local_state<T>(
    path: &Option<PathBuf>,
    update: impl FnOnce(&mut LocalState) -> T,
) -> Result<T> {
    let Some(path) = path else {
        let mut state = LocalState::default();
        return Ok(update(&mut state));
    };

    with_local_state_lock(path, || {
        let mut state = load_local_state_unlocked(path)?;
        let output = update(&mut state);
        save_local_state_unlocked(path, &state)?;
        Ok(output)
    })
}

fn load_local_state_unlocked(path: &PathBuf) -> Result<LocalState> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let state = serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse local state file {}", path.display()))?;
            Ok(normalize_local_state(state))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            migrate_legacy_local_state(path)
        }
        Err(error) => Err(error)
            .with_context(|| format!("failed to read local state file {}", path.display())),
    }
}

fn save_local_state_unlocked(path: &PathBuf, state: &LocalState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create local state directory {}",
                parent.display()
            )
        })?;
    }

    let contents =
        serde_json::to_string_pretty(state).context("failed to serialize local state")?;
    write_local_state_atomically(path, contents.as_bytes())
}

fn write_local_state_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let temp_path = unique_local_state_temp_path(path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .with_context(|| {
            format!(
                "failed to create temporary local state file {}",
                temp_path.display()
            )
        })?;

    let write_result = (|| -> Result<()> {
        file.write_all(contents).with_context(|| {
            format!(
                "failed to write temporary local state file {}",
                temp_path.display()
            )
        })?;
        file.sync_all().with_context(|| {
            format!(
                "failed to sync temporary local state file {}",
                temp_path.display()
            )
        })?;
        Ok(())
    })();
    drop(file);

    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }

    if let Err(error) = std::fs::rename(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error).with_context(|| {
            format!(
                "failed to replace local state file {} from temporary file {}",
                path.display(),
                temp_path.display()
            )
        });
    }

    sync_local_state_parent_dir(path)?;
    Ok(())
}

fn unique_local_state_temp_path(path: &Path) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.json");
    path.with_file_name(format!(".{file_name}.tmp-{}-{nonce}", std::process::id()))
}

#[cfg(unix)]
fn sync_local_state_parent_dir(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let dir = File::open(parent)
        .with_context(|| format!("failed to open local state directory {}", parent.display()))?;
    dir.sync_all()
        .with_context(|| format!("failed to sync local state directory {}", parent.display()))
}

#[cfg(not(unix))]
fn sync_local_state_parent_dir(_path: &Path) -> Result<()> {
    Ok(())
}

fn migrate_legacy_local_state(path: &PathBuf) -> Result<LocalState> {
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
    save_local_state_unlocked(path, &state)?;
    Ok(state)
}

fn with_local_state_lock<T>(path: &Path, action: impl FnOnce() -> Result<T>) -> Result<T> {
    let Some(parent) = path.parent() else {
        return action();
    };
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create local state directory {}",
            parent.display()
        )
    })?;
    let lock_path = local_state_lock_file_path(path);
    let _lock = open_local_state_lock(&lock_path)?;
    action()
}

fn local_state_lock_file_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "state.json".into());
    name.push(".lock");
    path.with_file_name(name)
}

fn open_local_state_lock(lock_path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .with_context(|| format!("failed to open local state lock {}", lock_path.display()))?;
    file.lock()
        .with_context(|| format!("failed to lock local state lock {}", lock_path.display()))?;
    Ok(file)
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
    use super::{load_local_state, normalize_local_state, LocalState};
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

    #[test]
    fn invalid_json_is_reported_instead_of_resetting_state() {
        let path = std::env::temp_dir().join(format!(
            "skyffla-state-invalid-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos()
        ));
        std::fs::write(&path, "{invalid json").expect("invalid file should be written");

        let error = load_local_state(&Some(path.clone())).expect_err("invalid json should fail");
        assert!(error
            .to_string()
            .contains("failed to parse local state file"));

        let _ = std::fs::remove_file(path);
    }
}
