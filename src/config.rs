use std::collections::BTreeMap;
use std::env;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use url::Url;

pub const REASONING_EFFORTS: [&str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_reasoning_effort: Option<String>,
    #[serde(default)]
    pub servers: BTreeMap<String, ServerConfig>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(rename = "type")]
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub auth_token_env: Option<String>,
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_reasoning_effort: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Target {
    pub server: String,
    pub endpoint: Endpoint,
    pub model: Option<String>,
    pub model_reasoning_effort: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Endpoint {
    Unix {
        path: PathBuf,
    },
    WebSocket {
        url: String,
        auth_token: Option<String>,
    },
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Endpoint::Unix { path } => formatter.debug_struct("Unix").field("path", path).finish(),
            Endpoint::WebSocket { url, auth_token } => formatter
                .debug_struct("WebSocket")
                .field("url", url)
                .field("auth_token", &auth_token.as_ref().map(|_| "<redacted>"))
                .finish(),
        }
    }
}

impl Target {
    pub fn configured(alias: &str, server: &ServerConfig, config: &AppConfig) -> Result<Self> {
        Ok(Self {
            server: alias.to_string(),
            endpoint: server.resolve_endpoint(alias)?,
            model: server.model.clone().or_else(|| config.model.clone()),
            model_reasoning_effort: server
                .model_reasoning_effort
                .clone()
                .or_else(|| config.model_reasoning_effort.clone()),
        })
    }

    pub fn annotation_namespace(&self) -> String {
        self.endpoint.display()
    }
}

impl Endpoint {
    pub fn display(&self) -> String {
        match self {
            Endpoint::Unix { path } => format!("unix://{}", path.display()),
            Endpoint::WebSocket { url, .. } => url.clone(),
        }
    }
}

impl ServerConfig {
    pub fn endpoint_display(&self, alias: &str) -> Result<String> {
        let endpoint = if let Some(endpoint) = self.endpoint.as_deref() {
            endpoint
        } else {
            let path = self
                .path
                .as_ref()
                .ok_or_else(|| anyhow!("server `{alias}` is missing `endpoint`"))?;
            return Ok(format!("unix://{}", path.display()));
        };
        endpoint_display(&format!("server `{alias}`"), endpoint)
    }

    pub fn is_legacy(&self) -> bool {
        self.endpoint.is_none() && self.kind.as_deref() == Some("uds") && self.path.is_some()
    }

    fn resolve_endpoint(&self, alias: &str) -> Result<Endpoint> {
        let endpoint = if let Some(endpoint) = self.endpoint.as_deref() {
            endpoint
        } else {
            let path = self
                .path
                .as_ref()
                .ok_or_else(|| anyhow!("server `{alias}` is missing `endpoint`"))?;
            return resolve_endpoint(
                alias,
                &format!("unix://{}", path.display()),
                self.auth_token_env.as_deref(),
                self.auth_token.as_deref(),
            );
        };
        resolve_endpoint(
            alias,
            endpoint,
            self.auth_token_env.as_deref(),
            self.auth_token.as_deref(),
        )
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
        validate_server_shape(alias, server)?;
        validate_server_endpoint(alias, server)?;
        validate_defaults(
            &format!("server `{alias}`"),
            server.model.as_deref(),
            server.model_reasoning_effort.as_deref(),
        )?;
    }
    Ok(())
}

fn validate_server_shape(alias: &str, server: &ServerConfig) -> Result<()> {
    if server.endpoint.is_some() && (server.kind.is_some() || server.path.is_some()) {
        return Err(anyhow!(
            "server `{alias}` cannot combine `endpoint` with deprecated `type`/`path`"
        ));
    }
    if server.endpoint.is_none() {
        match (server.kind.as_deref(), server.path.as_ref()) {
            (Some("uds"), Some(path)) if !path.as_os_str().is_empty() => {}
            (Some("uds"), _) => return Err(anyhow!("server `{alias}` is missing `path`")),
            (Some(kind), _) => {
                return Err(anyhow!(
                    "server `{alias}` has unsupported type `{kind}`; use `endpoint = \"unix:///...\"`, `endpoint = \"ws://host:port\"`, or `endpoint = \"wss://host:port\"`"
                ));
            }
            (None, Some(_)) => {
                return Err(anyhow!(
                    "server `{alias}` has `path` without `type = \"uds\"`; use `endpoint = \"unix:///path/to.sock\"`"
                ));
            }
            (None, None) => return Err(anyhow!("server `{alias}` is missing `endpoint`")),
        }
    } else if server
        .endpoint
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(anyhow!("server `{alias}` has empty `endpoint`"));
    }
    if server.auth_token.is_some() && server.auth_token_env.is_some() {
        return Err(anyhow!(
            "server `{alias}` cannot set both `auth_token` and `auth_token_env`"
        ));
    }
    Ok(())
}

