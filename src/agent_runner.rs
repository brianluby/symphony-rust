//! Agent runner — spawns coding-agent subprocess and streams JSON-RPC events over stdio.
//! Maps to SPEC.md §12.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;

use crate::config::{
    DEFAULT_CODEX_APPROVAL_POLICY, DEFAULT_CODEX_THREAD_SANDBOX, DEFAULT_CODEX_TURN_SANDBOX_POLICY,
};
use crate::types::{CancelHandle, Issue, LiveSession, RunningEntry, make_session_id};

/// Outcome of an agent run attempt.
#[derive(Debug)]
pub enum AgentOutcome {
    /// Worker finished normally (agent completed).
    Normal {
        entry: RunningEntry,
        total_turns: u32,
    },
    /// Worker failed (agent error, timeout, crash).
    Error { error: String, entry: RunningEntry },
    /// Worker was cancelled (reconciliation).
    Cancelled { reason: String, entry: RunningEntry },
}

/// Configuration for the agent runner.
#[derive(Clone)]
pub struct AgentRunnerConfig {
    pub command: String,
    pub workspace_path: PathBuf,
    pub approval_policy: String,
    pub thread_sandbox: String,
    pub turn_sandbox_policy: String,
    #[allow(dead_code)]
    pub turn_timeout_ms: u64,
    #[allow(dead_code)]
    pub read_timeout_ms: u64,
    pub stall_timeout_ms: u64,
    pub max_turns: u32,
    pub stop_after_first_turn: bool,
    pub on_session_update: Option<Arc<dyn Fn(LiveSession) + Send + Sync>>,
    pub cancelled: CancelHandle,
}

