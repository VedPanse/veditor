//! Persistence helpers for session restore and user settings.

use std::{collections::BTreeMap, env, fs, io, path::PathBuf};

use crate::{GlobalSettings, NvimBufferState, SessionState, io_error, theme::normalize_hex_color};

/// Loads the last saved workspace session from disk.
pub(crate) fn load_saved_session() -> Option<SessionState> {
    let path = session_state_path()?;
    let contents = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
    let root = value.get("root")?.as_str()?;
    let root = fs::canonicalize(root).ok()?;
    if !root.is_dir() {
        return None;
    }

    let open_files = value
        .get("open_files")
        .and_then(serde_json::Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let active_file = value
        .get("active_file")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from);
    let accent_hex = value
        .get("accent_hex")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let accent_registry = value
        .get("accent_registry")
        .and_then(serde_json::Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .as_str()
                        .and_then(normalize_hex_color)
                        .map(|hex| (name.to_string(), hex))
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();

    Some(SessionState {
        root,
        open_files,
        active_file,
        accent_hex,
        accent_registry,
    })
}

/// Persists the workspace session snapshot to disk.
pub(crate) fn save_saved_session(session: &SessionState) -> io::Result<()> {
    let Some(path) = session_state_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let payload = serde_json::json!({
        "root": session.root.display().to_string(),
        "open_files": session.open_files.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
        "active_file": session.active_file.as_ref().map(|path| path.display().to_string()),
        "accent_hex": session.accent_hex,
        "accent_registry": session.accent_registry,
    });
    let contents = serde_json::to_string_pretty(&payload).map_err(io_error)?;
    fs::write(path, contents)
}

/// Loads global editor settings that apply across restored sessions.
pub(crate) fn load_global_settings() -> Option<GlobalSettings> {
    let path = settings_path()?;
    let contents = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
    let accent_hex = value
        .get("accent_hex")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let accent_registry = value
        .get("accent_registry")
        .and_then(serde_json::Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .as_str()
                        .and_then(normalize_hex_color)
                        .map(|hex| (name.to_string(), hex))
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();

    Some(GlobalSettings {
        accent_hex,
        accent_registry,
    })
}

/// Persists global settings while preserving existing accent aliases by name.
pub(crate) fn save_global_settings(settings: &GlobalSettings) -> io::Result<()> {
    let Some(path) = settings_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut accent_registry = load_global_settings()
        .map(|saved| saved.accent_registry)
        .unwrap_or_default();
    for (name, hex) in &settings.accent_registry {
        if let Some(existing_name) = accent_registry
            .keys()
            .find(|existing| existing.eq_ignore_ascii_case(name))
            .cloned()
        {
            accent_registry.remove(&existing_name);
        }
        accent_registry.insert(name.clone(), hex.clone());
    }

    let payload = serde_json::json!({
        "accent_hex": settings.accent_hex,
        "accent_registry": accent_registry,
    });
    let contents = serde_json::to_string_pretty(&payload).map_err(io_error)?;
    fs::write(path, contents)
}

/// Parses the temporary JSON file emitted by the embedded Neovim buffer dump hook.
pub(crate) fn parse_nvim_buffer_state(contents: &str) -> Option<NvimBufferState> {
    let value: serde_json::Value = serde_json::from_str(contents).ok()?;
    let files = value
        .get("files")
        .and_then(serde_json::Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let current = value
        .get("current")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);

    Some(NvimBufferState { files, current })
}

/// Returns the location of the current session snapshot file.
pub(crate) fn session_state_path() -> Option<PathBuf> {
    let home = env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".veditor").join("session.json"))
}

/// Returns the location of the global settings file.
pub(crate) fn settings_path() -> Option<PathBuf> {
    let home = env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".veditor").join("settings.json"))
}

/// Returns the temporary file used to snapshot embedded Neovim buffers.
pub(crate) fn nvim_snapshot_path() -> io::Result<PathBuf> {
    let Some(path) = session_state_path() else {
        return Err(io_error("home directory unavailable"));
    };
    let parent = path
        .parent()
        .ok_or_else(|| io_error("invalid session path"))?;
    Ok(parent.join("nvim-buffers.json"))
}
