//! Symphony — orchestrator that turns an issue tracker into a control plane
//! for long-running coding agents. Reference implementation in Rust.
//! Maps to https://github.com/openai/symphony/blob/main/SPEC.md

mod agent_runner;
mod config;
mod http;
mod logging;
mod orchestrator;
mod template;
mod tracker;
mod types;
mod workspace;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use config::ServiceConfig;
use logging::{LogFormat, LogVerbosity, LoggingConfig};
use orchestrator::Orchestrator;
use tracker::{LinearClient, MockTracker, TrackerClient};
use workspace::WorkspaceManager;

use crate::types::Issue;

/// Symphony: issue-tracker-driven coding agent orchestrator.
#[derive(Parser, Debug)]
#[command(
    name = "symphony",
    version,
    about = "Orchestrates coding agents from an issue tracker control plane.",
    long_about = None
)]
struct Cli {
    /// Path to WORKFLOW.md
    #[arg(short, long, default_value = "./WORKFLOW.md")]
    workflow_path: PathBuf,

    /// Workspace root directory (overrides WORKFLOW.md setting)
    #[arg(long)]
    workspace_root: Option<PathBuf>,

    /// HTTP server port (0 = disabled)
    #[arg(short, long, default_value = "0")]
    port: u16,

    /// Use mock tracker (for development/testing)
    #[arg(long)]
    mock_tracker: bool,

    /// Use mock agent (for development/testing)
    #[arg(long)]
    mock_agent: bool,

    /// JSON log output
    #[arg(short, long)]
    json_logs: bool,

    /// Verbose logging
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Initialize logging
    logging::init_logging(LoggingConfig {
        format: if cli.json_logs {
            LogFormat::Json
        } else {
            LogFormat::Text
        },
        verbosity: if cli.verbose {
            LogVerbosity::Verbose
        } else {
            LogVerbosity::Normal
        },
    });

    tracing::info!(
        workflow_path = %cli.workflow_path.display(),
        mock_tracker = cli.mock_tracker,
        mock_agent = cli.mock_agent,
        "symphony starting"
    );

    // Load WORKFLOW.md
    let workflow = load_workflow(&cli.workflow_path)?;

    // Resolve workspace root
    let workflow_dir = cli
        .workflow_path
        .parent()
        .unwrap_or(std::path::Path::new("."));

    // Build ServiceConfig from workflow front matter
    let mut svc_config = ServiceConfig::from_yaml(&workflow.config, workflow_dir)?;

    // CLI overrides
    if let Some(ref workspace_override) = cli.workspace_root {
        svc_config.workspace_root = workspace_override.to_string_lossy().into_owned();
    }
    svc_config.mock_mode = cli.mock_tracker;
    svc_config.mock_agent = cli.mock_agent;
    svc_config.prompt_template = workflow.prompt_template;

    tracing::info!(
        workspace_root = %svc_config.workspace_root,
        tracker_kind = %svc_config.tracker_kind,
        poll_interval_ms = svc_config.poll_interval_ms,
        max_concurrent = svc_config.agent_max_concurrent_agents,
        "config loaded"
    );

    // Create workspace manager
    let workspace_mgr = WorkspaceManager::new(PathBuf::from(&svc_config.workspace_root));

    // Apply hooks from config
    let workspace_mgr = WorkspaceManager {
        after_create: svc_config.hooks_after_create.clone(),
        before_run: svc_config.hooks_before_run.clone(),
        after_run: svc_config.hooks_after_run.clone(),
        before_remove: svc_config.hooks_before_remove.clone(),
        hook_timeout_ms: svc_config.hooks_timeout_ms,
        ..workspace_mgr
    };

    // Create tracker client
    let tracker: Arc<dyn TrackerClient> = if cli.mock_tracker {
        // Mock tracker with sample issues
        let mock_issues = vec![
            Issue {
                id: "mock-1".into(),
                identifier: "DEMO-1".into(),
                title: "Fix login timeout bug".into(),
                description: Some("Users report intermittent timeouts during SSO login".into()),
                priority: Some(2),
                state: "Todo".into(),
                branch_name: None,
                url: Some("https://linear.app/issue/DEMO-1".into()),
                labels: vec!["bug".into(), "p1".into()],
                blocked_by: vec![],
                created_at: Some(chrono::Utc::now()),
                updated_at: None,
            },
            Issue {
                id: "mock-2".into(),
                identifier: "DEMO-2".into(),
                title: "Add rate limiting to API".into(),
                description: Some("Implement token bucket rate limiter".into()),
                priority: Some(1),
                state: "Todo".into(),
                branch_name: None,
                url: Some("https://linear.app/issue/DEMO-2".into()),
                labels: vec!["feature".into()],
                blocked_by: vec![],
                created_at: Some(chrono::Utc::now()),
                updated_at: None,
            },
            Issue {
                id: "mock-3".into(),
                identifier: "DEMO-3".into(),
                title: "Update CI pipeline".into(),
                description: None,
                priority: Some(3),
                state: "Done".into(),
                branch_name: None,
                url: None,
                labels: vec![],
                blocked_by: vec![],
                created_at: Some(chrono::Utc::now()),
                updated_at: None,
            },
        ];
        Arc::new(MockTracker::new(mock_issues))
    } else {
        Arc::new(LinearClient::new(
            svc_config.tracker_endpoint.clone(),
            svc_config.tracker_api_key.clone(),
            svc_config.tracker_project_slug.clone().unwrap_or_default(),
            svc_config.tracker_active_states.clone(),
            svc_config.tracker_terminal_states.clone(),
        ))
    };

