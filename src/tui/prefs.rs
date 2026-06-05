use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
    pub browser: BrowserPrefs,
    pub detail: DetailPrefs,
    pub refresh: RefreshPrefs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedPrefs {
    pub prefs: TuiPrefs,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserPrefs {
    pub columns: VisibleColumns,
    pub sort: Option<SortKey>,
    pub direction: SortDirectionPref,
    #[serde(rename = "previewPane")]
    pub preview_pane: bool,
    #[serde(rename = "relativeUpdated")]
    pub relative_updated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DetailPrefs {
    #[serde(rename = "messageMode")]
    pub message_mode: MessageMode,
    pub wrap: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefreshPrefs {
    pub auto: bool,
    #[serde(rename = "intervalSeconds")]
    pub interval_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VisibleColumns {
    pub status: bool,
    pub updated: bool,
    pub cwd: bool,
    pub annotation: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SortDirectionPref {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageMode {
    Summary,
}

impl Default for TuiPrefs {
    fn default() -> Self {
        Self {
            version: PREFS_VERSION,
            updated_at: now_epoch_seconds(),
            browser: BrowserPrefs::default(),
            detail: DetailPrefs::default(),
            refresh: RefreshPrefs::default(),
        }
    }
}

impl Default for BrowserPrefs {
    fn default() -> Self {
        Self {
            columns: VisibleColumns::default(),
            sort: Some(SortKey::Updated),
            direction: SortDirectionPref::Desc,
            preview_pane: true,
            relative_updated: true,
        }
    }
}

impl Default for DetailPrefs {
    fn default() -> Self {
        Self {
            message_mode: MessageMode::Summary,
            wrap: true,
        }
    }
}

impl Default for RefreshPrefs {
    fn default() -> Self {
        Self {
            auto: false,
            interval_seconds: 30,
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

pub fn load_prefs_with_warning() -> LoadedPrefs {
    match read_prefs() {
        Ok(prefs) => LoadedPrefs {
            prefs,
            warning: None,
        },
        Err(err) => LoadedPrefs {
            prefs: TuiPrefs::default(),
            warning: Some(format!("TUI prefs reset: {err:#}")),
        },
    }
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
    let mut prefs = prefs.clone();
    prefs.version = PREFS_VERSION;
    prefs.updated_at = now_epoch_seconds();
    write_prefs_atomic(&path, &prefs)
}

fn read_prefs() -> Result<TuiPrefs> {
    let path = prefs_path();
    read_prefs_from_path(&path)
}

fn read_prefs_from_path(path: &Path) -> Result<TuiPrefs> {
    if !path.exists() {
        return Ok(TuiPrefs::default());
    }
    let lock = prefs_lock(path)?;
    let _guard = lock.read()?;
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            backup_corrupt_prefs(path);
            return Err(err)
                .with_context(|| format!("failed to read TUI prefs `{}`", path.display()));
        }
    };
    let prefs: TuiPrefs = match serde_json::from_str(&text) {
        Ok(prefs) => prefs,
        Err(err) => {
            backup_corrupt_prefs(path);
            return Err(err)
                .with_context(|| format!("failed to parse TUI prefs `{}`", path.display()));
        }
    };
    if prefs.version != PREFS_VERSION {
        backup_corrupt_prefs(path);
        return Err(anyhow!(
            "unsupported TUI prefs version {}; expected {PREFS_VERSION}",
            prefs.version
        ));
    }
    Ok(prefs)
}

fn backup_corrupt_prefs(path: &Path) {
    if !path.exists() {
        return;
    }
    let backup = path.with_file_name(format!("{PREFS_FILE}.corrupt.{}", now_epoch_seconds()));
    let _ = fs::rename(path, backup);
}

fn now_epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
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
    use tempfile::TempDir;

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

    #[test]
    fn prefs_schema_matches_nested_design_shape() {
        let prefs = TuiPrefs::default();
        let value = serde_json::to_value(&prefs).expect("prefs json");
        assert_eq!(value["version"], PREFS_VERSION);
        assert!(value["updatedAt"].is_i64());
        assert_eq!(value["browser"]["columns"]["status"], true);
        assert_eq!(value["browser"]["sort"], "updated");
        assert_eq!(value["browser"]["direction"], "desc");
        assert_eq!(value["browser"]["previewPane"], true);
        assert_eq!(value["browser"]["relativeUpdated"], true);
        assert_eq!(value["detail"]["messageMode"], "summary");
        assert_eq!(value["detail"]["wrap"], true);
        assert_eq!(value["refresh"]["auto"], false);
        assert_eq!(value["refresh"]["intervalSeconds"], 30);
        assert!(value.get("visibleColumns").is_none());
        assert!(value.get("sortKey").is_none());
    }

    #[test]
    fn corrupt_prefs_are_backed_up_and_defaulted() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join(PREFS_FILE);
        fs::write(&path, "{not json").expect("prefs");
        let prefs = read_prefs_from_path(&path).unwrap_or_default();
        assert_eq!(prefs, TuiPrefs::default());
        assert!(!path.exists());
        let backups = fs::read_dir(temp.path())
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("tui.json.corrupt.")
            })
            .count();
        assert_eq!(backups, 1);
    }
}
