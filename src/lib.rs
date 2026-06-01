mod app;
mod cli;
mod config;
mod rpc;

pub async fn run() -> i32 {
    app::run_cli(std::env::args_os()).await
}

#[cfg(test)]
mod tests {
    use super::config::{AppConfig, ServerConfig, resolve_config_path_from, resolve_target_from};
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
            },
        );
        let config = AppConfig { servers };
        let target =
            resolve_target_from(&config, Some("unix:///tmp/direct.sock"), None, None).unwrap();
        assert_eq!(target.server, "unix:///tmp/direct.sock");
        assert_eq!(target.path, PathBuf::from("/tmp/direct.sock"));

        let target = resolve_target_from(&config, None, None, None).unwrap();
        assert_eq!(target.server, "main");

        let target = resolve_target_from(&config, None, None, Some("main")).unwrap();
        assert_eq!(target.server, "main");
    }
}
