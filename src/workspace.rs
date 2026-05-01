//! Workspace manager — deterministic per-issue workspaces with lifecycle hooks.
//! Maps to SPEC.md §4.1.4 and §7 (workspace hooks).

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::types::{Workspace, sanitize_workspace_key};

/// Errors that can occur during workspace operations.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("failed to create workspace directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("after_create hook failed: {0}")]
    AfterCreateHook(String),
    #[error("before_run hook failed: {0}")]
    BeforeRunHook(String),
    #[error("before_remove hook failed: {0}")]
    BeforeRemoveHook(String),
    #[error("hook timed out after {0}ms")]
    HookTimeout(u64),
    #[error("hook failed: {0}")]
    HookFailed(String),
    #[error("workspace path is not a directory: {0}")]
    NotADirectory(PathBuf),
}

/// Workspace manager holding root path and hooks config.
#[derive(Debug, Clone)]
pub struct WorkspaceManager {
    pub root: PathBuf,
    pub after_create: Option<String>,
    pub before_run: Option<String>,
    pub after_run: Option<String>,
    pub before_remove: Option<String>,
    pub hook_timeout_ms: u64,
}

impl WorkspaceManager {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            after_create: None,
            before_run: None,
            after_run: None,
            before_remove: None,
            hook_timeout_ms: 60_000,
        }
    }

    /// Create (or reuse) a workspace for a given issue identifier.
    /// Returns a Workspace with `created_now` set to true only if the
    /// directory was freshly created.
    pub async fn create_for_issue(&self, identifier: &str) -> Result<Workspace, WorkspaceError> {
        let key = sanitize_workspace_key(identifier);
        let path = self.root.join(&key);

        let created_now = !path.exists();

        if created_now {
            std::fs::create_dir_all(&path).map_err(|e| WorkspaceError::CreateDir {
                path: path.clone(),
                source: e,
            })?;

            // Run after_create hook only on fresh creation
            if let Some(ref hook) = self.after_create {
                self.run_hook(hook, &path, identifier)
                    .await
                    .map_err(|e| match e {
                        WorkspaceError::HookTimeout(ms) => WorkspaceError::HookTimeout(ms),
                        other => WorkspaceError::AfterCreateHook(other.to_string()),
                    })?;
            }
        } else if path.exists() && !path.is_dir() {
            return Err(WorkspaceError::NotADirectory(path));
        }

        Ok(Workspace {
            path,
            workspace_key: key,
            created_now,
        })
    }

    /// Run the before_run hook. Failure aborts the current attempt.
    pub async fn run_before_run(
        &self,
        workspace_path: &Path,
        identifier: &str,
    ) -> Result<(), WorkspaceError> {
        if let Some(ref hook) = self.before_run {
            self.run_hook(hook, workspace_path, identifier)
                .await
                .map_err(|e| match e {
                    WorkspaceError::HookTimeout(ms) => WorkspaceError::HookTimeout(ms),
                    other => WorkspaceError::BeforeRunHook(other.to_string()),
                })?;
        }
        Ok(())
    }

    /// Run the after_run hook. Failure is logged but ignored.
    pub async fn run_after_run(&self, workspace_path: &Path, identifier: &str) {
        if let Some(ref hook) = self.after_run
            && let Err(e) = self.run_hook(hook, workspace_path, identifier).await
        {
            tracing::debug!(
                workspace = %workspace_path.display(),
                error = %e,
                "after_run hook failed (ignored)"
            );
        }
    }

    /// Run the before_remove hook. Failure aborts workspace removal.
    pub async fn run_before_remove(
        &self,
        workspace_path: &Path,
        identifier: &str,
    ) -> Result<(), WorkspaceError> {
        if let Some(ref hook) = self.before_remove
            && let Err(e) = self.run_hook(hook, workspace_path, identifier).await
        {
            return Err(match e {
                WorkspaceError::HookTimeout(ms) => WorkspaceError::HookTimeout(ms),
                other => WorkspaceError::BeforeRemoveHook(other.to_string()),
            });
        }
        Ok(())
    }

    /// Remove a workspace directory (with before_remove hook).
    pub async fn remove_workspace(&self, identifier: &str) {
        let key = sanitize_workspace_key(identifier);
        let path = self.root.join(&key);

        if path.exists() {
            if let Err(e) = self.run_before_remove(&path, identifier).await {
                tracing::error!(
                    workspace = %path.display(),
                    error = %e,
                    "before_remove hook failed, preserving workspace"
                );
                return;
            }

            if let Err(e) = std::fs::remove_dir_all(&path) {
                tracing::error!(
                    workspace = %path.display(),
                    error = %e,
                    "failed to remove workspace"
                );
            }
        }
    }

    /// Run a shell hook script with a timeout. Uses tokio::process so the
    /// hook process is killed when the timeout expires.
    async fn run_hook(
        &self,
        script: &str,
        workspace_path: &Path,
        identifier: &str,
    ) -> Result<(), WorkspaceError> {
        let timeout_ms = self.hook_timeout_ms;
        let workspace_key = workspace_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(identifier);

        let child = tokio::process::Command::new("bash")
            .arg("-lc")
            .arg(script)
            .current_dir(workspace_path)
            .env("SYMPHONY_WORKSPACE", workspace_path)
            .env("SYMPHONY_WORKSPACE_KEY", workspace_key)
            .env("SYMPHONY_ISSUE_IDENTIFIER", identifier)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| WorkspaceError::HookFailed(e.to_string()))?;

        let result =
            tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output()).await;

        match result {
            Ok(Ok(output)) => {
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr)
                        .chars()
                        .take(500)
                        .collect();
                    return Err(WorkspaceError::HookFailed(stderr));
                }
                Ok(())
            }
            Ok(Err(e)) => Err(WorkspaceError::HookFailed(e.to_string())),
            Err(_elapsed) => Err(WorkspaceError::HookTimeout(timeout_ms)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = WorkspaceManager::new(tmp.path().to_path_buf());

        let ws = mgr.create_for_issue("ABC-123").await.unwrap();
        assert!(ws.path.exists());
        assert!(ws.created_now);
        assert_eq!(ws.workspace_key, "ABC-123");

        // Re-create: should not be "created_now"
        let ws2 = mgr.create_for_issue("ABC-123").await.unwrap();
        assert!(!ws2.created_now);
    }

    #[tokio::test]
    async fn test_workspace_key_sanitization() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = WorkspaceManager::new(tmp.path().to_path_buf());

        let ws = mgr.create_for_issue("TEAM/ISSUE:1").await.unwrap();
        assert_eq!(ws.workspace_key, "TEAM_ISSUE_1");
        assert!(ws.path.exists());
    }

    #[tokio::test]
    async fn test_hooks_receive_workspace_environment() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mgr = WorkspaceManager::new(tmp.path().to_path_buf());
        mgr.after_create = Some(
            "printf '%s|%s|%s' \"$SYMPHONY_ISSUE_IDENTIFIER\" \"$SYMPHONY_WORKSPACE_KEY\" \"$SYMPHONY_WORKSPACE\" > hook-env.txt"
                .into(),
        );

        let ws = mgr.create_for_issue("TEAM/ISSUE:1").await.unwrap();
        let content = std::fs::read_to_string(ws.path.join("hook-env.txt")).unwrap();

        assert_eq!(
            content,
            format!("TEAM/ISSUE:1|TEAM_ISSUE_1|{}", ws.path.to_string_lossy())
        );
    }

    #[tokio::test]
    async fn test_remove_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = WorkspaceManager::new(tmp.path().to_path_buf());

        mgr.create_for_issue("T-99").await.unwrap();
        assert!(mgr.root.join("T-99").exists());

        mgr.remove_workspace("T-99").await;
        assert!(!mgr.root.join("T-99").exists());
    }

    #[tokio::test]
    async fn test_before_remove_hook_timeout_preserves_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mgr = WorkspaceManager::new(tmp.path().to_path_buf());
        mgr.before_remove = Some("sleep 2".into());
        mgr.hook_timeout_ms = 50;

        mgr.create_for_issue("T-100").await.unwrap();
        mgr.remove_workspace("T-100").await;

        assert!(mgr.root.join("T-100").exists());
    }

    #[tokio::test]
    async fn test_verbose_hook_does_not_block_on_piped_output() {
        let tmp = tempfile::tempdir().unwrap();
        let mut mgr = WorkspaceManager::new(tmp.path().to_path_buf());
        mgr.after_create = Some("for i in {1..20000}; do echo line-$i; done".into());
        mgr.hook_timeout_ms = 5_000;

        let ws = mgr.create_for_issue("T-101").await.unwrap();

        assert!(ws.path.exists());
    }
}
