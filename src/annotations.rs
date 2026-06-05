use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use fd_lock::RwLock;
use serde::{Deserialize, Serialize};

use crate::config::{Target, home_dir};

const STATE_VERSION: u32 = 1;
const STATE_FILE: &str = "annotations.json";
const LOCK_FILE: &str = "annotations.json.lock";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Annotation {
    pub text: String,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AnnotationNamespace {
    #[serde(rename = "displayServer")]
    pub display_server: String,
    pub endpoint: String,
    #[serde(default)]
    pub threads: BTreeMap<String, Annotation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AnnotationState {
    pub version: u32,
    #[serde(default)]
    pub namespaces: BTreeMap<String, AnnotationNamespace>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotationListItem {
    pub server: String,
    pub endpoint: String,
    pub thread_id: String,
    pub annotation: Annotation,
}

impl Default for AnnotationState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            namespaces: BTreeMap::new(),
        }
    }
}

pub fn state_path() -> PathBuf {
    resolve_state_path_from(
        env::var("CODEX_THREADS_STATE").ok().as_deref(),
        env::var("XDG_STATE_HOME").ok().as_deref(),
        &home_dir(),
    )
}

pub fn resolve_state_path_from(
    state_dir_env: Option<&str>,
    xdg_state_home: Option<&str>,
    home: &Path,
) -> PathBuf {
    let dir = if let Some(path) = state_dir_env {
        PathBuf::from(path)
    } else if let Some(path) = xdg_state_home {
        PathBuf::from(path).join("codex-threads")
    } else {
        home.join(".local/state/codex-threads")
    };
    dir.join(STATE_FILE)
}

pub fn now_epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn load_annotation(target: &Target, thread_id: &str) -> Result<Option<Annotation>> {
    let state = read_state()?;
    Ok(state
        .namespaces
        .get(&target.annotation_namespace())
        .and_then(|namespace| namespace.threads.get(thread_id))
        .cloned())
}

pub fn set_annotation(target: &Target, thread_id: &str, text: &str) -> Result<Annotation> {
    let text = text.trim();
    if text.is_empty() {
        return Err(anyhow!("annotation text must not be empty"));
    }
    let now = now_epoch_seconds();
    update_state(|state| {
        let namespace_key = target.annotation_namespace();
        let namespace = state
            .namespaces
            .entry(namespace_key.clone())
            .or_insert_with(|| AnnotationNamespace {
                display_server: target.server.clone(),
                endpoint: namespace_key.clone(),
                threads: BTreeMap::new(),
            });
        namespace.display_server = target.server.clone();
        namespace.endpoint = namespace_key;
        let created_at = namespace
            .threads
            .get(thread_id)
            .map(|annotation| annotation.created_at)
            .unwrap_or(now);
        let annotation = Annotation {
            text: text.to_string(),
            created_at,
            updated_at: now,
        };
        namespace
            .threads
            .insert(thread_id.to_string(), annotation.clone());
        Ok(annotation)
    })
}

pub fn clear_annotation(target: &Target, thread_id: &str) -> Result<bool> {
    update_state(|state| {
        let namespace_key = target.annotation_namespace();
        let cleared = state
            .namespaces
            .get_mut(&namespace_key)
            .and_then(|namespace| namespace.threads.remove(thread_id))
            .is_some();
        remove_empty_namespace(state, &namespace_key);
        Ok(cleared)
    })
}

pub fn clear_annotations(target: &Target, thread_ids: &[String]) -> Result<usize> {
    let removals = thread_ids.iter().cloned().collect::<BTreeSet<_>>();
    update_state(|state| {
        let namespace_key = target.annotation_namespace();
        let mut removed = 0;
        if let Some(namespace) = state.namespaces.get_mut(&namespace_key) {
            for thread_id in &removals {
                if namespace.threads.remove(thread_id).is_some() {
                    removed += 1;
                }
            }
        }
        remove_empty_namespace(state, &namespace_key);
        Ok(removed)
    })
}