fn validate_server_endpoint(alias: &str, server: &ServerConfig) -> Result<()> {
    let endpoint = if let Some(endpoint) = server.endpoint.as_deref() {
        endpoint.to_string()
    } else {
        let path = server
            .path
            .as_ref()
            .ok_or_else(|| anyhow!("server `{alias}` is missing `endpoint`"))?;
        format!("unix://{}", path.display())
    };
    validate_endpoint_syntax(&format!("server `{alias}`"), &endpoint)?;
    if server.auth_token.is_some() || server.auth_token_env.is_some() {
        let url = Url::parse(&endpoint).ok();
        match url.as_ref().map(Url::scheme) {
            Some("ws" | "wss") => {
                let url = url.expect("checked");
                if !websocket_url_supports_auth_token(&url) {
                    return Err(anyhow!(
                        "server `{alias}` auth token requires a wss:// or loopback ws:// endpoint"
                    ));
                }
            }
            _ => {
                return Err(anyhow!(
                    "server `{alias}` auth token requires a websocket endpoint"
                ));
            }
        }
    }
    if let Some(token) = server.auth_token.as_deref()
        && token.trim().is_empty()
    {
        return Err(anyhow!("server `{alias}` has empty `auth_token`"));
    }
    if let Some(env_name) = server.auth_token_env.as_deref()
        && env_name.trim().is_empty()
    {
        return Err(anyhow!("server `{alias}` has empty `auth_token_env`"));
    }
    Ok(())
}

fn validate_endpoint_syntax(scope: &str, endpoint: &str) -> Result<()> {
    let Some((scheme, _rest)) = endpoint.split_once("://") else {
        return Err(anyhow!(
            "{scope} endpoint `{endpoint}` must use unix://, ws://, or wss://"
        ));
    };
    match scheme {
        "unix" => {
            let path = endpoint
                .strip_prefix("unix://")
                .expect("scheme checked")
                .trim();
            if path.is_empty() {
                return Err(anyhow!("{scope} endpoint `unix://` is missing a path"));
            }
            Ok(())
        }
        "ws" | "wss" => {
            let url = Url::parse(endpoint)
                .with_context(|| format!("{scope} has invalid websocket endpoint `{endpoint}`"))?;
            if url.host().is_none() {
                return Err(anyhow!("{scope} websocket endpoint must include a host"));
            }
            if url.port().is_none() {
                return Err(anyhow!(
                    "{scope} websocket endpoint must include an explicit port"
                ));
            }
            if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
                return Err(anyhow!(
                    "{scope} websocket endpoint must not include a path, query, or fragment"
                ));
            }
            Ok(())
        }
        _ => Err(anyhow!(
            "{scope} endpoint `{endpoint}` must use unix://, ws://, or wss://"
        )),
    }
}