/// Run a full agent session for an issue in a workspace.
///
/// This function:
/// 1. Launches the coding-agent subprocess
/// 2. Streams JSON-RPC events over stdio
/// 3. Runs multiple turns, re-checking issue state between turns
/// 4. Returns the final outcome
pub async fn run_agent_attempt(
    config: AgentRunnerConfig,
    issue: Issue,
    attempt: Option<u32>,
    prompt: String,
    continuation_template: impl Fn(u32, u32) -> String,
) -> AgentOutcome {
    let mut entry = RunningEntry {
        issue_id: issue.id.clone(),
        identifier: issue.identifier.clone(),
        issue: issue.clone(),
        session: LiveSession::default(),
        retry_attempt: attempt,
        started_at: chrono::Utc::now(),
        cancelled: config.cancelled.clone(),
    };

    // Launch agent process
    let mut child = match launch_agent(&config.command, &config.workspace_path).await {
        Ok(c) => c,
        Err(e) => {
            return AgentOutcome::Error {
                error: format!("failed to launch agent: {e}"),
                entry,
            };
        }
    };

    // Set up stdio reader
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            return AgentOutcome::Error {
                error: "agent stdout was not piped".into(),
                entry,
            };
        }
    };
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    // Set up stderr reader for diagnostics
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            return AgentOutcome::Error {
                error: "agent stderr was not piped".into(),
                entry,
            };
        }
    };
    let stderr_reader = BufReader::new(stderr);
    let mut stderr_lines = stderr_reader.lines();

    // Spawn stderr logging task
    let stderr_task = tokio::spawn(async move {
        let mut output = Vec::new();
        while let Ok(Some(line)) = stderr_lines.next_line().await {
            tracing::debug!(stderr = %line, "agent stderr");
            output.push(line);
        }
        output
    });

    let mut turn_number: u32 = 1;
    let max_turns = config.max_turns;
    let workspace_cwd = std::fs::canonicalize(&config.workspace_path)
        .unwrap_or_else(|_| config.workspace_path.clone());

    // ── Initialize app-server connection ──
    let initialize_msg = initialize_request(1);

    // Take stdin once and keep it alive for the full turn loop
    let mut child_stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            return AgentOutcome::Error {
                error: "agent stdin was not piped".into(),
                entry,
            };
        }
    };

    {
        use tokio::io::AsyncWriteExt;
        let msg_str = serde_json::to_string(&initialize_msg).unwrap();
        if child_stdin
            .write_all(format!("{}\n", msg_str).as_bytes())
            .await
            .is_err()
        {
            let _ = child.kill().await;
            return AgentOutcome::Error {
                error: "failed to write initialize request to agent stdin".into(),
                entry,
            };
        }
    }

    if let Err(e) =
        read_initialize_response(&mut lines, &mut entry, config.on_session_update.as_deref()).await
    {
        let _ = child.kill().await;
        let _ = child.wait().await;
        stderr_task.abort();
        return AgentOutcome::Error {
            error: format!("initialize failed: {e:?}"),
            entry,
        };
    }

    // ── Send initial thread setup ──
    let setup_msg = thread_start_request(2, &config, &workspace_cwd);

    {
        use tokio::io::AsyncWriteExt;
        let msg_str = serde_json::to_string(&setup_msg).unwrap();
        if child_stdin
            .write_all(format!("{}\n", msg_str).as_bytes())
            .await
            .is_err()
        {
            let _ = child.kill().await;
            return AgentOutcome::Error {
                error: "failed to write to agent stdin".into(),
                entry,
            };
        }
    }

    let thread_id = match read_thread_start_response(
        &mut lines,
        &mut entry,
        config.on_session_update.as_deref(),
    )
    .await
    {
        Ok(thread_id) => thread_id,
        Err(e) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            stderr_task.abort();
            return AgentOutcome::Error {
                error: format!("thread start failed: {e:?}"),
                entry,
            };
        }
    };

    entry.session.thread_id = Some(thread_id.clone());

    // ── Turn loop ──
    loop {
        if config.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = child.kill().await;
            let _ = child.wait().await;
            stderr_task.abort();
            return AgentOutcome::Cancelled {
                reason: "cancelled by reconciliation".into(),
                entry,
            };
        }

        let turn_id = format!("pending-{turn_number}");
        entry.session.session_id = Some(make_session_id(&thread_id, &turn_id));
        entry.session.turn_id = Some(turn_id);
        entry.session.turn_count = u64::from(turn_number);

        let turn_prompt = if turn_number == 1 {
            prompt.clone()
        } else {
            continuation_template(turn_number, max_turns)
        };

        let turn_msg = turn_start_request(
            turn_number + 2,
            &thread_id,
            &turn_prompt,
            &config,
            &workspace_cwd,
        );

        {
            use tokio::io::AsyncWriteExt;
            let msg_str = serde_json::to_string(&turn_msg).unwrap();
            if child_stdin
                .write_all(format!("{}\n", msg_str).as_bytes())
                .await
                .is_err()
            {
                let _ = child.kill().await;
                return AgentOutcome::Error {
                    error: "failed to write turn start to agent stdin".into(),
                    entry,
                };
            }
        }

        // Read events from agent with stall detection
        let read_result = if config.stall_timeout_ms > 0 {
            tokio::select! {
                result = timeout(
                    Duration::from_millis(config.stall_timeout_ms),
                    read_agent_events(&mut lines, &mut entry, config.on_session_update.as_deref()),
                ) => result,
                () = wait_for_cancel(config.cancelled.clone()) => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    stderr_task.abort();
                    return AgentOutcome::Cancelled {
                        reason: "cancelled by reconciliation".into(),
                        entry,
                    };
                }
            }
        } else {
            tokio::select! {
                result = read_agent_events(&mut lines, &mut entry, config.on_session_update.as_deref()) => Ok(result),
                () = wait_for_cancel(config.cancelled.clone()) => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    stderr_task.abort();
                    return AgentOutcome::Cancelled {
                        reason: "cancelled by reconciliation".into(),
                        entry,
                    };
                }
            }
        };

        match read_result {
            Ok(Ok(())) => {
                // Turn completed normally — check if we should continue
                tracing::info!(
                    issue_id = %entry.issue_id,
                    issue_identifier = %entry.identifier,
                    turn = turn_number,
                    "turn completed"
                );

                if turn_number >= max_turns {
                    tracing::info!(
                        issue_id = %entry.issue_id,
                        "max turns reached, stopping"
                    );
                    break;
                }

                if config.stop_after_first_turn {
                    tracing::info!(
                        issue_id = %entry.issue_id,
                        "completion workflow configured, stopping after first turn"
                    );
                    break;
                }

                turn_number += 1;
            }
            Ok(Err(EventError::Exit)) => {
                // Agent exited
                break;
            }
            Ok(Err(EventError::TurnFailed(reason))) => {
                return AgentOutcome::Error {
                    error: format!("turn failed: {reason}"),
                    entry,
                };
            }
            Err(_elapsed) => {
                // Stall timeout
                let _ = child.kill().await;
                return AgentOutcome::Error {
                    error: format!(
                        "agent stalled (no events for {}ms)",
                        config.stall_timeout_ms
                    ),
                    entry,
                };
            }
        }
    }

    // Clean shutdown
    let _ = child.kill().await;
    let _ = stderr_task.await;

    AgentOutcome::Normal {
        entry,
        total_turns: turn_number,
    }
}

fn initialize_request(id: u32) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "params": {
            "clientInfo": {
                "name": "symphony",
                "title": "Symphony",
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": {
                "experimentalApi": true
            }
        },
        "id": id
    })
}