pub fn list_annotations(target: &Target, query: Option<&str>) -> Result<Vec<AnnotationListItem>> {
    let state = read_state()?;
    let Some(namespace) = state.namespaces.get(&target.annotation_namespace()) else {
        return Ok(Vec::new());
    };
    let query = query.map(|value| value.to_lowercase());
    let mut items = namespace
        .threads
        .iter()
        .filter(|(_, annotation)| {
            query
                .as_ref()
                .is_none_or(|query| annotation.text.to_lowercase().contains(query))
        })
        .map(|(thread_id, annotation)| AnnotationListItem {
            server: target.server.clone(),
            endpoint: namespace.endpoint.clone(),
            thread_id: thread_id.clone(),
            annotation: annotation.clone(),
        })
        .collect::<Vec<_>>();
    items.sort_by(|left, right| {
        right
            .annotation
            .updated_at
            .cmp(&left.annotation.updated_at)
            .then_with(|| left.thread_id.cmp(&right.thread_id))
    });
    Ok(items)
}

pub fn namespace_annotations(target: &Target) -> Result<BTreeMap<String, Annotation>> {
    let state = read_state()?;
    Ok(state
        .namespaces
        .get(&target.annotation_namespace())
        .map(|namespace| namespace.threads.clone())
        .unwrap_or_default())
}

fn remove_empty_namespace(state: &mut AnnotationState, namespace_key: &str) {
    if state
        .namespaces
        .get(namespace_key)
        .is_some_and(|namespace| namespace.threads.is_empty())
    {
        state.namespaces.remove(namespace_key);
    }
}

fn read_state() -> Result<AnnotationState> {
    let path = state_path();
    if !path.exists() {
        return Ok(AnnotationState::default());
    }
    let lock = state_lock(&path)?;
    let _guard = lock.read()?;
    load_state_from_path(&path)
}

fn update_state<T>(mut update: impl FnMut(&mut AnnotationState) -> Result<T>) -> Result<T> {
    let path = state_path();
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("annotation state path has no parent: `{}`", path.display()))?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create annotation state dir `{}`",
            parent.display()
        )
    })?;
    let mut lock = state_lock(&path)?;
    let _guard = lock.write()?;
    let mut state = load_state_from_path(&path)?;
    let result = update(&mut state)?;
    validate_state(&state)?;
    write_state_atomic(&path, &state)?;
    Ok(result)
}

fn state_lock(path: &Path) -> Result<RwLock<File>> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("annotation state path has no parent: `{}`", path.display()))?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create annotation state dir `{}`",
            parent.display()
        )
    })?;
    let lock_path = parent.join(LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open annotation lock `{}`", lock_path.display()))?;
    Ok(RwLock::new(file))
}