fn endpoint_display(scope: &str, endpoint: &str) -> Result<String> {
    let Some((scheme, _rest)) = endpoint.split_once("://") else {
        return Err(anyhow!(
            "{scope} endpoint `{endpoint}` must use unix://, ws://, or wss://"
        ));
    };
    match scheme {
        "unix" => {
            let path = endpoint
                .strip_prefix("unix://")
                .expect("scheme checked")
                .trim();
            if path.is_empty() {
                return Err(anyhow!("{scope} endpoint `unix://` is missing a path"));
            }
            Ok(format!("unix://{path}"))
        }
        "ws" | "wss" => {
            validate_endpoint_syntax(scope, endpoint)?;
            let url = Url::parse(endpoint)
                .with_context(|| format!("{scope} has invalid websocket endpoint `{endpoint}`"))?;
            Ok(url.to_string())
        }
        _ => Err(anyhow!(
            "{scope} endpoint `{endpoint}` must use unix://, ws://, or wss://"
        )),
    }
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
    REASONING_EFFORTS.contains(&effort)
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

pub fn resolve_target(config: &AppConfig, server_flag: Option<&str>) -> Result<Target> {
    resolve_target_from(
        config,
        server_flag,
        env::var("CODEX_THREADS_SERVER").ok().as_deref(),
    )
}

#[cfg(feature = "tui")]
pub fn resolve_tui_targets(config: &AppConfig, server_flag: Option<&str>) -> Result<Vec<Target>> {
    if server_flag.is_some() || env::var("CODEX_THREADS_SERVER").is_ok() {
        return Ok(vec![resolve_target(config, server_flag)?]);
    }
    if config.servers.is_empty() {
        return Err(anyhow!("no servers configured"));
    }
    config
        .servers
        .iter()
        .map(|(alias, server)| Target::configured(alias, server, config))
        .collect()
}

pub fn resolve_target_from(
    config: &AppConfig,
    server_flag: Option<&str>,
    server_env: Option<&str>,
) -> Result<Target> {
    if let Some(alias) = server_flag.or(server_env) {
        let server = config
            .servers
            .get(alias)
            .ok_or_else(|| anyhow!("unknown server alias `{alias}`"))?;
        return Target::configured(alias, server, config);
    }

    if config.servers.len() == 1 {
        let (alias, server) = config.servers.iter().next().expect("len checked");
        return Target::configured(alias, server, config);
    }

    if config.servers.is_empty() {
        return Err(anyhow!("no servers configured"));
    }

    Err(anyhow!(
        "multiple servers configured; pass --server ALIAS or set CODEX_THREADS_SERVER"
    ))
}

pub fn resolve_direct_target(
    endpoint: &str,
    auth_token_env: Option<&str>,
    auth_token: Option<&str>,
) -> Result<Target> {
    Ok(Target {
        server: endpoint.to_string(),
        endpoint: resolve_endpoint("direct connection", endpoint, auth_token_env, auth_token)?,
        model: None,
        model_reasoning_effort: None,
    })
}

pub fn legacy_server_warnings(config: &AppConfig) -> Vec<String> {
    config
        .servers
        .iter()
        .filter_map(|(alias, server)| {
            if server.is_legacy() {
                let path = server.path.as_ref()?;
                Some(format!(
                    "server `{alias}` uses deprecated `type = \"uds\"` + `path`; replace with `endpoint = \"unix://{}\"`",
                    path.display()
                ))
            } else {
                None
            }
        })
        .collect()
}

fn resolve_endpoint(
    scope: &str,
    endpoint: &str,
    auth_token_env: Option<&str>,
    auth_token: Option<&str>,
) -> Result<Endpoint> {
    if auth_token.is_some() && auth_token_env.is_some() {
        return Err(anyhow!(
            "{scope} cannot set both `auth_token` and `auth_token_env`"
        ));
    }
    let Some((scheme, _rest)) = endpoint.split_once("://") else {
        return Err(anyhow!(
            "{scope} endpoint `{endpoint}` must use unix://, ws://, or wss://"
        ));
    };
    match scheme {
        "unix" => {
            if auth_token.is_some() || auth_token_env.is_some() {
                return Err(anyhow!("{scope} auth token requires a websocket endpoint"));
            }
            let path = endpoint
                .strip_prefix("unix://")
                .expect("scheme checked")
                .trim();
            if path.is_empty() {
                return Err(anyhow!("{scope} endpoint `unix://` is missing a path"));
            }
            Ok(Endpoint::Unix {
                path: PathBuf::from(path),
            })
        }
        "ws" | "wss" => resolve_websocket_endpoint(scope, endpoint, auth_token_env, auth_token),
        _ => Err(anyhow!(
            "{scope} endpoint `{endpoint}` must use unix://, ws://, or wss://"
        )),
    }
}

fn resolve_websocket_endpoint(
    scope: &str,
    endpoint: &str,
    auth_token_env: Option<&str>,
    auth_token: Option<&str>,
) -> Result<Endpoint> {
    let url = Url::parse(endpoint)
        .with_context(|| format!("{scope} has invalid websocket endpoint `{endpoint}`"))?;
    if url.host().is_none() {
        return Err(anyhow!("{scope} websocket endpoint must include a host"));
    }
    if url.port().is_none() {
        return Err(anyhow!(
            "{scope} websocket endpoint must include an explicit port"
        ));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(anyhow!(
            "{scope} websocket endpoint must not include a path, query, or fragment"
        ));
    }
    let auth_token = resolve_auth_token(scope, &url, auth_token_env, auth_token)?;
    Ok(Endpoint::WebSocket {
        url: url.to_string(),
        auth_token,
    })
}

fn resolve_auth_token(
    scope: &str,
    url: &Url,
    auth_token_env: Option<&str>,
    auth_token: Option<&str>,
) -> Result<Option<String>> {
    let token = match (auth_token_env, auth_token) {
        (Some(env_name), None) => {
            let env_name = env_name.trim();
            if env_name.is_empty() {
                return Err(anyhow!("{scope} has empty `auth_token_env`"));
            }
            let token = env::var(env_name)
                .with_context(|| format!("{scope} `auth_token_env` is not set or is invalid"))?;
            Some(non_empty_token(scope, &token, "`auth_token_env` value")?)
        }
        (None, Some(token)) => Some(non_empty_token(scope, token, "`auth_token`")?),
        (None, None) => None,
        (Some(_), Some(_)) => unreachable!("checked by caller"),
    };
    if token.is_some() && !websocket_url_supports_auth_token(url) {
        return Err(anyhow!(
            "{scope} auth token requires a wss:// or loopback ws:// endpoint"
        ));
    }
    Ok(token)
}

fn non_empty_token(scope: &str, value: &str, field: &str) -> Result<String> {
    let token = value.trim();
    if token.is_empty() {
        return Err(anyhow!("{scope} has empty {field}"));
    }
    Ok(token.to_string())
}

fn websocket_url_supports_auth_token(url: &Url) -> bool {
    match (url.scheme(), url.host()) {
        ("wss", Some(_)) => true,
        ("ws", Some(url::Host::Domain(domain))) => domain.eq_ignore_ascii_case("localhost"),
        ("ws", Some(url::Host::Ipv4(addr))) => IpAddr::V4(addr).is_loopback(),
        ("ws", Some(url::Host::Ipv6(addr))) => IpAddr::V6(addr).is_loopback(),
        _ => false,
    }
}
