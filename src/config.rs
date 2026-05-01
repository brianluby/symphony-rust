use std::collections::HashMap;

use serde::Deserialize;

pub const DEFAULT_CODEX_APPROVAL_POLICY: &str = "never";
pub const DEFAULT_CODEX_THREAD_SANDBOX: &str = "workspace-write";
pub const DEFAULT_CODEX_TURN_SANDBOX_POLICY: &str = "workspace-write";

const CODEX_APPROVAL_POLICY_VALUES: &str = "never, on-request, on-failure, unless-trusted";
const CODEX_SANDBOX_VALUES: &str = "read-only, workspace-write, danger-full-access";

// ── Typed Service Config (§6.4) ─────────────────────────────────────────────

/// Typed runtime values derived from `WORKFLOW.md` front matter with defaults
/// and environment variable indirection resolved.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    // tracker (§5.3.1)
    pub tracker_kind: String,
    pub tracker_endpoint: String,
    pub tracker_api_key: Option<String>,
    pub tracker_project_slug: Option<String>,
    pub tracker_active_states: Vec<String>,
    pub tracker_terminal_states: Vec<String>,
    pub tracker_claim_state: Option<String>,
    pub tracker_completion_state: Option<String>,

    // polling (§5.3.2)
    pub poll_interval_ms: u64,

    // workspace (§5.3.3)
    pub workspace_root: String,

    // hooks (§5.3.4)
    pub hooks_after_create: Option<String>,
    pub hooks_before_run: Option<String>,
    pub hooks_after_run: Option<String>,
    pub hooks_before_remove: Option<String>,
    pub hooks_timeout_ms: u64,

    // agent (§5.3.5)
    pub agent_max_concurrent_agents: u32,
    pub agent_max_turns: u32,
    pub agent_max_retry_backoff_ms: u64,
    pub agent_max_concurrent_agents_by_state: HashMap<String, u32>,

    // codex (§5.3.6)
    pub codex_command: String,
    pub codex_approval_policy: Option<String>,
    pub codex_thread_sandbox: Option<String>,
    pub codex_turn_sandbox_policy: Option<String>,
    pub codex_turn_timeout_ms: u64,
    pub codex_read_timeout_ms: u64,
    pub codex_stall_timeout_ms: u64,

    /// If true, skip credential validation (mock mode).
    pub mock_mode: bool,

    /// If true, use mock agent instead of real codex subprocess.
    pub mock_agent: bool,

    /// Prompt template from WORKFLOW.md (Liquid template body).
    pub prompt_template: String,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            tracker_kind: "linear".into(),
            tracker_endpoint: "https://api.linear.app/graphql".into(),
            tracker_api_key: None,
            tracker_project_slug: None,
            tracker_active_states: vec!["Todo".into(), "In Progress".into()],
            tracker_terminal_states: vec![
                "Closed".into(),
                "Cancelled".into(),
                "Canceled".into(),
                "Duplicate".into(),
                "Done".into(),
            ],
            tracker_claim_state: None,
            tracker_completion_state: None,
            poll_interval_ms: 30_000,
            workspace_root: String::new(), // filled during init
            hooks_after_create: None,
            hooks_before_run: None,
            hooks_after_run: None,
            hooks_before_remove: None,
            hooks_timeout_ms: 60_000,
            agent_max_concurrent_agents: 10,
            agent_max_turns: 20,
            agent_max_retry_backoff_ms: 300_000,
            agent_max_concurrent_agents_by_state: HashMap::new(),
            codex_command: "codex app-server".into(),
            codex_approval_policy: Some(DEFAULT_CODEX_APPROVAL_POLICY.into()),
            codex_thread_sandbox: Some(DEFAULT_CODEX_THREAD_SANDBOX.into()),
            codex_turn_sandbox_policy: Some(DEFAULT_CODEX_TURN_SANDBOX_POLICY.into()),
            codex_turn_timeout_ms: 3_600_000,
            codex_read_timeout_ms: 5_000,
            codex_stall_timeout_ms: 300_000u64,
            mock_mode: false,
            mock_agent: false,
            prompt_template: String::new(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawConfig {
    tracker: Option<RawTrackerConfig>,
    polling: Option<RawPollingConfig>,
    workspace: Option<RawWorkspaceConfig>,
    hooks: Option<RawHooksConfig>,
    agent: Option<RawAgentConfig>,
    codex: Option<RawCodexConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawTrackerConfig {
    kind: Option<String>,
    endpoint: Option<String>,
    api_key: Option<String>,
    project_slug: Option<String>,
    active_states: Option<Vec<String>>,
    terminal_states: Option<Vec<String>>,
    claim_state: Option<String>,
    completion_state: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawPollingConfig {
    interval_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawWorkspaceConfig {
    root: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawHooksConfig {
    after_create: Option<String>,
    before_run: Option<String>,
    after_run: Option<String>,
    before_remove: Option<String>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawAgentConfig {
    max_concurrent_agents: Option<u32>,
    max_turns: Option<u32>,
    max_retry_backoff_ms: Option<u64>,
    max_concurrent_agents_by_state: Option<HashMap<String, u32>>,
    claim_state: Option<String>,
    completion_state: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawCodexConfig {
    command: Option<String>,
    approval_policy: Option<String>,
    thread_sandbox: Option<String>,
    turn_sandbox_policy: Option<String>,
    turn_timeout_ms: Option<u64>,
    read_timeout_ms: Option<u64>,
    stall_timeout_ms: Option<u64>,
}

impl ServiceConfig {
    /// Build a `ServiceConfig` from a raw YAML config map, applying defaults
    /// and resolving `$VAR` environment variable indirection.
    pub fn from_yaml(
        config: &serde_yaml::Value,
        workflow_dir: &std::path::Path,
    ) -> Result<Self, ConfigError> {
        let mut sc = Self::default();
        let raw: RawConfig = if config.is_null() {
            RawConfig::default()
        } else {
            serde_yaml::from_value(config.clone())
                .map_err(|e| ConfigError::InvalidConfig(e.to_string()))?
        };

        if let Some(tracker) = raw.tracker {
            if let Some(kind) = tracker.kind {
                sc.tracker_kind = kind;
            }
            if let Some(endpoint) = tracker.endpoint {
                sc.tracker_endpoint = endpoint;
            }

            sc.tracker_api_key = tracker.api_key.as_deref().map(resolve_env_var);
            sc.tracker_project_slug = tracker.project_slug;

            if let Some(states) = tracker.active_states {
                sc.tracker_active_states = states;
            }
            if let Some(states) = tracker.terminal_states {
                sc.tracker_terminal_states = states;
            }
            sc.tracker_claim_state = tracker.claim_state;
            sc.tracker_completion_state = tracker.completion_state;
        }

        if let Some(polling) = raw.polling
            && let Some(interval_ms) = polling.interval_ms
        {
            sc.poll_interval_ms = interval_ms;
        }

        sc.workspace_root = raw
            .workspace
            .and_then(|ws| ws.root)
            .map(|root| expand_path(&root, workflow_dir))
            .unwrap_or_else(default_workspace_root);

        if let Some(hooks) = raw.hooks {
            sc.hooks_after_create = hooks.after_create;
            sc.hooks_before_run = hooks.before_run;
            sc.hooks_after_run = hooks.after_run;
            sc.hooks_before_remove = hooks.before_remove;
            if let Some(timeout_ms) = hooks.timeout_ms {
                sc.hooks_timeout_ms = timeout_ms;
            }
        }

        if let Some(agent) = raw.agent {
            if let Some(max_concurrent_agents) = agent.max_concurrent_agents {
                sc.agent_max_concurrent_agents = max_concurrent_agents;
            }
            if let Some(max_turns) = agent.max_turns {
                sc.agent_max_turns = max_turns;
            }
            if let Some(max_retry_backoff_ms) = agent.max_retry_backoff_ms {
                sc.agent_max_retry_backoff_ms = max_retry_backoff_ms;
            }
            if let Some(by_state) = agent.max_concurrent_agents_by_state {
                sc.agent_max_concurrent_agents_by_state = by_state
                    .into_iter()
                    .filter(|(_state, max)| *max > 0)
                    .map(|(state, max)| (state.to_lowercase(), max))
                    .collect();
            }
            merge_tracker_handoff_state(
                "tracker.claim_state",
                "agent.claim_state",
                &mut sc.tracker_claim_state,
                agent.claim_state,
            )?;
            merge_tracker_handoff_state(
                "tracker.completion_state",
                "agent.completion_state",
                &mut sc.tracker_completion_state,
                agent.completion_state,
            )?;
        }

        if let Some(codex) = raw.codex {
            if let Some(command) = codex.command {
                sc.codex_command = command;
            }
            if let Some(approval_policy) = codex.approval_policy {
                sc.codex_approval_policy = Some(
                    normalize_codex_approval_policy(&approval_policy)
                        .ok_or_else(|| {
                            invalid_value(
                                "codex.approval_policy",
                                approval_policy,
                                CODEX_APPROVAL_POLICY_VALUES,
                            )
                        })?
                        .into(),
                );
            }
            if let Some(thread_sandbox) = codex.thread_sandbox {
                sc.codex_thread_sandbox = Some(
                    normalize_codex_sandbox(&thread_sandbox)
                        .ok_or_else(|| {
                            invalid_value(
                                "codex.thread_sandbox",
                                thread_sandbox,
                                CODEX_SANDBOX_VALUES,
                            )
                        })?
                        .into(),
                );
            }
            if let Some(turn_sandbox_policy) = codex.turn_sandbox_policy {
                sc.codex_turn_sandbox_policy = Some(
                    normalize_codex_sandbox(&turn_sandbox_policy)
                        .ok_or_else(|| {
                            invalid_value(
                                "codex.turn_sandbox_policy",
                                turn_sandbox_policy,
                                CODEX_SANDBOX_VALUES,
                            )
                        })?
                        .into(),
                );
            }
            if let Some(turn_timeout_ms) = codex.turn_timeout_ms {
                sc.codex_turn_timeout_ms = turn_timeout_ms;
            }
            if let Some(read_timeout_ms) = codex.read_timeout_ms {
                sc.codex_read_timeout_ms = read_timeout_ms;
            }
            if let Some(stall_timeout_ms) = codex.stall_timeout_ms {
                sc.codex_stall_timeout_ms = stall_timeout_ms;
            }
        }

        Ok(sc)
    }

    /// Validate dispatch-critical config. Returns `Ok(())` if the orchestrator
    /// can safely dispatch work.
    pub fn validate_dispatch(&self) -> Result<(), ConfigError> {
        self.validate_codex_runtime_policy()?;
        self.validate_tracker_handoff_policy()?;

        // Mock mode skips credential and project slug checks
        if self.mock_mode {
            if self.codex_command.is_empty() {
                return Err(ConfigError::MissingField("codex.command".into()));
            }
            return Ok(());
        }

        if self.tracker_kind.is_empty() {
            return Err(ConfigError::MissingField("tracker.kind".into()));
        }
        if self.tracker_kind != "linear" {
            return Err(ConfigError::UnsupportedTracker(self.tracker_kind.clone()));
        }
        if self.tracker_api_key.as_deref().is_none_or(str::is_empty) {
            return Err(ConfigError::MissingCredentials);
        }
        if self
            .tracker_project_slug
            .as_deref()
            .is_none_or(str::is_empty)
        {
            return Err(ConfigError::MissingField("tracker.project_slug".into()));
        }
        if self.codex_command.is_empty() {
            return Err(ConfigError::MissingField("codex.command".into()));
        }
        Ok(())
    }

    fn validate_codex_runtime_policy(&self) -> Result<(), ConfigError> {
        validate_optional_value(
            "codex.approval_policy",
            self.codex_approval_policy.as_deref(),
            CODEX_APPROVAL_POLICY_VALUES,
            normalize_codex_approval_policy,
        )?;
        validate_optional_value(
            "codex.thread_sandbox",
            self.codex_thread_sandbox.as_deref(),
            CODEX_SANDBOX_VALUES,
            normalize_codex_sandbox,
        )?;
        validate_optional_value(
            "codex.turn_sandbox_policy",
            self.codex_turn_sandbox_policy.as_deref(),
            CODEX_SANDBOX_VALUES,
            normalize_codex_sandbox,
        )?;
        Ok(())
    }

    fn validate_tracker_handoff_policy(&self) -> Result<(), ConfigError> {
        validate_optional_non_empty("tracker.claim_state", self.tracker_claim_state.as_deref())?;
        validate_optional_non_empty(
            "tracker.completion_state",
            self.tracker_completion_state.as_deref(),
        )?;

        if let Some(claim_state) = self.tracker_claim_state.as_deref()
            && !self
                .tracker_active_states
                .iter()
                .any(|state| state.eq_ignore_ascii_case(claim_state))
        {
            return Err(ConfigError::InvalidConfig(format!(
                "tracker.claim_state must be listed in tracker.active_states: {claim_state}"
            )));
        }

        if let Some(completion_state) = self.tracker_completion_state.as_deref()
            && self
                .tracker_active_states
                .iter()
                .any(|state| state.eq_ignore_ascii_case(completion_state))
        {
            return Err(ConfigError::InvalidConfig(format!(
                "tracker.completion_state must not be listed in tracker.active_states: {completion_state}"
            )));
        }

        Ok(())
    }
}

// ── Config Errors ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, thiserror::Error)]
pub enum ConfigError {
    #[error("missing_workflow_file: {0}")]
    MissingWorkflowFile(String),
    #[error("workflow_parse_error: {0}")]
    WorkflowParseError(String),
    #[error("workflow_front_matter_not_a_map")]
    WorkflowFrontMatterNotAMap,
    #[error("missing required field: {0}")]
    MissingField(String),
    #[error("unsupported tracker kind: {0}")]
    UnsupportedTracker(String),
    #[error("missing tracker credentials")]
    MissingCredentials,
    #[error("template_parse_error: {0}")]
    TemplateParseError(String),
    #[error("template_render_error: {0}")]
    TemplateRenderError(String),
    #[error("invalid config: {0}")]
    InvalidConfig(String),
    #[error("invalid value for {field}: {value}; allowed values: {allowed}")]
    InvalidValue {
        field: String,
        value: String,
        allowed: &'static str,
    },
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn default_workspace_root() -> String {
    std::env::temp_dir()
        .join("symphony_workspaces")
        .to_string_lossy()
        .into_owned()
}

/// Expand `$VAR_NAME` or `${VAR_NAME}` references in a string, preserving
/// any suffix after the variable name.
/// Examples:
///   `$HOME/workspaces`  → `/home/user/workspaces`
///   `${HOME}/workspaces` → `/home/user/workspaces`
///   `$UNSET/default`    → `/default`
pub fn resolve_env_var(value: &str) -> String {
    if !value.starts_with('$') {
        return value.to_string();
    }

    let rest = &value[1..];

    // ${VAR} syntax
    if rest.starts_with('{')
        && let Some(end) = rest.find('}')
    {
        let var = &rest[1..end];
        let suffix = &rest[end + 1..];
        let resolved = std::env::var(var).unwrap_or_default();
        return format!("{resolved}{suffix}");
    }

    // $VAR syntax — split on first non-alphanumeric, non-underscore char
    if let Some(pos) = rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        let var = &rest[..pos];
        let suffix = &rest[pos..];
        let resolved = std::env::var(var).unwrap_or_default();
        return format!("{resolved}{suffix}");
    }

    // Whole string is the variable name
    std::env::var(rest).unwrap_or_default()
}

/// Expand `~` and `$VAR` in paths. Relative paths resolve against `workflow_dir`.
pub fn expand_path(raw: &str, workflow_dir: &std::path::Path) -> String {
    let resolved = resolve_env_var(raw);

    // Expand tilde
    let expanded = if resolved.starts_with('~') {
        if let Some(home) = dirs::home_dir() {
            resolved.replacen('~', &home.to_string_lossy(), 1)
        } else {
            resolved
        }
    } else {
        resolved
    };

    // Resolve relative paths
    let path = std::path::Path::new(&expanded);
    if path.is_absolute() {
        expanded
    } else {
        workflow_dir.join(path).to_string_lossy().into_owned()
    }
}

pub fn normalize_codex_approval_policy(value: &str) -> Option<&'static str> {
    match value.trim() {
        "never" => Some("never"),
        "on-request" | "on_request" | "onRequest" => Some("on-request"),
        "on-failure" | "on_failure" | "onFailure" => Some("on-failure"),
        "unless-trusted" | "unless_trusted" | "unlessTrusted" | "untrusted" => {
            Some("unless-trusted")
        }
        _ => None,
    }
}

pub fn normalize_codex_sandbox(value: &str) -> Option<&'static str> {
    match value.trim() {
        "read-only" | "read_only" | "readOnly" | "readonly" => Some("read-only"),
        "workspace-write" | "workspace_write" | "workspaceWrite" => Some("workspace-write"),
        "danger-full-access" | "danger_full_access" | "dangerFullAccess" => {
            Some("danger-full-access")
        }
        _ => None,
    }
}

fn validate_optional_value(
    field: &str,
    value: Option<&str>,
    allowed: &'static str,
    normalize: fn(&str) -> Option<&'static str>,
) -> Result<(), ConfigError> {
    if let Some(value) = value
        && normalize(value).is_none()
    {
        return Err(invalid_value(field, value, allowed));
    }
    Ok(())
}

fn validate_optional_non_empty(field: &str, value: Option<&str>) -> Result<(), ConfigError> {
    if let Some(value) = value
        && value.trim().is_empty()
    {
        return Err(ConfigError::MissingField(field.into()));
    }
    Ok(())
}

fn merge_tracker_handoff_state(
    preferred_field: &str,
    alias_field: &str,
    preferred_value: &mut Option<String>,
    alias_value: Option<String>,
) -> Result<(), ConfigError> {
    let Some(alias_value) = alias_value else {
        return Ok(());
    };

    if let Some(existing) = preferred_value.as_deref()
        && existing != alias_value
    {
        return Err(ConfigError::InvalidConfig(format!(
            "{alias_field} conflicts with {preferred_field}"
        )));
    }

    *preferred_value = Some(alias_value);
    Ok(())
}

fn invalid_value(field: &str, value: impl Into<String>, allowed: &'static str) -> ConfigError {
    ConfigError::InvalidValue {
        field: field.into(),
        value: value.into(),
        allowed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = ServiceConfig::default();
        assert_eq!(cfg.tracker_kind, "linear");
        assert_eq!(cfg.poll_interval_ms, 30_000);
        assert_eq!(cfg.agent_max_concurrent_agents, 10);
        assert_eq!(cfg.agent_max_turns, 20);
        assert_eq!(cfg.hooks_timeout_ms, 60_000);
        assert_eq!(cfg.codex_stall_timeout_ms, 300_000);
        assert_eq!(cfg.codex_approval_policy.as_deref(), Some("never"));
        assert_eq!(cfg.codex_thread_sandbox.as_deref(), Some("workspace-write"));
        assert_eq!(
            cfg.codex_turn_sandbox_policy.as_deref(),
            Some("workspace-write")
        );
    }

    #[test]
    fn test_resolve_env_var() {
        unsafe {
            std::env::set_var("TEST_VAR", "secret123");
            std::env::set_var("TEST_HOME", "/home/user");
        }
        assert_eq!(resolve_env_var("$TEST_VAR"), "secret123");
        assert_eq!(resolve_env_var("plaintext"), "plaintext");
        // Suffix preservation
        assert_eq!(
            resolve_env_var("$TEST_HOME/workspaces"),
            "/home/user/workspaces"
        );
        // Brace syntax
        assert_eq!(
            resolve_env_var("${TEST_HOME}/workspaces"),
            "/home/user/workspaces"
        );
        // Unset var with suffix
        assert_eq!(resolve_env_var("$UNSET_VAR/fallback"), "/fallback");
    }

    #[test]
    fn test_expand_tilde() {
        let home = dirs::home_dir().unwrap();
        let expanded = expand_path("~/workspaces", std::path::Path::new("."));
        assert!(expanded.starts_with(&home.to_string_lossy().into_owned()));
    }

    #[test]
    fn test_config_from_yaml_minimal() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
tracker:
  kind: linear
  api_key: $LINEAR_API_KEY
  project_slug: my-project
codex:
  command: codex app-server
"#,
        )
        .unwrap();

        unsafe {
            std::env::set_var("LINEAR_API_KEY", "test-key");
        }
        let cfg = ServiceConfig::from_yaml(&yaml, std::path::Path::new(".")).unwrap();
        assert_eq!(cfg.tracker_api_key.unwrap(), "test-key");
        assert_eq!(cfg.tracker_project_slug.unwrap(), "my-project");
    }

    #[test]
    fn test_config_from_yaml_tracker_handoff_policy() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
tracker:
  kind: linear
  active_states:
    - Todo
    - In Progress
  claim_state: In Progress
  completion_state: In Review
"#,
        )
        .unwrap();

        let mut cfg = ServiceConfig::from_yaml(&yaml, std::path::Path::new(".")).unwrap();
        cfg.mock_mode = true;
        assert_eq!(cfg.tracker_claim_state.as_deref(), Some("In Progress"));
        assert_eq!(cfg.tracker_completion_state.as_deref(), Some("In Review"));
        assert!(cfg.validate_dispatch().is_ok());
    }

    #[test]
    fn test_config_accepts_legacy_agent_handoff_aliases() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
agent:
  claim_state: In Progress
  completion_state: In Review
"#,
        )
        .unwrap();

        let cfg = ServiceConfig::from_yaml(&yaml, std::path::Path::new(".")).unwrap();
        assert_eq!(cfg.tracker_claim_state.as_deref(), Some("In Progress"));
        assert_eq!(cfg.tracker_completion_state.as_deref(), Some("In Review"));
    }

    #[test]
    fn test_config_rejects_conflicting_handoff_aliases() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