    // Optionally start HTTP server
    let (refresh_tx, refresh_rx) = tokio::sync::watch::channel(false);
    let orch_refresh_rx = if cli.port > 0 { Some(refresh_rx) } else { None };

    // Create orchestrator
    let orch = Orchestrator::new(
        svc_config.clone(),
        tracker,
        workspace_mgr,
        cli.workflow_path.clone(),
        orch_refresh_rx,
    );

    // Optionally start HTTP server
    if cli.port > 0 {
        let http_state = http::HttpState {
            orchestrator_state: Arc::clone(&orch.state),
            refresh_tx,
        };

        let port = cli.port;
        tokio::spawn(async move {
            if let Err(e) = http::start_http_server(http_state, port).await {
                tracing::error!(error = %e, "HTTP server failed");
            }
        });
    }

    // Run orchestrator loop
    orch.run().await?;

    Ok(())
}

/// Load and parse WORKFLOW.md into a WorkflowDefinition.
fn load_workflow(path: &PathBuf) -> Result<crate::types::WorkflowDefinition, config::ConfigError> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        config::ConfigError::MissingWorkflowFile(format!("{}: {e}", path.display()))
    })?;

    parse_workflow_content(&content)
}

/// Parse WORKFLOW.md content into front matter + prompt body.
/// Scans line-by-line for `---` delimiters (exact match after trimming).
pub fn parse_workflow_content(
    content: &str,
) -> Result<crate::types::WorkflowDefinition, config::ConfigError> {
    let trimmed = content.trim();

    if !trimmed.starts_with("---") {
        // No front matter — entire file is prompt body
        return Ok(crate::types::WorkflowDefinition {
            config: serde_yaml::Value::Null,
            prompt_template: trimmed.to_string(),
        });
    }

    // Find closing `---` delimiter: line that is exactly "---" after trimming
    let lines: Vec<&str> = trimmed.lines().collect();
    // First line is "---", skip it
    let mut yaml_lines = Vec::new();
    let mut body_start = 0;
    let mut found_closing = false;

    for (i, line) in lines.iter().enumerate().skip(1) {
        if line.trim() == "---" {
            body_start = i + 1;
            found_closing = true;
            break;
        }
        yaml_lines.push(*line);
    }

    if !found_closing {
        return Err(config::ConfigError::WorkflowParseError(
            "unclosed YAML front matter".into(),
        ));
    }

    let yaml_str = yaml_lines.join("\n").trim().to_string();
    let body = lines[body_start..].join("\n").trim().to_string();

    let config: serde_yaml::Value = serde_yaml::from_str(&yaml_str)
        .map_err(|e| config::ConfigError::WorkflowParseError(e.to_string()))?;

    // Validate front matter is a map
    if !config.is_mapping() && !config.is_null() {
        return Err(config::ConfigError::WorkflowFrontMatterNotAMap);
    }

    Ok(crate::types::WorkflowDefinition {
        config,
        prompt_template: body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_workflow_no_front_matter() {
        let wf = parse_workflow_content("Just a prompt body").unwrap();
        assert_eq!(wf.prompt_template, "Just a prompt body");
        assert!(wf.config.is_null());
    }

    #[test]
    fn test_parse_workflow_with_front_matter() {
        let wf = parse_workflow_content(
            r#"---
tracker:
  kind: linear
  project_slug: test
---

You are working on {{ issue.identifier }}
"#,
        )
        .unwrap();
        assert!(wf.config.is_mapping());
        assert!(wf.prompt_template.contains("{{ issue.identifier }}"));
    }

    #[test]
    fn test_parse_workflow_missing_front_matter_returns_error() {
        let result = parse_workflow_content(
            r#"---
incomplete front matter
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_workflow_non_map_front_matter() {
        let result = parse_workflow_content(
            r#"---
- list item
- not a map
---

body
"#,
        );
        assert!(result.is_err());
    }
}
