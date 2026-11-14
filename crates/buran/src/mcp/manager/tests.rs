//! Unit tests for `McpManager` internals.

use std::collections::BTreeMap;
use std::time::Duration;

use super::*;

/// The original configs must survive in the manager so lazy restart-on-call
/// can respawn from them (hardening v1.5.1) — exercised end-to-end by
/// `dead_server_is_respawned_and_call_succeeds` in `mcp_mock_server.rs`;
/// this pins that the configs themselves are stored verbatim, in order.
#[tokio::test]
async fn start_stores_original_configs() {
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
    assert_eq!(manager.configs, configs);
    manager.shutdown().await;
}

/// Dead-transport detection: the classification of rmcp errors marks
/// transport-level failures as restartable while MCP-level errors are not.
#[test]
fn transport_errors_are_restartable_and_mcp_errors_are_not() {
    use rmcp::model::ErrorData;
    let mcp = ErrorData::invalid_request("nope", None);
    assert!(!is_restartable(&rmcp::ServiceError::McpError(mcp)));
    assert!(is_restartable(&rmcp::ServiceError::TransportClosed));
    // TransportSend wraps a DynamicTransportError; build one from parts.
    let dte = rmcp::transport::DynamicTransportError::from_parts(
        "test",
        std::any::TypeId::of::<()>(),
        "pipe".into(),
    );
    assert!(is_restartable(&rmcp::ServiceError::TransportSend(dte)));
    assert!(!is_restartable(&rmcp::ServiceError::UnexpectedResponse));
    assert!(!is_restartable(&rmcp::ServiceError::Cancelled {
        reason: None
    }));
}
