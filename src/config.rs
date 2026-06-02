use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_reasoning_effort: Option<String>,
    #[serde(default)]
    pub servers: BTreeMap<String, ServerConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    #[serde(rename = "type")]
    pub kind: String,
    pub path: PathBuf,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_reasoning_effort: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Target {
    pub server: String,
    pub path: PathBuf,
    pub model: Option<String>,
    pub model_reasoning_effort: Option<String>,
}

impl Target {
    pub fn configured(alias: &str, server: &ServerConfig, config: &AppConfig) -> Self {
        Self {
            server: alias.to_string(),
            path: server.path.clone(),
            model: server.model.clone().or_else(|| config.model.clone()),
            model_reasoning_effort: server
                .model_reasoning_effort
                .clone()
                .or_else(|| config.model_reasoning_effort.clone()),
        }
    }
}

pub fn load_config(path: &Path) -> Result<AppConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config `{}`", path.display()))?;
    let config: AppConfig = toml::from_str(&text)
        .with_context(|| format!("failed to parse config `{}`", path.display()))?;
    validate_config(&config)?;
    Ok(config)
}

pub fn validate_config(config: &AppConfig) -> Result<()> {
    validate_defaults(
        "global config",
        config.model.as_deref(),
        config.model_reasoning_effort.as_deref(),
    )?;
    for (alias, server) in &config.servers {
        if alias.trim().is_empty() {
            return Err(anyhow!("server alias must not be empty"));
        }
        if server.kind != "uds" {
            return Err(anyhow!(
                "server `{alias}` has unsupported type `{}`; only `uds` is supported",
                server.kind
            ));
        }
        if server.path.as_os_str().is_empty() {
            return Err(anyhow!("server `{alias}` is missing `path`"));
        }
        validate_defaults(
            &format!("server `{alias}`"),
            server.model.as_deref(),
            server.model_reasoning_effort.as_deref(),
        )?;
    }
    Ok(())
}

fn validate_defaults(scope: &str, model: Option<&str>, effort: Option<&str>) -> Result<()> {
    if model.is_some_and(|model| model.trim().is_empty()) {
        return Err(anyhow!("{scope} has empty `model`"));
    }
    if let Some(effort) = effort
        && !is_valid_reasoning_effort(effort)
    {
        return Err(anyhow!(
            "{scope} has invalid `model_reasoning_effort` `{effort}`"
        ));
    }
    Ok(())
}

pub fn is_valid_reasoning_effort(effort: &str) -> bool {
    matches!(
        effort,
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh"
    )
}

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn resolve_config_path(flag: Option<PathBuf>) -> PathBuf {
    let home = home_dir();
    resolve_config_path_from(
        flag,
        env::var("CODEX_THREADS_CONFIG").ok().as_deref(),
        &home,
    )
}

pub fn resolve_config_path_from(
    flag: Option<PathBuf>,
    env_value: Option<&str>,
    home: &Path,
) -> PathBuf {
    if let Some(path) = flag {
        return path;
    }
    if let Some(path) = env_value {
        return PathBuf::from(path);
    }
    home.join(".config/codex-threads/config.toml")
}

pub fn resolve_target(
    config: &AppConfig,
    connect: Option<&str>,
    server_flag: Option<&str>,
) -> Result<Target> {
    resolve_target_from(
        config,
        connect,
        server_flag,
        env::var("CODEX_THREADS_SERVER").ok().as_deref(),
    )
}

pub fn resolve_target_from(
    config: &AppConfig,
    connect: Option<&str>,
    server_flag: Option<&str>,
    server_env: Option<&str>,
) -> Result<Target> {
    if let Some(endpoint) = connect {
        let path = endpoint
            .strip_prefix("unix://")
            .ok_or_else(|| anyhow!("--connect currently supports only unix:// endpoints"))?;
        if server_flag.is_some() || server_env.is_some() {
            return Err(anyhow!(
                "--connect is mutually exclusive with --server and CODEX_THREADS_SERVER"
            ));
        }
        return Ok(Target {
            server: endpoint.to_string(),
            path: PathBuf::from(path),
            model: None,
            model_reasoning_effort: None,
        });
    }

    if let Some(alias) = server_flag.or(server_env) {
        let server = config
            .servers
            .get(alias)
            .ok_or_else(|| anyhow!("unknown server alias `{alias}`"))?;
        return Ok(Target::configured(alias, server, config));
    }

    if config.servers.len() == 1 {
        let (alias, server) = config.servers.iter().next().expect("len checked");
        return Ok(Target::configured(alias, server, config));
    }

    if config.servers.is_empty() {
        return Err(anyhow!("no servers configured"));
    }

    Err(anyhow!(
        "multiple servers configured; pass --server ALIAS or set CODEX_THREADS_SERVER"
    ))
}
