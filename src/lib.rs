mod annotations;
mod app;
mod cli;
mod completion;
mod config;
mod debuglog;
mod errors;
mod rpc;
mod session;
mod time_filter;
#[cfg(feature = "tui")]
mod tui;
mod turns;

pub async fn run() -> i32 {
    app::run_cli(std::env::args_os()).await
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "tui")]
    use super::config::resolve_tui_targets;
    use super::config::{
        AppConfig, Endpoint, ServerConfig, resolve_config_path_from, resolve_direct_target,
        resolve_target_from, validate_config,
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

    fn server(endpoint: &str) -> ServerConfig {
        ServerConfig {
            endpoint: Some(endpoint.to_string()),
            kind: None,
            path: None,
            auth_token_env: None,
            auth_token: None,
            model: None,
            model_reasoning_effort: None,
        }
    }

    fn legacy_server(path: &str) -> ServerConfig {
        ServerConfig {
            endpoint: None,
            kind: Some("uds".to_string()),
            path: Some(PathBuf::from(path)),
            auth_token_env: None,
            auth_token: None,
            model: None,
            model_reasoning_effort: None,
        }
    }

    #[test]
    fn target_precedence_handles_connect_server_env_and_singleton() {
        let mut servers = BTreeMap::new();
        servers.insert("main".to_string(), server("unix:///tmp/main.sock"));
        let config = AppConfig {
            model: None,
            model_reasoning_effort: None,
            servers,
        };
        let target = resolve_direct_target("unix:///tmp/direct.sock", None, None).unwrap();
        assert_eq!(target.server, "unix:///tmp/direct.sock");
        assert_eq!(
            target.endpoint,
            Endpoint::Unix {
                path: PathBuf::from("/tmp/direct.sock")
            }
        );

        let target = resolve_target_from(&config, None, None).unwrap();
        assert_eq!(target.server, "main");

        let target = resolve_target_from(&config, None, Some("main")).unwrap();
        assert_eq!(target.server, "main");
    }

    #[test]
    fn target_resolution_merges_global_and_server_model_defaults() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "main".to_string(),
            ServerConfig {
                endpoint: Some("unix:///tmp/main.sock".to_string()),
                kind: None,
                path: None,
                auth_token_env: None,
                auth_token: None,
                model: None,
                model_reasoning_effort: Some("high".to_string()),
            },
        );
        servers.insert(
            "work".to_string(),
            ServerConfig {
                endpoint: Some("unix:///tmp/work.sock".to_string()),
                kind: None,
                path: None,
                auth_token_env: None,
                auth_token: None,
                model: Some("gpt-5.5".to_string()),
                model_reasoning_effort: None,
            },
        );
        let config = AppConfig {
            model: Some("gpt-global".to_string()),
            model_reasoning_effort: Some("low".to_string()),
            servers,
        };

        let main = resolve_target_from(&config, Some("main"), None).unwrap();
        assert_eq!(main.model.as_deref(), Some("gpt-global"));
        assert_eq!(main.model_reasoning_effort.as_deref(), Some("high"));

        let work = resolve_target_from(&config, Some("work"), None).unwrap();
        assert_eq!(work.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(work.model_reasoning_effort.as_deref(), Some("low"));

        let direct = resolve_direct_target("unix:///tmp/direct.sock", None, None).unwrap();
        assert_eq!(direct.model, None);
        assert_eq!(direct.model_reasoning_effort, None);
    }

    #[test]
    fn singleton_target_resolution_uses_model_defaults() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "main".to_string(),
            ServerConfig {
                endpoint: Some("unix:///tmp/main.sock".to_string()),
                kind: None,
                path: None,
                auth_token_env: None,
                auth_token: None,
                model: Some("gpt-5.5".to_string()),
                model_reasoning_effort: None,
            },
        );
        let config = AppConfig {
            model: Some("gpt-global".to_string()),
            model_reasoning_effort: Some("high".to_string()),
            servers,
        };

        let target = resolve_target_from(&config, None, None).unwrap();
        assert_eq!(target.server, "main");
        assert_eq!(target.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(target.model_reasoning_effort.as_deref(), Some("high"));
    }

    #[cfg(feature = "tui")]
    #[test]
    fn tui_target_resolution_defaults_to_all_servers_and_flag_narrows() {
        let mut servers = BTreeMap::new();
        servers.insert("main".to_string(), server("unix:///tmp/main.sock"));
        servers.insert("work".to_string(), server("unix:///tmp/work.sock"));
        let config = AppConfig {
            model: None,
            model_reasoning_effort: None,
            servers,
        };

        let targets = resolve_tui_targets(&config, None).unwrap();
        assert_eq!(
            targets
                .iter()
                .map(|target| target.server.as_str())
                .collect::<Vec<_>>(),
            vec!["main", "work"]
        );

        let targets = resolve_tui_targets(&config, Some("work")).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].server, "work");
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

    #[test]
    fn config_validation_accepts_legacy_uds_and_rejects_mixed_endpoint_shape() {
        let mut servers = BTreeMap::new();
        servers.insert("main".to_string(), legacy_server("/tmp/main.sock"));
        assert!(
            validate_config(&AppConfig {
                model: None,
                model_reasoning_effort: None,
                servers,
            })
            .is_ok()
        );

        let mut servers = BTreeMap::new();
        let mut mixed = server("unix:///tmp/main.sock");
        mixed.kind = Some("uds".to_string());
        servers.insert("main".to_string(), mixed);
        let err = validate_config(&AppConfig {
            model: None,
            model_reasoning_effort: None,
            servers,
        })
        .expect_err("mixed endpoint and legacy fields should fail");
        assert!(err.to_string().contains("cannot combine"));
    }

    #[test]
    fn config_validation_rejects_path_without_legacy_type() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "main".to_string(),
            ServerConfig {
                endpoint: None,
                kind: None,
                path: Some(PathBuf::from("/tmp/main.sock")),
                auth_token_env: None,
                auth_token: None,
                model: None,
                model_reasoning_effort: None,
            },
        );
        let err = validate_config(&AppConfig {
            model: None,
            model_reasoning_effort: None,
            servers,
        })
        .expect_err("path alone should fail");
        assert!(err.to_string().contains("path"));
    }

    #[test]
    fn config_validation_rejects_websocket_tokens_on_insecure_non_loopback_ws() {
        let mut servers = BTreeMap::new();
        let mut server = server("ws://example.com:1234");
        server.auth_token = Some("secret".to_string());
        servers.insert("main".to_string(), server);
        let err = validate_config(&AppConfig {
            model: None,
            model_reasoning_effort: None,
            servers,
        })
        .expect_err("non-loopback ws auth token should fail");
        assert!(err.to_string().contains("wss:// or loopback ws://"));
    }

    #[test]
    fn config_parse_rejects_unknown_fields() {
        let err = toml::from_str::<AppConfig>(
            r#"[servers.main]
endpoint = "ws://127.0.0.1:1234"
auth_token_en = "CODEX_APP_SERVER_TOKEN"
"#,
        )
        .expect_err("misspelled auth field should fail");
        assert!(err.to_string().contains("unknown field"));
    }
}
