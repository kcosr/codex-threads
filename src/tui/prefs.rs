use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use fd_lock::RwLock;
use serde::{Deserialize, Serialize};

use crate::annotations::resolve_state_path_from;
use crate::cli::SortKey;
use crate::config::home_dir;

const PREFS_VERSION: u32 = 1;
const PREFS_FILE: &str = "tui.json";
const LOCK_FILE: &str = "tui.json.lock";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TuiPrefs {
    pub version: u32,
    #[serde(rename = "visibleColumns")]
    pub visible_columns: VisibleColumns,
    #[serde(rename = "sortKey")]
    pub sort_key: Option<SortKey>,
    #[serde(rename = "sortDescending")]
    pub sort_descending: bool,
    #[serde(rename = "autoRefresh")]
    pub auto_refresh: bool,
    #[serde(rename = "autoRefreshSeconds")]
    pub auto_refresh_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VisibleColumns {
    pub status: bool,
    pub updated: bool,
    pub cwd: bool,
    pub annotation: bool,
}

impl Default for TuiPrefs {
    fn default() -> Self {
        Self {
            version: PREFS_VERSION,
            visible_columns: VisibleColumns::default(),
            sort_key: Some(SortKey::Updated),
            sort_descending: true,
            auto_refresh: false,
            auto_refresh_seconds: 30,
        }
    }
}

impl Default for VisibleColumns {
    fn default() -> Self {
        Self {
            status: true,
            updated: true,
            cwd: true,
            annotation: true,
        }
    }
}

pub fn prefs_path() -> PathBuf {
    resolve_prefs_path_from(
        std::env::var("CODEX_THREADS_STATE").ok().as_deref(),
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        &home_dir(),
    )
}

pub fn resolve_prefs_path_from(
    state_dir_env: Option<&str>,
    xdg_state_home: Option<&str>,
    home: &Path,
) -> PathBuf {
    let annotations_path = resolve_state_path_from(state_dir_env, xdg_state_home, home);
    annotations_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(PREFS_FILE)
}

pub fn load_prefs() -> TuiPrefs {
    read_prefs().unwrap_or_default()
}

pub fn save_prefs(prefs: &TuiPrefs) -> Result<()> {
    let path = prefs_path();
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("TUI prefs path has no parent: `{}`", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create TUI prefs dir `{}`", parent.display()))?;
    let mut lock = prefs_lock(&path)?;
    let _guard = lock.write()?;
    write_prefs_atomic(&path, prefs)
}

fn read_prefs() -> Result<TuiPrefs> {
    let path = prefs_path();
    if !path.exists() {
        return Ok(TuiPrefs::default());
    }
    let lock = prefs_lock(&path)?;
    let _guard = lock.read()?;
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read TUI prefs `{}`", path.display()))?;
    let prefs: TuiPrefs = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse TUI prefs `{}`", path.display()))?;
    if prefs.version != PREFS_VERSION {
        return Ok(TuiPrefs::default());
    }
    Ok(prefs)
}

fn prefs_lock(path: &Path) -> Result<RwLock<File>> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("TUI prefs path has no parent: `{}`", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create TUI prefs dir `{}`", parent.display()))?;
    let lock_path = parent.join(LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open TUI prefs lock `{}`", lock_path.display()))?;
    Ok(RwLock::new(file))
}

fn write_prefs_atomic(path: &Path, prefs: &TuiPrefs) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("TUI prefs path has no parent: `{}`", path.display()))?;
    let temp_path = parent.join(format!(".{}.{}.tmp", PREFS_FILE, std::process::id()));
    let write_result = (|| -> Result<()> {
        let mut file = File::create(&temp_path).with_context(|| {
            format!(
                "failed to create temporary TUI prefs `{}`",
                temp_path.display()
            )
        })?;
        serde_json::to_writer_pretty(&mut file, prefs)
            .with_context(|| format!("failed to write TUI prefs `{}`", temp_path.display()))?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }
    if let Err(err) = fs::rename(&temp_path, path).with_context(|| {
        format!(
            "failed to replace TUI prefs `{}` with `{}`",
            path.display(),
            temp_path.display()
        )
    }) {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_path_uses_shared_state_dir() {
        assert_eq!(
            resolve_prefs_path_from(
                Some("/tmp/state"),
                Some("/tmp/xdg"),
                Path::new("/home/tester")
            ),
            PathBuf::from("/tmp/state/tui.json")
        );
        assert_eq!(
            resolve_prefs_path_from(None, Some("/tmp/xdg"), Path::new("/home/tester")),
            PathBuf::from("/tmp/xdg/codex-threads/tui.json")
        );
        assert_eq!(
            resolve_prefs_path_from(None, None, Path::new("/home/tester")),
            PathBuf::from("/home/tester/.local/state/codex-threads/tui.json")
        );
    }
}