tracker:
  completion_state: In Review
agent:
  completion_state: Done
"#,
        )
        .unwrap();

        let err = ServiceConfig::from_yaml(&yaml, std::path::Path::new(".")).unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidConfig(message) if message.contains("agent.completion_state conflicts with tracker.completion_state"))
        );
    }

    #[test]
    fn test_config_rejects_unknown_keys() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
tracker:
  kind: linear
  api_key: test-key
  project_slug: my-project
  project_slog: typo
"#,
        )
        .unwrap();

        let err = ServiceConfig::from_yaml(&yaml, std::path::Path::new(".")).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidConfig(_)));
    }

    #[test]
    fn test_config_normalizes_codex_runtime_policy() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
codex:
  approval_policy: onRequest
  thread_sandbox: dangerFullAccess
  turn_sandbox_policy: readOnly
"#,
        )
        .unwrap();

        let cfg = ServiceConfig::from_yaml(&yaml, std::path::Path::new(".")).unwrap();
        assert_eq!(cfg.codex_approval_policy.as_deref(), Some("on-request"));
        assert_eq!(
            cfg.codex_thread_sandbox.as_deref(),
            Some("danger-full-access")
        );
        assert_eq!(cfg.codex_turn_sandbox_policy.as_deref(), Some("read-only"));
    }

    #[test]
    fn test_config_rejects_invalid_codex_runtime_policy() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
