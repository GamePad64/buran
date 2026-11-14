//! Unit tests for `McpManager` internals.

use std::collections::BTreeMap;
use std::time::Duration;

use super::*;

/// The original configs must survive in the manager so a later
/// lazy-restart can respawn from them (hardening v1.5.1).
#[tokio::test]
async fn start_stores_original_configs_for_restart() {
    let configs = vec![
        McpServerConfig {
            name: "a".into(),
            command: "cat".into(),
            args: vec![],
            env: BTreeMap::new(),
        },
        McpServerConfig {
            name: "b".into(),
            command: "cat".into(),
            args: vec!["-A".into()],
            env: BTreeMap::new(),
        },
    ];
    let manager = McpManager::start(&configs, Duration::from_secs(1))
        .await
        .unwrap();
    let stored = manager.stored_configs();
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0].name, "a");
    assert_eq!(stored[1].args, vec!["-A".to_string()]);
    manager.shutdown().await;
}
