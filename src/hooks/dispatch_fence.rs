//! Helper for non-agent dispatch paths to fire `BeforeToolCall` before
//! invoking a tool.
//!
//! The four agent-initiated tool dispatch paths (chat, job, engine v2
//! effect, gate approval) fire `BeforeToolCall` inline because they need
//! per-path behavior on `HookOutcome::Continue { modified }` (sensitive-
//! parameter re-injection, prepare_tool_params re-normalization, etc.).
//!
//! The non-agent dispatch paths (channel/CLI gateway via `ToolDispatcher`,
//! scheduler subtasks, lightweight routine tool calls) historically did
//! not fire the hook at all — discovered as a coverage gap by CLIC01's
//! GAR-V1 audit. This helper closes that gap with consistent behavior:
//!
//! - Build a `HookEvent::ToolCall` with the standard payload.
//! - Run the hook chain.
//! - Translate `HookOutcome::Reject` into [`DispatchFenceError::Rejected`]
//!   so the caller can map it to whatever local error type it returns
//!   (`ToolError::ExecutionFailed`, etc.).
//! - Treat `HookOutcome::Continue { modified }` as allow; modifications
//!   are silently ignored on these paths (no caller currently uses
//!   modify-on-bypass; if needed later, add a richer return shape).
//! - Hook framework errors (timeout, executor failure with FailOpen
//!   default) are absorbed by `HookRegistry::run`; we only see them
//!   when a hook is explicitly `FailClosed`, in which case we propagate
//!   as `DispatchFenceError::Blocked`.

use crate::hooks::{HookError, HookEvent, HookOutcome, HookRegistry};
use crate::tools::Tool;
use crate::tools::redact_params;

/// Failure modes returned by [`fire_before_tool_call`].
#[derive(Debug, thiserror::Error)]
pub enum DispatchFenceError {
    /// A hook explicitly rejected this tool call.
    #[error("Tool call rejected by hook: {reason}")]
    Rejected { reason: String },

    /// Hook framework error with a fail-closed hook in the chain.
    #[error("Tool call blocked by hook policy: {0}")]
    Blocked(String),
}

/// Fire `BeforeToolCall` before a non-agent dispatch path executes a tool.
///
/// Returns `Ok(())` to proceed (potentially with hook-modified params, which
/// are *not* applied — the caller continues with the original params). Returns
/// `Err` to abort.
///
/// Sensitive parameters are redacted via the tool's `sensitive_params()` list
/// before the hook event is constructed, so hooks (and the audit chain) never
/// see secret values.
pub async fn fire_before_tool_call(
    hooks: &HookRegistry,
    tool: &dyn Tool,
    raw_params: &serde_json::Value,
    user_id: &str,
    context: String,
    thread_id: Option<String>,
    run_id: Option<String>,
) -> Result<(), DispatchFenceError> {
    let hook_params = redact_params(raw_params, tool.sensitive_params());
    let event = HookEvent::ToolCall {
        tool_name: tool.name().to_string(),
        parameters: hook_params,
        user_id: user_id.to_string(),
        context,
        thread_id,
        run_id,
    };

    match hooks.run(&event).await {
        Ok(HookOutcome::Continue { .. }) => Ok(()),
        Ok(HookOutcome::Reject { reason }) => Err(DispatchFenceError::Rejected { reason }),
        Err(HookError::Rejected { reason }) => Err(DispatchFenceError::Rejected { reason }),
        Err(err) => Err(DispatchFenceError::Blocked(err.to_string())),
    }
}
