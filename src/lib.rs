mod app;
mod cli;
mod config;
mod rpc;

pub async fn run() -> i32 {
    app::run_cli(std::env::args_os()).await
}

#[cfg(test)]
mod tests {
    use super::config::{
        AppConfig, ServerConfig, resolve_config_path_from, resolve_target_from, validate_config,
    };
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[test]
    fn config_path_precedence_prefers_flag_then_env_then_default() {
        let home = PathBuf::from("/home/tester");
        assert_eq!(
            resolve_config_path_from(
                Some(PathBuf::from("/tmp/a.toml")),
                Some("/tmp/b.toml"),
                &home
            ),
            PathBuf::from("/tmp/a.toml")
        );
        assert_eq!(
            resolve_config_path_from(None, Some("/tmp/b.toml"), &home),
            PathBuf::from("/tmp/b.toml")
        );
        assert_eq!(
            resolve_config_path_from(None, None, &home),
            PathBuf::from("/home/tester/.config/codex-threads/config.toml")
        );
    }

    #[test]
    fn target_precedence_handles_connect_server_env_and_singleton() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "main".to_string(),
            ServerConfig {
                kind: "uds".to_string(),
                path: PathBuf::from("/tmp/main.sock"),
                model: None,
                model_reasoning_effort: None,
            },
        );
        let config = AppConfig {
            model: None,
            model_reasoning_effort: None,
            servers,
        };
        let target =
            resolve_target_from(&config, Some("unix:///tmp/direct.sock"), None, None).unwrap();
        assert_eq!(target.server, "unix:///tmp/direct.sock");
        assert_eq!(target.path, PathBuf::from("/tmp/direct.sock"));

        let target = resolve_target_from(&config, None, None, None).unwrap();
        assert_eq!(target.server, "main");

        let target = resolve_target_from(&config, None, None, Some("main")).unwrap();
        assert_eq!(target.server, "main");
    }

    #[test]
    fn target_resolution_merges_global_and_server_model_defaults() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "main".to_string(),
            ServerConfig {
                kind: "uds".to_string(),
                path: PathBuf::from("/tmp/main.sock"),
                model: None,
                model_reasoning_effort: Some("high".to_string()),
            },
        );
        servers.insert(
            "work".to_string(),
            ServerConfig {
                kind: "uds".to_string(),
                path: PathBuf::from("/tmp/work.sock"),
                model: Some("gpt-5.5".to_string()),
                model_reasoning_effort: None,
            },
        );
        let config = AppConfig {
            model: Some("gpt-global".to_string()),
            model_reasoning_effort: Some("low".to_string()),
            servers,
        };

        let main = resolve_target_from(&config, None, Some("main"), None).unwrap();
        assert_eq!(main.model.as_deref(), Some("gpt-global"));
        assert_eq!(main.model_reasoning_effort.as_deref(), Some("high"));

        let work = resolve_target_from(&config, None, Some("work"), None).unwrap();
        assert_eq!(work.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(work.model_reasoning_effort.as_deref(), Some("low"));

        let direct =
            resolve_target_from(&config, Some("unix:///tmp/direct.sock"), None, None).unwrap();
        assert_eq!(direct.model, None);
        assert_eq!(direct.model_reasoning_effort, None);
    }

    #[test]
    fn config_validation_rejects_invalid_model_defaults() {
        let config = AppConfig {
            model: Some("   ".to_string()),
            model_reasoning_effort: None,
            servers: BTreeMap::new(),
        };
        assert!(validate_config(&config).is_err());

        let config = AppConfig {
            model: None,
            model_reasoning_effort: Some("giant".to_string()),
            servers: BTreeMap::new(),
        };
        assert!(validate_config(&config).is_err());
    }
}