codex:
  approval_policy: always
"#,
        )
        .unwrap();

        let err = ServiceConfig::from_yaml(&yaml, std::path::Path::new(".")).unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidValue { field, .. } if field == "codex.approval_policy")
        );
    }

    #[test]
    fn test_validate_dispatch_missing_key() {
        let cfg = ServiceConfig {
            tracker_api_key: None,
            ..ServiceConfig::default()
        };
        assert!(cfg.validate_dispatch().is_err());
    }

    #[test]
    fn test_validate_dispatch_rejects_empty_handoff_state() {
        let cfg = ServiceConfig {
            mock_mode: true,
            tracker_completion_state: Some(" ".into()),
            ..ServiceConfig::default()
        };

        assert!(
            matches!(cfg.validate_dispatch(), Err(ConfigError::MissingField(field)) if field == "tracker.completion_state")
        );
    }

    #[test]
    fn test_validate_dispatch_rejects_claim_state_outside_active_states() {
        let cfg = ServiceConfig {
            mock_mode: true,
            tracker_active_states: vec!["Todo".into()],
            tracker_claim_state: Some("In Progress".into()),
            ..ServiceConfig::default()
        };

        assert!(matches!(
            cfg.validate_dispatch(),
            Err(ConfigError::InvalidConfig(message))
                if message.contains("tracker.claim_state must be listed in tracker.active_states")
        ));
    }

    #[test]
    fn test_validate_dispatch_rejects_completion_state_inside_active_states() {
        let cfg = ServiceConfig {
            mock_mode: true,
            tracker_active_states: vec!["Todo".into(), "In Review".into()],
            tracker_completion_state: Some("In Review".into()),
            ..ServiceConfig::default()
        };

        assert!(matches!(
            cfg.validate_dispatch(),
            Err(ConfigError::InvalidConfig(message))
                if message.contains("tracker.completion_state must not be listed in tracker.active_states")
        ));
    }
}