fn thread_start_request(id: u32, config: &AgentRunnerConfig, cwd: &Path) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "thread/start",
        "params": {
            "cwd": cwd,
            "approvalPolicy": approval_policy_wire(&config.approval_policy),
            "sandbox": thread_sandbox_wire(&config.thread_sandbox),
        },
        "id": id
    })
}

fn turn_start_request(
    id: u32,
    thread_id: &str,
    prompt: &str,
    config: &AgentRunnerConfig,
    cwd: &Path,
) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "turn/start",
        "params": {
            "threadId": thread_id,
            "input": [
                {
                    "type": "text",
                    "text": prompt,
                    "text_elements": []
                }
            ],
            "approvalPolicy": approval_policy_wire(&config.approval_policy),
            "sandboxPolicy": turn_sandbox_policy_payload(&config.turn_sandbox_policy, cwd),
        },
        "id": id
    })
}

fn approval_policy_wire(policy: &str) -> &'static str {
    match policy {
        "on-request" => "onRequest",
        "on-failure" => "onFailure",
        "unless-trusted" => "unlessTrusted",
        "never" => "never",
        _ => approval_policy_wire(DEFAULT_CODEX_APPROVAL_POLICY),
    }
}

fn thread_sandbox_wire(sandbox: &str) -> &'static str {
    match sandbox {
        "read-only" => "readOnly",
        "workspace-write" => "workspaceWrite",
        "danger-full-access" => "dangerFullAccess",
        _ => thread_sandbox_wire(DEFAULT_CODEX_THREAD_SANDBOX),
    }
}

fn turn_sandbox_policy_payload(sandbox: &str, cwd: &Path) -> serde_json::Value {
    match sandbox {
        "read-only" => serde_json::json!({ "type": "readOnly" }),
        "danger-full-access" => serde_json::json!({ "type": "dangerFullAccess" }),
        "workspace-write" => serde_json::json!({
            "type": "workspaceWrite",
            "writableRoots": [cwd],
            "networkAccess": true,
            "excludeTmpdirEnvVar": false,
            "excludeSlashTmp": false,
        }),
        _ => turn_sandbox_policy_payload(DEFAULT_CODEX_TURN_SANDBOX_POLICY, cwd),
    }
}

// ── Agent process lifecycle ─────────────────────────────────────────────────

async fn launch_agent(command: &str, workspace: &Path) -> Result<Child, std::io::Error> {
    if command.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "codex command must not be empty",
        ));
    }

    let mut cmd = Command::new("bash");
    cmd.arg("-lc")
        .arg(command)
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    cmd.spawn()
}

// ── Event reading ───────────────────────────────────────────────────────────

#[derive(Debug)]
enum EventError {
    Exit,
    TurnFailed(String),
}

async fn read_initialize_response(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    entry: &mut RunningEntry,
    on_session_update: Option<&(dyn Fn(LiveSession) + Send + Sync)>,
) -> Result<(), EventError> {
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => return Err(EventError::Exit),
            Err(e) => return Err(EventError::TurnFailed(e.to_string())),
        };

        if line.trim().is_empty() {
            continue;
        }

        let msg: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                tracing::debug!(line = %line, "non-JSON agent output");
                continue;
            }
        };

        if let Some(error) = json_rpc_error_message(&msg) {
            return Err(EventError::TurnFailed(error));
        }

        if msg.get("id").and_then(|v| v.as_u64()) == Some(1) && msg.get("result").is_some() {
            entry.session.last_codex_event = Some("initialize".to_string());
            entry.session.last_codex_timestamp = Some(chrono::Utc::now());
            publish_session_update(entry, on_session_update);
            return Ok(());
        }
    }
}

async fn read_thread_start_response(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    entry: &mut RunningEntry,
    on_session_update: Option<&(dyn Fn(LiveSession) + Send + Sync)>,
) -> Result<String, EventError> {
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => return Err(EventError::Exit),
            Err(e) => return Err(EventError::TurnFailed(e.to_string())),
        };

        if line.trim().is_empty() {
            continue;
        }

        let msg: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                tracing::debug!(line = %line, "non-JSON agent output");
                continue;
            }
        };

        if let Some(error) = json_rpc_error_message(&msg) {
            return Err(EventError::TurnFailed(error));
        }

        if let Some(method) = msg.get("method").and_then(|v| v.as_str()) {
            entry.session.last_codex_event = Some(method.to_string());
            entry.session.last_codex_timestamp = Some(chrono::Utc::now());
            publish_session_update(entry, on_session_update);
        }

        if let Some(thread_id) = msg
            .pointer("/result/thread/id")
            .or_else(|| msg.pointer("/params/thread/id"))
            .and_then(|v| v.as_str())
        {
            entry.session.thread_id = Some(thread_id.to_string());
            publish_session_update(entry, on_session_update);
            return Ok(thread_id.to_string());
        }
    }
}

