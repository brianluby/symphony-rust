//! Core domain types — the stable internal model used across all modules.
//! Maps to SPEC.md §4 (Core Domain Model).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

// ── Issue (§4.1.1) ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockerRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub blocked_by: Vec<BlockerRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

impl Issue {
    /// Normalized state (lowercased) for comparisons.
    pub fn state_normalized(&self) -> String {
        self.state.to_lowercase()
    }

    /// Whether this issue is in an "active" state per the given list.
    pub fn is_active(&self, active_states: &[String]) -> bool {
        let norm = self.state_normalized();
        active_states.iter().any(|s| s.to_lowercase() == norm)
    }

    /// Whether this issue is in a "terminal" state per the given list.
    pub fn is_terminal(&self, terminal_states: &[String]) -> bool {
        let norm = self.state_normalized();
        terminal_states.iter().any(|s| s.to_lowercase() == norm)
    }
}

// ── Workflow Definition (§4.1.2) ────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct WorkflowDefinition {
    pub config: serde_yaml::Value,
    pub prompt_template: String,
}

// ── Workspace (§4.1.4) ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Workspace {
    pub path: PathBuf,
    #[allow(dead_code)]
    pub workspace_key: String,
    pub created_now: bool,
}

// ── Live Session (§4.1.6) ───────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct LiveSession {
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    #[allow(dead_code)]
    pub codex_app_server_pid: Option<String>,
    pub last_codex_event: Option<String>,
    pub last_codex_timestamp: Option<DateTime<Utc>>,
    pub last_codex_message: Option<String>,
    pub codex_input_tokens: u64,
    pub codex_output_tokens: u64,
    pub codex_total_tokens: u64,
    pub last_reported_input_tokens: u64,
    pub last_reported_output_tokens: u64,
    pub turn_count: u64,
}

/// Cancellation handle: shared between orchestrator reconciliation and the
/// spawned worker task. Set to `true` by reconciliation to signal the worker
/// should stop and not schedule further retries.
pub type CancelHandle = std::sync::Arc<std::sync::atomic::AtomicBool>;

#[derive(Debug, Clone)]
pub struct RunningEntry {
    pub issue_id: String,
    pub identifier: String,
    pub issue: Issue,
    pub session: LiveSession,
    pub retry_attempt: Option<u32>,
    pub started_at: DateTime<Utc>,
    /// Set to true by reconciliation; worker checks this before reporting outcome.
    pub cancelled: CancelHandle,
}

// ── Retry Entry (§4.1.7) ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RetryEntry {
    pub issue_id: String,
    pub identifier: String,
    pub attempt: u32,
    pub due_at_ms: u64,
    pub error: Option<String>,
}

// ── Orchestrator Runtime State (§4.1.8) ─────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct CodexTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub seconds_running: f64,
}

#[derive(Debug, Clone)]
pub struct OrchestratorState {
    #[allow(dead_code)]
    pub poll_interval_ms: u64,
    pub max_concurrent_agents: u32,
    pub running: HashMap<String, RunningEntry>,
    pub claimed: HashSet<String>,
    pub retry_attempts: HashMap<String, RetryEntry>,
    pub completed: HashSet<String>,
    pub codex_totals: CodexTotals,
    #[allow(dead_code)]
    pub codex_rate_limits: Option<serde_json::Value>,
}

impl OrchestratorState {
    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    pub fn available_slots(&self) -> usize {
        let max = self.max_concurrent_agents as usize;
        let running = self.running_count();
        max.saturating_sub(running)
    }
}

// ── Workspace Key Sanitization (§4.2) ───────────────────────────────────────

pub fn sanitize_workspace_key(identifier: &str) -> String {
    identifier
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ── Session ID Composition (§4.2) ───────────────────────────────────────────

pub fn make_session_id(thread_id: &str, turn_id: &str) -> String {
    format!("{}-{}", thread_id, turn_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_workspace_key() {
        assert_eq!(sanitize_workspace_key("ABC-123"), "ABC-123");
        assert_eq!(sanitize_workspace_key("TEAM/456"), "TEAM_456");
        assert_eq!(sanitize_workspace_key("some.thing_ok"), "some.thing_ok");
        assert_eq!(sanitize_workspace_key("hello world!"), "hello_world_");
    }

    #[test]
    fn test_make_session_id() {
        assert_eq!(make_session_id("th-1", "t-5"), "th-1-t-5");
    }

    #[test]
    fn test_issue_state_normalization() {
        let issue = Issue {
            id: "1".into(),
            identifier: "T-1".into(),
            title: "Test".into(),
            description: None,
            priority: None,
            state: "In Progress".into(),
            branch_name: None,
            url: None,
            labels: vec![],
            blocked_by: vec![],
            created_at: None,
            updated_at: None,
        };
        assert_eq!(issue.state_normalized(), "in progress");
        assert!(issue.is_active(&["todo".into(), "in progress".into()]));
        assert!(!issue.is_terminal(&["done".into()]));
    }
}
