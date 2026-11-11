//! The Ask-policy wire round-trip: `session/request_permission` to the
//! ACP client, decision mapped back from the selected option's `kind`.
//!
//! Runs only inside the spawned prompt task (never on the dispatch loop —
//! the request must block the turn, not message processing).

use std::time::Duration;

use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::{
    Client,
    v1::schema::{
        PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionOutcome,
        RequestPermissionRequest, SessionId, ToolCallId, ToolCallStatus, ToolCallUpdate,
        ToolCallUpdateFields, ToolKind,
    },
};

use crate::llm::loop_::PermissionDecision;

/// The options buran offers, and the kind each id resolves to. The
/// selected `option_id` never echoes a kind, so the outcome is resolved
/// against this table (yolo example rule: decide by kind, never by
/// position or option name).
const OFFERED: [(&str, PermissionOptionKind); 4] = [
    ("allow", PermissionOptionKind::AllowOnce),
    ("allow_always", PermissionOptionKind::AllowAlways),
    ("reject", PermissionOptionKind::RejectOnce),
    ("reject_always", PermissionOptionKind::RejectAlways),
];

/// Asks the client to approve one tool call. The tool call arrives as
/// `Pending` so editors render a confirmable card; the decision is read
/// from the selected option's `kind`.
///
/// Every failure mode denies: timeout, cancelled outcome, response error.
/// A client answering `AllowAlways`/`RejectAlways` is honored for this
/// call (persistence is v1.5, so "always" never outlives it).
pub async fn request_permission(
    connection: &ConnectionTo<Client>,
    session_id: &SessionId,
    tool_call_id: ToolCallId,
    tool_name: &str,
    tool_title: &str,
    timeout: Duration,
) -> PermissionDecision {
    let options = OFFERED
        .iter()
        .map(|(id, kind)| PermissionOption::new(*id, id_label(id), *kind))
        .collect::<Vec<_>>();
    let request = RequestPermissionRequest::new(
        session_id.clone(),
        ToolCallUpdate::new(
            tool_call_id,
            ToolCallUpdateFields::new()
                .title(tool_title.to_string())
                .name(tool_name.to_string())
                .kind(ToolKind::Other)
                .status(ToolCallStatus::Pending),
        ),
        options,
    );

    let response =
        match tokio::time::timeout(timeout, connection.send_request(request).block_task()).await {
            Ok(Ok(response)) => response,
            // Timeout or transport error: deny rather than guess allow.
            _ => return PermissionDecision::Deny,
        };

    let RequestPermissionOutcome::Selected(selected) = response.outcome else {
        // `Cancelled` — the turn is being cancelled (protocol rule after
        // session/cancel); the loop also cancels independently.
        return PermissionDecision::Deny;
    };

    match kind_of(&selected.option_id) {
        Some(PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways) => {
            PermissionDecision::Allow
        }
        _ => PermissionDecision::Deny,
    }
}

fn kind_of(option_id: &PermissionOptionId) -> Option<PermissionOptionKind> {
    let id = option_id.0.as_ref();
    OFFERED
        .iter()
        .find(|(offered, _)| *offered == id)
        .map(|(_, kind)| *kind)
}

fn id_label(id: &str) -> &str {
    match id {
        "allow" => "Allow",
        "allow_always" => "Always allow",
        "reject" => "Reject",
        "reject_always" => "Always reject",
        _ => id,
    }
}