async fn read_agent_events(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    entry: &mut RunningEntry,
    on_session_update: Option<&(dyn Fn(LiveSession) + Send + Sync)>,
) -> Result<(), EventError> {
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => return Err(EventError::Exit),
            Err(e) => return Err(EventError::TurnFailed(e.to_string())),
        };

        if line.trim().is_empty() {
            continue;
        }

        // Parse JSON-RPC message
        let msg: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                tracing::debug!(line = %line, "non-JSON agent output");
                continue;
            }
        };

        if let Some(error) = json_rpc_error_message(&msg) {
            return Err(EventError::TurnFailed(error));
        }

        let event_type = msg
            .get("method")
            .or_else(|| msg.get("result").and_then(|r| r.get("type")))
            .and_then(|v| v.as_str());

        entry.session.last_codex_event = event_type.map(String::from);
        entry.session.last_codex_timestamp = Some(chrono::Utc::now());
        entry.session.last_codex_message = msg
            .get("result")
            .and_then(|r| r.get("message"))
            .or_else(|| msg.pointer("/params/message"))
            .and_then(|m| m.as_str())
            .map(String::from);

        if let Some(thread_id) = msg
            .pointer("/result/thread/id")
            .or_else(|| msg.pointer("/params/threadId"))
            .or_else(|| msg.pointer("/params/thread/id"))
            .and_then(|v| v.as_str())
        {
            entry.session.thread_id = Some(thread_id.to_string());
        }

        if let Some(turn_id) = msg
            .pointer("/result/turn/id")
            .or_else(|| msg.pointer("/params/turn/id"))
            .and_then(|v| v.as_str())
        {
            entry.session.turn_id = Some(turn_id.to_string());
            if let Some(thread_id) = entry.session.thread_id.as_deref() {
                entry.session.session_id = Some(make_session_id(thread_id, turn_id));
            }
        }

        // Parse token usage if present
        if let Some(usage) = msg
            .pointer("/result/usage")
            .or_else(|| msg.pointer("/params/tokenUsage/last"))
        {
            if let Some(input) = usage.get("inputTokens").and_then(|v| v.as_u64()) {
                entry.session.codex_input_tokens += input;
                entry.session.last_reported_input_tokens = input;
            }
            if let Some(output) = usage.get("outputTokens").and_then(|v| v.as_u64()) {
                entry.session.codex_output_tokens += output;
                entry.session.last_reported_output_tokens = output;
            }
        }

        if let Some(total) = msg.pointer("/params/tokenUsage/total") {
            if let Some(input) = total.get("inputTokens").and_then(|v| v.as_u64()) {
                entry.session.codex_input_tokens = input;
            }
            if let Some(output) = total.get("outputTokens").and_then(|v| v.as_u64()) {
                entry.session.codex_output_tokens = output;
            }
            if let Some(total_tokens) = total.get("totalTokens").and_then(|v| v.as_u64()) {
                entry.session.codex_total_tokens = total_tokens;
            }
        }

        publish_session_update(entry, on_session_update);

        match event_type {
            Some("v2/TurnCompleted") | Some("turn_completed") | Some("turn/completed") => {
                tracing::info!(session = ?entry.session.session_id, "turn completed event");
                if let Some(status) = msg.pointer("/params/turn/status").and_then(|v| v.as_str())
                    && status == "failed"
                {
                    let reason = msg
                        .pointer("/params/turn/error/message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("turn failed");
                    return Err(EventError::TurnFailed(reason.to_string()));
                }
                return Ok(());
            }
            Some("v2/TurnFailed") | Some("turn_failed") => {
                let reason = msg
                    .pointer("/result/error/message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                return Err(EventError::TurnFailed(reason.to_string()));
            }
            Some("exit") => {
                return Err(EventError::Exit);
            }
            _ => {
                // Notification or other event — consume and continue
                tracing::trace!(event = ?event_type, "agent event");
            }
        }
    }
}

fn json_rpc_error_message(msg: &serde_json::Value) -> Option<String> {
    let error = msg.get("error")?;
    let message = error
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("JSON-RPC error");
    let code = error.get("code").and_then(|v| v.as_i64());

    Some(match code {
        Some(code) => format!("JSON-RPC error {code}: {message}"),
        None => format!("JSON-RPC error: {message}"),
    })
}