fn load_state_from_path(path: &Path) -> Result<AnnotationState> {
    if !path.exists() {
        return Ok(AnnotationState::default());
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read annotation state `{}`", path.display()))?;
    let state: AnnotationState = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse annotation state `{}`", path.display()))?;
    validate_state(&state)?;
    Ok(state)
}

fn validate_state(state: &AnnotationState) -> Result<()> {
    if state.version != STATE_VERSION {
        return Err(anyhow!(
            "unsupported annotation state version {}; expected {STATE_VERSION}",
            state.version
        ));
    }
    Ok(())
}

fn write_state_atomic(path: &Path, state: &AnnotationState) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("annotation state path has no parent: `{}`", path.display()))?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create annotation state dir `{}`",
            parent.display()
        )
    })?;
    let temp_path = parent.join(format!(".{}.{}.tmp", STATE_FILE, std::process::id()));
    {
        let mut file = File::create(&temp_path).with_context(|| {
            format!(
                "failed to create temporary annotation state `{}`",
                temp_path.display()
            )
        })?;
        serde_json::to_writer_pretty(&mut file, state).with_context(|| {
            format!("failed to write annotation state `{}`", temp_path.display())
        })?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    fs::rename(&temp_path, path).with_context(|| {
        format!(
            "failed to replace annotation state `{}` with `{}`",
            path.display(),
            temp_path.display()
        )
    })?;
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Endpoint, Target};
    use std::sync::Mutex;
    use tempfile::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn target() -> Target {
        Target {
            server: "work".to_string(),
            endpoint: Endpoint::Unix {
                path: PathBuf::from("/tmp/codex.sock"),
            },
            model: None,
            model_reasoning_effort: None,
        }
    }

    fn with_state_dir<T>(f: impl FnOnce(&TempDir) -> T) -> T {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let temp = TempDir::new().expect("tempdir");
        // SAFETY: These tests run synchronously and only depend on this process env.
        unsafe {
            env::set_var("CODEX_THREADS_STATE", temp.path());
            env::remove_var("XDG_STATE_HOME");
        }
        let result = f(&temp);
        unsafe {
            env::remove_var("CODEX_THREADS_STATE");
        }
        result
    }

    #[test]
    fn state_path_precedence_prefers_state_env_then_xdg_then_default() {
        let home = PathBuf::from("/home/tester");
        assert_eq!(
            resolve_state_path_from(Some("/tmp/state"), Some("/tmp/xdg"), &home),
            PathBuf::from("/tmp/state/annotations.json")
        );
        assert_eq!(
            resolve_state_path_from(None, Some("/tmp/xdg"), &home),
            PathBuf::from("/tmp/xdg/codex-threads/annotations.json")
        );
        assert_eq!(
            resolve_state_path_from(None, None, &home),
            PathBuf::from("/home/tester/.local/state/codex-threads/annotations.json")
        );
    }

    #[test]
    fn set_get_list_search_and_clear_annotation() {
        with_state_dir(|_| {
            let target = target();
            let first = set_annotation(&target, "thread_1", "First note").unwrap();
            assert_eq!(first.text, "First note");
            let second = set_annotation(&target, "thread_1", "Release follow-up").unwrap();
            assert_eq!(second.created_at, first.created_at);
            assert_eq!(second.text, "Release follow-up");
            assert_eq!(
                load_annotation(&target, "thread_1").unwrap().unwrap().text,
                "Release follow-up"
            );
            assert_eq!(list_annotations(&target, None).unwrap().len(), 1);
            assert_eq!(
                list_annotations(&target, Some("release")).unwrap()[0].thread_id,
                "thread_1"
            );
            assert!(
                list_annotations(&target, Some("missing"))
                    .unwrap()
                    .is_empty()
            );
            assert!(clear_annotation(&target, "thread_1").unwrap());
            assert!(!clear_annotation(&target, "thread_1").unwrap());
            assert!(load_annotation(&target, "thread_1").unwrap().is_none());
        });
    }

    #[test]
    fn rejects_corrupt_state_without_overwriting() {
        with_state_dir(|temp| {
            let path = temp.path().join(STATE_FILE);
            fs::write(&path, "not json").unwrap();
            let err = load_annotation(&target(), "thread_1").unwrap_err();
            assert!(err.to_string().contains("failed to parse annotation state"));
            assert_eq!(fs::read_to_string(path).unwrap(), "not json");
        });
    }

    #[test]
    fn rejects_unknown_state_version() {
        with_state_dir(|temp| {
            let path = temp.path().join(STATE_FILE);
            fs::write(&path, r#"{"version":2,"namespaces":{}}"#).unwrap();
            let err = load_annotation(&target(), "thread_1").unwrap_err();
            assert!(
                err.to_string()
                    .contains("unsupported annotation state version")
            );
        });
    }

    #[test]
    fn clears_multiple_annotations() {
        with_state_dir(|_| {
            let target = target();
            set_annotation(&target, "thread_1", "one").unwrap();
            set_annotation(&target, "thread_2", "two").unwrap();
            let removed = clear_annotations(&target, &["thread_1".to_string()]).unwrap();
            assert_eq!(removed, 1);
            assert!(load_annotation(&target, "thread_1").unwrap().is_none());
            assert!(load_annotation(&target, "thread_2").unwrap().is_some());
        });
    }
}