fn publish_session_update(
    entry: &RunningEntry,
    on_session_update: Option<&(dyn Fn(LiveSession) + Send + Sync)>,
) {
    if let Some(callback) = on_session_update {
        callback(entry.session.clone());
    }
}

async fn wait_for_cancel(cancelled: CancelHandle) {
    while !cancelled.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ── Mock agent launcher (for testing) ───────────────────────────────────────

/// Launch a mock agent that simulates a successful coding-agent run.
/// This writes to a control file in the workspace so the orchestrator
/// can simulate the agent protocol.
pub async fn run_mock_agent(workspace: &Path, issue_id: &str) -> Result<(), String> {
    let control_path = workspace.join(".symphony_agent_control");

    // Simulate a 3-turn agent session
    let events = vec![
        serde_json::json!({
            "method": "notification",
            "params": { "message": format!("Starting work on {issue_id}"), "type": "notification" }
        }),
        serde_json::json!({
            "result": {
                "type": "v2/TurnCompleted",
                "message": "Turn 1 done: read the codebase",
                "usage": { "inputTokens": 100, "outputTokens": 200 }
            },
            "id": 1
        }),
        serde_json::json!({
            "method": "notification",
            "params": { "message": "Starting turn 2", "type": "notification" }
        }),
        serde_json::json!({
            "result": {
                "type": "v2/TurnCompleted",
                "message": "Turn 2 done: wrote tests",
                "usage": { "inputTokens": 150, "outputTokens": 300 }
            },
            "id": 2
        }),
        serde_json::json!({
            "method": "notification",
            "params": { "message": "Starting turn 3", "type": "notification" }
        }),
        serde_json::json!({
            "result": {
                "type": "v2/TurnCompleted",
                "message": "Turn 3 done: implemented fix, created PR",
                "usage": { "inputTokens": 200, "outputTokens": 500 }
            },
            "id": 3
        }),
    ];

    let content = serde_json::to_string_pretty(&events).unwrap();
    std::fs::write(&control_path, content).map_err(|e| e.to_string())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn test_config() -> AgentRunnerConfig {
        AgentRunnerConfig {
            command: "codex app-server".into(),
            workspace_path: PathBuf::from("/tmp/symphony-workspace"),
            approval_policy: "on-request".into(),
            thread_sandbox: "danger-full-access".into(),
            turn_sandbox_policy: "read-only".into(),
            turn_timeout_ms: 1,
            read_timeout_ms: 1,
            stall_timeout_ms: 1,
            max_turns: 1,
            stop_after_first_turn: false,
            on_session_update: None,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn thread_start_request_includes_configured_runtime_policy() {
        let cwd = Path::new("/tmp/symphony-workspace");
        let msg = thread_start_request(2, &test_config(), cwd);

        assert_eq!(msg["method"], "thread/start");
        assert_eq!(msg.pointer("/params/approvalPolicy").unwrap(), "onRequest");
        assert_eq!(msg.pointer("/params/sandbox").unwrap(), "dangerFullAccess");
        assert_eq!(
            msg.pointer("/params/cwd").unwrap(),
            "/tmp/symphony-workspace"
        );
        assert!(msg.pointer("/params/prompt").is_none());
    }

    #[test]
    fn turn_start_request_includes_configured_runtime_policy() {
        let cwd = Path::new("/tmp/symphony-workspace");
        let msg = turn_start_request(3, "thread-1", "prompt", &test_config(), cwd);

        assert_eq!(msg["method"], "turn/start");
        assert_eq!(msg.pointer("/params/threadId").unwrap(), "thread-1");
        assert_eq!(msg.pointer("/params/input/0/text").unwrap(), "prompt");
        assert_eq!(msg.pointer("/params/approvalPolicy").unwrap(), "onRequest");
        assert_eq!(
            msg.pointer("/params/sandboxPolicy").unwrap(),
            &serde_json::json!({ "type": "readOnly" })
        );
        assert!(msg.pointer("/params/turnId").is_none());
    }

    #[test]
    fn workspace_write_turn_sandbox_keeps_network_enabled_default() {
        let mut config = test_config();
        config.turn_sandbox_policy = "workspace-write".into();
        let cwd = Path::new("/tmp/symphony-workspace");
        let msg = turn_start_request(3, "thread-1", "prompt", &config, cwd);

        assert_eq!(
            msg.pointer("/params/sandboxPolicy").unwrap(),
            &serde_json::json!({
                "type": "workspaceWrite",
                "writableRoots": ["/tmp/symphony-workspace"],
                "networkAccess": true,
                "excludeTmpdirEnvVar": false,
                "excludeSlashTmp": false,
            })
        );
    }
}
