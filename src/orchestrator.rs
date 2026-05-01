//! Orchestrator — polling loop, concurrency control, retry/backoff, reconciliation.
//! Maps to SPEC.md §7 and §8.
//! Supports both mock-agent and real Codex subprocess worker paths.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{RwLock, Semaphore};
use tokio::time::{MissedTickBehavior, interval};

use crate::config::ServiceConfig;
use crate::template;
use crate::tracker::TrackerClient;
use crate::types::{CancelHandle, Issue, LiveSession, OrchestratorState, RetryEntry, RunningEntry};
use crate::workspace::WorkspaceManager;

pub struct Orchestrator {
    pub state: Arc<RwLock<OrchestratorState>>,
    config: Arc<RwLock<ServiceConfig>>,
    tracker: Arc<dyn TrackerClient>,
    workspace_mgr: WorkspaceManager,
    semaphore: Arc<Semaphore>,
    workflow_path: std::path::PathBuf,
    /// Receiver for /api/v1/refresh triggers (None = not enabled).
    refresh_rx: Option<tokio::sync::watch::Receiver<bool>>,
}

/// Outcome reported by a worker task back to the orchestrator.
#[derive(Debug, Clone)]
pub enum WorkerOutcome {
    /// Worker completed normally — schedule continuation retry.
    Normal {
        tokens_in: u64,
        tokens_out: u64,
        elapsed_seconds: f64,
    },
    /// Worker failed — schedule backoff retry.
    Error {
        error: String,
        retry_attempt: Option<u32>,
        tokens_in: u64,
        tokens_out: u64,
        elapsed_seconds: f64,
    },
}

impl Orchestrator {
    pub fn new(
        config: ServiceConfig,
        tracker: Arc<dyn TrackerClient>,
        workspace_mgr: WorkspaceManager,
        workflow_path: std::path::PathBuf,
        refresh_rx: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> Self {
        let max_concurrent = config.agent_max_concurrent_agents as usize;
        let poll_interval = config.poll_interval_ms;
        let state = OrchestratorState {
            poll_interval_ms: poll_interval,
            max_concurrent_agents: config.agent_max_concurrent_agents,
            running: Default::default(),
            claimed: Default::default(),
            retry_attempts: Default::default(),
            completed: Default::default(),
            codex_totals: Default::default(),
            codex_rate_limits: None,
        };

        Self {
            state: Arc::new(RwLock::new(state)),
            config: Arc::new(RwLock::new(config)),
            tracker,
            workspace_mgr,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            workflow_path,
            refresh_rx,
        }
    }

    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.config
            .read()
            .await
            .validate_dispatch()
            .map_err(|e| format!("startup validation failed: {e}"))?;

        // Spawn retry timer dispatch task
        self.spawn_retry_dispatcher();

        // Spawn dynamic reload watcher
        self.spawn_workflow_watcher();

        self.startup_terminal_cleanup().await;

        let poll_ms = self.config.read().await.poll_interval_ms;
        let mut tick = interval(Duration::from_millis(poll_ms));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // Initial poll
        self.poll_and_dispatch().await;

        // Take the refresh receiver if available
        let mut refresh_rx = self.refresh_rx.clone();

        loop {
            if let Some(ref mut rx) = refresh_rx {
                tokio::select! {
                    _ = tick.tick() => {},
                    Ok(()) = rx.changed() => {
                        tracing::info!("refresh triggered via /api/v1/refresh");
                    }
                }
            } else {
                tick.tick().await;
            }

            let new_poll_ms = self.config.read().await.poll_interval_ms;
            let current_period = tick.period();
            if current_period.as_millis() as u64 != new_poll_ms {
                tick = interval(Duration::from_millis(new_poll_ms));
                tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            }

            self.poll_and_dispatch().await;
        }
    }

    async fn poll_and_dispatch(&self) {
        self.reconcile_running_issues().await;

        let config = self.config.read().await.clone();
        if let Err(e) = config.validate_dispatch() {
            tracing::warn!(error = %e, "dispatch validation failed, skipping tick");
            return;
        }

        let issues = match self.tracker.fetch_candidate_issues().await {
            Ok(issues) => issues,
            Err(e) => {
                tracing::error!(error = %e, "tracker fetch failed");
                return;
            }
        };

        tracing::info!(candidate_count = issues.len(), "fetched candidates");

        let mut sorted = issues;
        sorted.sort_by(|a, b| {
            a.priority
                .unwrap_or(i32::MAX)
                .cmp(&b.priority.unwrap_or(i32::MAX))
                .then_with(|| a.created_at.cmp(&b.created_at))
                .then_with(|| a.identifier.cmp(&b.identifier))
        });

        for issue in &sorted {
            if self.should_dispatch(issue, &config).await {
                self.dispatch_issue(issue.clone(), None).await;
            }
        }
    }

    async fn should_dispatch(&self, issue: &Issue, config: &ServiceConfig) -> bool {
        let state = self.state.read().await;

        if issue.id.is_empty()
            || issue.identifier.is_empty()
            || issue.title.is_empty()
            || issue.state.is_empty()
        {
            return false;
        }

        if !issue.is_active(&config.tracker_active_states) {
            return false;
        }

        if issue.is_terminal(&config.tracker_terminal_states) {
            return false;
        }

        if state.running.contains_key(&issue.id)
            || state.claimed.contains(&issue.id)
            || state.retry_attempts.contains_key(&issue.id)
        {
            return false;
        }

        // Blocker rule
        if issue.state == "Todo" {
            let any_non_terminal_blocker = issue.blocked_by.iter().any(|b| {
                b.state
                    .as_deref()
                    .map(|s| {
                        !config
                            .tracker_terminal_states
                            .iter()
                            .any(|ts| ts.to_lowercase() == s.to_lowercase())
                    })
                    .unwrap_or(true)
            });
            if any_non_terminal_blocker {
                tracing::debug!(
                    issue_id = %issue.id,
                    identifier = %issue.identifier,
                    "blocked by non-terminal issue, skipping"
                );
                return false;
            }
        }

        // Global concurrency
        if state.available_slots() == 0 {
            return false;
        }

        // Per-state concurrency
        let state_norm = issue.state.to_lowercase();
        if let Some(per_state_max) = config.agent_max_concurrent_agents_by_state.get(&state_norm) {
            let count_in_state = state
                .running
                .values()
                .filter(|e| e.issue.state.to_lowercase() == state_norm)
                .count();
            if count_in_state >= *per_state_max as usize {
                return false;
            }
        }

        true
    }

    // ── Dispatch ──────────────────────────────────────────────────────────

    async fn dispatch_issue(&self, issue: Issue, attempt: Option<u32>) {
        let issue_id = issue.id.clone();
        let identifier = issue.identifier.clone();

        {
            let mut state = self.state.write().await;
            state.claimed.insert(issue_id.clone());
            state.retry_attempts.remove(&issue_id);
        }

        let permit = match self.semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                self.schedule_retry(
                    &issue_id,
                    &identifier,
                    attempt.unwrap_or(1),
                    "no available slots",
                )
                .await;
                return;
            }
        };

        let config_snapshot = self.config.read().await.clone();
        if let Some(claim_state) = config_snapshot.agent_claim_state.as_deref()
            && !issue.state.eq_ignore_ascii_case(claim_state)
        {
            match self
                .tracker
                .transition_issue_state(&issue_id, claim_state)
                .await
            {
                Ok(()) => tracing::info!(
                    issue_id = %issue_id,
                    identifier = %identifier,
                    state = %claim_state,
                    "issue transitioned after claim"
                ),
                Err(e) => {
                    tracing::error!(
                        issue_id = %issue_id,
                        identifier = %identifier,
                        state = %claim_state,
                        error = %e,
                        "failed to transition issue after claim"
                    );
                    self.schedule_retry(
                        &issue_id,
                        &identifier,
                        attempt.map_or(1, |a| a + 1),
                        &format!("claim transition: {e}"),
                    )
                    .await;
                    drop(permit);
                    return;
                }
            }
        }

        // Create workspace
        let workspace = match self.workspace_mgr.create_for_issue(&identifier).await {
            Ok(ws) => ws,
            Err(e) => {
                tracing::error!(
                    issue_id = %issue_id,
                    identifier = %identifier,
                    error = %e,
                    "workspace creation failed"
                );
                self.schedule_retry(
                    &issue_id,
                    &identifier,
                    attempt.map_or(1, |a| a + 1),
                    &format!("workspace error: {e}"),
                )
                .await;
                drop(permit);
                return;
            }
        };

        tracing::info!(
            issue_id = %issue_id,
            identifier = %identifier,
            workspace = %workspace.path.display(),
            created_now = workspace.created_now,
            "workspace ready"
        );

        // Run before_run hook
        if let Err(e) = self
            .workspace_mgr
            .run_before_run(&workspace.path, &identifier)
            .await
        {
            tracing::error!(issue_id = %issue_id, error = %e, "before_run hook failed");
            self.schedule_retry(
                &issue_id,
                &identifier,
                attempt.map_or(1, |a| a + 1),
                &format!("before_run hook: {e}"),
            )
            .await;
            drop(permit);
            return;
        }

        let started_at = chrono::Utc::now();
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let entry = RunningEntry {
            issue_id: issue_id.clone(),
            identifier: identifier.clone(),
            issue: issue.clone(),
            session: LiveSession::default(),
            retry_attempt: attempt,
            started_at,
            cancelled: cancelled.clone(),
        };

        {
            let mut state = self.state.write().await;
            state.running.insert(issue_id.clone(), entry);
        }

        // Build prompt
        let prompt_template = config_snapshot.prompt_template.clone();
        let rendering = if prompt_template.is_empty() {
            format!(
                "You are working on issue {}: {}.",
                issue.identifier, issue.title
            )
        } else {
            match template::render_prompt(&prompt_template, &issue, attempt) {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(issue_id = %issue_id, error = %e, "prompt render failed");
                    self.handle_worker_exit(&issue_id, "prompt error", true)
                        .await;
                    drop(permit);
                    return;
                }
            }
        };

        // Gather shared resources for the worker task
        let state_arc = self.state.clone();
        let wm = self.workspace_mgr.clone();
        let tracker = self.tracker.clone();
        let config = config_snapshot;
        let ws_path = workspace.path.clone();

        tokio::spawn(async move {
            let _permit = permit;
            let max_retry_backoff_ms = config.agent_max_retry_backoff_ms;
            let mock_mode = config.mock_agent;
            let completion_state = config.agent_completion_state.clone();
            let tracker_for_completion = tracker.clone();
            let outcome = run_worker(WorkerContext {
                config,
                issue_id: issue_id.clone(),
                identifier: identifier.clone(),
                state: state_arc.clone(),
                workspace_path: ws_path.clone(),
                prompt: rendering,
                attempt,
                mock_mode,
                tracker,
                cancelled: cancelled.clone(),
            })
            .await;

            if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                tracing::info!(issue_id = %issue_id, "worker cancelled, skipping outcome reporting");
                return;
            }

            // Run after_run hook
            wm.run_after_run(&ws_path, &identifier).await;

            if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                tracing::info!(issue_id = %issue_id, "worker cancelled after hooks, skipping outcome reporting");
                return;
            }

            if let WorkerOutcome::Normal { .. } = &outcome
                && let Some(state_name) = completion_state.as_deref()
            {
                match tracker_for_completion
                    .transition_issue_state(&issue_id, state_name)
                    .await
                {
                    Ok(()) => tracing::info!(
                        issue_id = %issue_id,
                        identifier = %identifier,
                        state = %state_name,
                        "issue transitioned after successful worker run"
                    ),
                    Err(e) => tracing::error!(
                        issue_id = %issue_id,
                        identifier = %identifier,
                        state = %state_name,
                        error = %e,
                        "failed to transition issue after successful worker run"
                    ),
                }
            }

            // Report outcome to orchestrator state
            let mut state = state_arc.write().await;
            state.running.remove(&issue_id);
            state.claimed.remove(&issue_id);

            match outcome {
                WorkerOutcome::Normal {
                    tokens_in,
                    tokens_out,
                    elapsed_seconds,
                    ..
                } => {
                    state.codex_totals.input_tokens += tokens_in;
                    state.codex_totals.output_tokens += tokens_out;
                    state.codex_totals.total_tokens += tokens_in + tokens_out;
                    state.codex_totals.seconds_running += elapsed_seconds;
                    state.completed.insert(issue_id.clone());

                    // Continuation retry per spec §7.4.
                    state.retry_attempts.insert(
                        issue_id.clone(),
                        RetryEntry {
                            issue_id: issue_id.clone(),
                            identifier: identifier.clone(),
                            attempt: 1,
                            due_at_ms: 1000, // 1 second
                            error: None,
                        },
                    );

                    tracing::info!(
                        issue_id = %issue_id,
                        identifier = %identifier,
                        "worker completed, continuation retry scheduled"
                    );
                }
                WorkerOutcome::Error {
                    error,
                    retry_attempt,
                    tokens_in,
                    tokens_out,
                    elapsed_seconds,
                    ..
                } => {
                    state.codex_totals.input_tokens += tokens_in;
                    state.codex_totals.output_tokens += tokens_out;
                    state.codex_totals.total_tokens += tokens_in + tokens_out;
                    state.codex_totals.seconds_running += elapsed_seconds;

                    let next = retry_attempt.unwrap_or(0) + 1;
                    let backoff_ms = compute_backoff(next, max_retry_backoff_ms);

                    state.retry_attempts.insert(
                        issue_id.clone(),
                        RetryEntry {
                            issue_id: issue_id.clone(),
                            identifier: identifier.clone(),
                            attempt: next,
                            due_at_ms: backoff_ms,
                            error: Some(error.clone()),
                        },
                    );

                    tracing::debug!(
                        issue_id = %issue_id,
                        identifier = %identifier,
                        attempt = next,
                        backoff_ms = backoff_ms,
                        error = %error,
                        "worker failed, retry scheduled"
                    );
                }
            }
        });
    }

    // ── Retry dispatcher ──────────────────────────────────────────────────

    fn spawn_retry_dispatcher(&self) {
        let state = self.state.clone();
        let _orch_state = self.state.clone();
        let config = self.config.clone();
        let tracker = self.tracker.clone();
        let semaphore = self.semaphore.clone();
        let workspace_mgr = self.workspace_mgr.clone();

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;

                let due: Vec<RetryEntry> = {
                    let mut s = state.write().await;
                    let mut ready = Vec::new();
                    s.retry_attempts.retain(|_id, entry| {
                        if entry.due_at_ms <= 1000 {
                            ready.push(entry.clone());
                            false
                        } else {
                            entry.due_at_ms = entry.due_at_ms.saturating_sub(1000);
                            true
                        }
                    });
                    for entry in &ready {
                        s.claimed.insert(entry.issue_id.clone());
                    }
                    ready
                };

                for retry in due {
                    let issue_id = retry.issue_id.clone();

                    // Continuation retries (error=None): just clean up terminal issues,
                    // do NOT re-dispatch. The next poll tick handles re-dispatch.
                    if retry.error.is_none() {
                        let refreshed = match tracker
                            .fetch_issue_states_by_ids(std::slice::from_ref(&issue_id))
                            .await
                        {
                            Ok(issues) => issues,
                            Err(_) => {
                                // Can't reach tracker, release claim so poll tick tries fresh
                                let mut s = state.write().await;
                                s.claimed.remove(&issue_id);
                                continue;
                            }
                        };

                        if let Some(refreshed_issue) = refreshed.first() {
                            let terminal = {
                                let cfg = config.read().await;
                                refreshed_issue.is_terminal(&cfg.tracker_terminal_states)
                            };
                            if terminal {
                                {
                                    let mut s = state.write().await;
                                    s.claimed.remove(&issue_id);
                                }
                                workspace_mgr
                                    .remove_workspace(&refreshed_issue.identifier)
                                    .await;
                                tracing::info!(
                                    issue_id = %issue_id,
                                    identifier = %refreshed_issue.identifier,
                                    "continuation check: issue terminal, cleaned workspace"
                                );
                            } else {
                                // Still active, release claim for next poll tick
                                let mut s = state.write().await;
                                s.claimed.remove(&issue_id);
                                tracing::debug!(
                                    issue_id = %issue_id,
                                    "continuation check: issue still active, releasing for poll"
                                );
                            }
                        } else {
                            let mut s = state.write().await;
                            s.claimed.remove(&issue_id);
                        }
                        continue;
                    }

                    // Backoff retries (error is Some): re-dispatch with backoff
                    let candidates = match tracker.fetch_candidate_issues().await {
                        Ok(issues) => issues,
                        Err(e) => {
                            tracing::error!(error = %e, "retry poll failed");
                            let mut s = state.write().await;
                            s.retry_attempts.insert(
                                issue_id.clone(),
                                RetryEntry {
                                    due_at_ms: 30_000,
                                    error: Some("retry poll failed".into()),
                                    ..retry.clone()
                                },
                            );
                            continue;
                        }
                    };

                    let issue = match candidates.iter().find(|i| i.id == issue_id) {
                        Some(i) => i.clone(),
                        None => {
                            let mut s = state.write().await;
                            s.claimed.remove(&issue_id);
                            tracing::info!(
                                issue_id = %issue_id,
                                "retry: issue no longer in candidates, releasing claim"
                            );
                            continue;
                        }
                    };

                    // Check if already running under a single write lock (no TOCTOU)
                    {
                        let mut s = state.write().await;
                        if s.running.contains_key(&issue_id) {
                            // Reinsert retry with short delay — do not drop it.
                            s.retry_attempts.insert(
                                issue_id.clone(),
                                RetryEntry {
                                    due_at_ms: 5_000,
                                    ..retry.clone()
                                },
                            );
                            continue;
                        }
                    }

                    let permit = match semaphore.clone().try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => {
                            let mut s = state.write().await;
                            s.retry_attempts.insert(
                                issue_id.clone(),
                                RetryEntry {
                                    due_at_ms: 30_000,
                                    error: Some("no available slots".into()),
                                    ..retry
                                },
                            );
                            continue;
                        }
                    };

                    // Dispatch via the same worker path
                    let cfg_snapshot = config.read().await.clone();
                    if let Some(claim_state) = cfg_snapshot.agent_claim_state.as_deref()
                        && !issue.state.eq_ignore_ascii_case(claim_state)
                    {
                        match tracker.transition_issue_state(&issue_id, claim_state).await {
                            Ok(()) => tracing::info!(
                                issue_id = %issue_id,
                                identifier = %issue.identifier,
                                state = %claim_state,
                                "retry issue transitioned after claim"
                            ),
                            Err(e) => {
                                let mut s = state.write().await;
                                s.retry_attempts.insert(
                                    issue_id.clone(),
                                    RetryEntry {
                                        attempt: retry.attempt,
                                        due_at_ms: 30_000,
                                        error: Some(format!("claim transition: {e}")),
                                        ..retry
                                    },
                                );
                                drop(permit);
                                continue;
                            }
                        }
                    }

                    let prompt_template = cfg_snapshot.prompt_template.clone();
                    let wm = workspace_mgr.clone();
                    let ws = match wm.create_for_issue(&issue.identifier).await {
                        Ok(ws) => ws,
                        Err(e) => {
                            let mut s = state.write().await;
                            s.retry_attempts.insert(
                                issue_id.clone(),
                                RetryEntry {
                                    attempt: retry.attempt,
                                    due_at_ms: 30_000,
                                    error: Some(format!("workspace error: {e}")),
                                    ..retry
                                },
                            );
                            drop(permit);
                            continue;
                        }
                    };

                    if let Err(e) = wm.run_before_run(&ws.path, &issue.identifier).await {
                        let mut s = state.write().await;
                        s.retry_attempts.insert(
                            issue_id.clone(),
                            RetryEntry {
                                attempt: retry.attempt,
                                due_at_ms: 30_000,
                                error: Some(format!("before_run hook: {e}")),
                                ..retry
                            },
                        );
                        drop(permit);
                        continue;
                    }

                    let rendering = if prompt_template.is_empty() {
                        format!(
                            "You are working on issue {}: {}.",
                            issue.identifier, issue.title
                        )
                    } else {
                        match template::render_prompt(&prompt_template, &issue, Some(retry.attempt))
                        {
                            Ok(p) => p,
                            Err(e) => {
                                let mut s = state.write().await;
                                s.retry_attempts.insert(
                                    issue_id.clone(),
                                    RetryEntry {
                                        attempt: retry.attempt,
                                        due_at_ms: 30_000,
                                        error: Some(format!("prompt render: {e}")),
                                        ..retry
                                    },
                                );
                                drop(permit);
                                continue;
                            }
                        }
                    };

                    let state_arc = state.clone();
                    let tracker_clone = tracker.clone();
                    let wm_clone = wm.clone();
                    let cfg = cfg_snapshot;
                    let ws_path = ws.path.clone();
                    let id = issue_id.clone();
                    let ident = issue.identifier.clone();
                    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

                    {
                        let mut s = state.write().await;
                        s.claimed.insert(id.clone());
                        s.running.insert(
                            id.clone(),
                            RunningEntry {
                                issue_id: id.clone(),
                                identifier: ident.clone(),
                                issue: issue.clone(),
                                session: LiveSession::default(),
                                retry_attempt: Some(retry.attempt),
                                started_at: chrono::Utc::now(),
                                cancelled: cancelled.clone(),
                            },
                        );
                    }

                    tokio::spawn(async move {
                        let _permit = permit;
                        let max_retry_backoff_ms = cfg.agent_max_retry_backoff_ms;
                        let mock_mode = cfg.mock_agent;
                        let completion_state = cfg.agent_completion_state.clone();
                        let tracker_for_completion = tracker_clone.clone();
                        let outcome = run_worker(WorkerContext {
                            config: cfg,
                            issue_id: id.clone(),
                            identifier: ident.clone(),
                            state: state_arc.clone(),
                            workspace_path: ws_path.clone(),
                            prompt: rendering,
                            attempt: Some(retry.attempt),
                            mock_mode,
                            tracker: tracker_clone,
                            cancelled: cancelled.clone(),
                        })
                        .await;

                        if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                            tracing::info!(issue_id = %id, "retry worker cancelled, skipping outcome reporting");
                            return;
                        }

                        wm_clone.run_after_run(&ws_path, &ident).await;

                        if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                            tracing::info!(issue_id = %id, "retry worker cancelled after hooks, skipping outcome reporting");
                            return;
                        }

                        if let WorkerOutcome::Normal { .. } = &outcome
                            && let Some(state_name) = completion_state.as_deref()
                        {
                            match tracker_for_completion
                                .transition_issue_state(&id, state_name)
                                .await
                            {
                                Ok(()) => tracing::info!(
                                    issue_id = %id,
                                    identifier = %ident,
                                    state = %state_name,
                                    "issue transitioned after successful retry worker run"
                                ),
                                Err(e) => tracing::error!(
                                    issue_id = %id,
                                    identifier = %ident,
                                    state = %state_name,
                                    error = %e,
                                    "failed to transition issue after successful retry worker run"
                                ),
                            }
                        }

                        let mut s = state_arc.write().await;
                        s.running.remove(&id);
                        s.claimed.remove(&id);

                        match outcome {
                            WorkerOutcome::Normal {
                                tokens_in,
                                tokens_out,
                                elapsed_seconds,
                                ..
                            } => {
                                s.codex_totals.input_tokens += tokens_in;
                                s.codex_totals.output_tokens += tokens_out;
                                s.codex_totals.total_tokens += tokens_in + tokens_out;
                                s.codex_totals.seconds_running += elapsed_seconds;
                                s.completed.insert(id.clone());
                                s.retry_attempts.insert(
                                    id.clone(),
                                    RetryEntry {
                                        issue_id: id.clone(),
                                        identifier: ident.clone(),
                                        attempt: 1,
                                        due_at_ms: 1000,
                                        error: None,
                                    },
                                );
                            }
                            WorkerOutcome::Error {
                                error,
                                retry_attempt,
                                tokens_in,
                                tokens_out,
                                elapsed_seconds,
                                ..
                            } => {
                                s.codex_totals.input_tokens += tokens_in;
                                s.codex_totals.output_tokens += tokens_out;
                                s.codex_totals.total_tokens += tokens_in + tokens_out;
                                s.codex_totals.seconds_running += elapsed_seconds;
                                let next = retry_attempt.unwrap_or(retry.attempt) + 1;
                                s.retry_attempts.insert(
                                    id.clone(),
                                    RetryEntry {
                                        issue_id: id.clone(),
                                        identifier: ident.clone(),
                                        attempt: next,
                                        due_at_ms: compute_backoff(next, max_retry_backoff_ms),
                                        error: Some(error),
                                    },
                                );
                            }
                        }
                    });
                }
            }
        });
    }

    // ── Reconciliation ────────────────────────────────────────────────────

    async fn reconcile_running_issues(&self) {
        let running_ids: Vec<String> = {
            let state = self.state.read().await;
            state.running.keys().cloned().collect()
        };

        if running_ids.is_empty() {
            return;
        }

        let refreshed = match self.tracker.fetch_issue_states_by_ids(&running_ids).await {
            Ok(issues) => issues,
            Err(e) => {
                tracing::debug!(error = %e, "reconciliation fetch failed, keeping workers");
                return;
            }
        };

        let config = self.config.read().await;

        for issue in &refreshed {
            let should_cancel = issue.is_terminal(&config.tracker_terminal_states)
                || !issue.is_active(&config.tracker_active_states);

            if should_cancel {
                // Signal cancellation to the live worker before state cleanup
                {
                    let mut state = self.state.write().await;
                    if let Some(entry) = state.running.get_mut(&issue.id) {
                        entry
                            .cancelled
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }

                if issue.is_terminal(&config.tracker_terminal_states) {
                    tracing::info!(
                        issue_id = %issue.id,
                        state = %issue.state,
                        "cancelling run (terminal state), cleaning workspace"
                    );
                    self.handle_worker_exit(&issue.id, "terminal state", false)
                        .await;
                    self.workspace_mgr.remove_workspace(&issue.identifier).await;
                } else {
                    tracing::info!(
                        issue_id = %issue.id,
                        state = %issue.state,
                        "cancelling run (non-active state)"
                    );
                    self.handle_worker_exit(&issue.id, "non-active state", false)
                        .await;
                }
            }
        }
    }

    async fn handle_worker_exit(&self, issue_id: &str, reason: &str, schedule_retry: bool) {
        let max_backoff = if schedule_retry {
            self.config.read().await.agent_max_retry_backoff_ms
        } else {
            0
        };
        let mut state = self.state.write().await;

        if let Some(entry) = state.running.remove(issue_id) {
            state.claimed.remove(issue_id);

            state.codex_totals.input_tokens += entry.session.codex_input_tokens;
            state.codex_totals.output_tokens += entry.session.codex_output_tokens;
            state.codex_totals.total_tokens += entry.session.codex_total_tokens;

            let duration = chrono::Utc::now()
                .signed_duration_since(entry.started_at)
                .num_seconds() as f64;
            state.codex_totals.seconds_running += duration;

            if schedule_retry {
                let next_attempt = entry.retry_attempt.unwrap_or(0) + 1;
                let backoff_ms = compute_backoff(next_attempt, max_backoff);

                state.retry_attempts.insert(
                    issue_id.to_string(),
                    RetryEntry {
                        issue_id: issue_id.to_string(),
                        identifier: entry.identifier.clone(),
                        attempt: next_attempt,
                        due_at_ms: backoff_ms,
                        error: Some(reason.to_string()),
                    },
                );
            }
        }
    }

    async fn schedule_retry(&self, issue_id: &str, identifier: &str, attempt: u32, error: &str) {
        let max_backoff = self.config.read().await.agent_max_retry_backoff_ms;
        let backoff_ms = compute_backoff(attempt, max_backoff);
        let mut state = self.state.write().await;
        state.retry_attempts.insert(
            issue_id.to_string(),
            RetryEntry {
                issue_id: issue_id.to_string(),
                identifier: identifier.to_string(),
                attempt,
                due_at_ms: backoff_ms,
                error: Some(error.to_string()),
            },
        );
        state.claimed.remove(issue_id);
    }

    async fn startup_terminal_cleanup(&self) {
        match self.tracker.fetch_terminal_issues().await {
            Ok(issues) => {
                for issue in &issues {
                    tracing::info!(
                        identifier = %issue.identifier,
                        "startup: cleaning terminal workspace"
                    );
                    self.workspace_mgr.remove_workspace(&issue.identifier).await;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "startup terminal cleanup failed");
            }
        }
    }

    // ── Dynamic reload ────────────────────────────────────────────────────

    fn spawn_workflow_watcher(&self) {
        let config = self.config.clone();
        let prompt_template = self.config.clone();
        let workflow_path = self.workflow_path.clone();

        tokio::spawn(async move {
            use notify::EventKind;
            use notify::RecursiveMode;
            use notify::Watcher;

            let (tx, mut rx) = tokio::sync::mpsc::channel(16);

            let mut watcher = match notify::recommended_watcher(move |res| {
                if let Ok(event) = res {
                    let _ = tx.blocking_send(event);
                }
            }) {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!(error = %e, "could not create file watcher");
                    return;
                }
            };

            if watcher
                .watch(&workflow_path, RecursiveMode::NonRecursive)
                .is_err()
            {
                return;
            }

            loop {
                let Some(event) = rx.recv().await else {
                    break;
                };

                if event.kind
                    != EventKind::Modify(notify::event::ModifyKind::Data(
                        notify::event::DataChange::Any,
                    ))
                {
                    continue;
                }

                tracing::info!(path = %workflow_path.display(), "WORKFLOW.md changed, reloading");

                // Read and re-parse
                let content = match std::fs::read_to_string(&workflow_path) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to read workflow file on reload");
                        continue;
                    }
                };

                let workflow = match crate::parse_workflow_content(&content) {
                    Ok(wf) => wf,
                    Err(e) => {
                        tracing::error!(error = %e, "invalid WORKFLOW.md reload, keeping last known good config");
                        continue;
                    }
                };

                let workflow_dir = workflow_path.parent().unwrap_or(std::path::Path::new("."));
                let new_config = match ServiceConfig::from_yaml(&workflow.config, workflow_dir) {
                    Ok(mut c) => {
                        c.prompt_template = workflow.prompt_template;
                        c
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "invalid config reload, keeping last known good config");
                        continue;
                    }
                };

                tracing::info!(
                    poll_interval_ms = new_config.poll_interval_ms,
                    max_concurrent = new_config.agent_max_concurrent_agents,
                    "config reloaded from WORKFLOW.md"
                );

                *config.write().await = new_config.clone();
                *prompt_template.write().await = new_config;
            }
        });
    }
}

// ── Worker (runs in a spawned task) ─────────────────────────────────────────

struct WorkerContext {
    config: ServiceConfig,
    issue_id: String,
    identifier: String,
    state: Arc<RwLock<OrchestratorState>>,
    workspace_path: std::path::PathBuf,
    prompt: String,
    attempt: Option<u32>,
    mock_mode: bool,
    tracker: Arc<dyn TrackerClient>,
    cancelled: CancelHandle,
}

async fn run_worker(ctx: WorkerContext) -> WorkerOutcome {
    let start = tokio::time::Instant::now();
    let mut total_tokens_in: u64 = 0;
    let mut total_tokens_out: u64 = 0;
    let running_state = ctx.state.clone();
    let running_issue_id = ctx.issue_id.clone();

    if ctx.mock_mode {
        // Mock agent: simulate a session
        tracing::info!(
            issue_id = %ctx.issue_id,
            identifier = %ctx.identifier,
            workspace = %ctx.workspace_path.display(),
            "worker started (mock mode)"
        );

        crate::agent_runner::run_mock_agent(&ctx.workspace_path, &ctx.issue_id)
            .await
            .ok();

        if ctx.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            return WorkerOutcome::Error {
                error: "cancelled by reconciliation".into(),
                retry_attempt: ctx.attempt,
                tokens_in: 0,
                tokens_out: 0,
                elapsed_seconds: start.elapsed().as_secs_f64(),
            };
        }

        // Simulate token usage
        total_tokens_in = 500;
        total_tokens_out = 1200;

        return WorkerOutcome::Normal {
            tokens_in: total_tokens_in,
            tokens_out: total_tokens_out,
            elapsed_seconds: start.elapsed().as_secs_f64(),
        };
    }

    // Real agent path: continuation turn loop per spec §7.1
    let max_turns = ctx.config.agent_max_turns;
    let mut turn_number: u32 = 1;
    let agent_cfg = crate::agent_runner::AgentRunnerConfig {
        command: ctx.config.codex_command.clone(),
        workspace_path: ctx.workspace_path.clone(),
        turn_timeout_ms: ctx.config.codex_turn_timeout_ms,
        read_timeout_ms: ctx.config.codex_read_timeout_ms,
        stall_timeout_ms: ctx.config.codex_stall_timeout_ms,
        max_turns,
        stop_after_first_turn: ctx.config.agent_completion_state.is_some(),
        on_session_update: Some(Arc::new(move |session| {
            let state = running_state.clone();
            let issue_id = running_issue_id.clone();
            tokio::spawn(async move {
                let mut state = state.write().await;
                if let Some(entry) = state.running.get_mut(&issue_id) {
                    entry.session = session;
                }
            });
        })),
        cancelled: ctx.cancelled.clone(),
    };

    loop {
        if ctx.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            return WorkerOutcome::Error {
                error: "cancelled by reconciliation".into(),
                retry_attempt: ctx.attempt,
                tokens_in: total_tokens_in,
                tokens_out: total_tokens_out,
                elapsed_seconds: start.elapsed().as_secs_f64(),
            };
        }

        // Build prompt for this turn
        let turn_prompt = if turn_number == 1 {
            ctx.prompt.clone()
        } else {
            format!(
                "Continue working on the task. Turn {turn}/{max}.",
                turn = turn_number,
                max = max_turns
            )
        };

        // Run the agent attempt
        let issue = Issue {
            id: ctx.issue_id.clone(),
            identifier: ctx.identifier.clone(),
            title: String::new(),
            description: None,
            priority: None,
            state: String::new(),
            branch_name: None,
            url: None,
            labels: vec![],
            blocked_by: vec![],
            created_at: None,
            updated_at: None,
        };

        let outcome = crate::agent_runner::run_agent_attempt(
            agent_cfg.clone(),
            issue.clone(),
            if turn_number == 1 { ctx.attempt } else { None },
            turn_prompt,
            |t, m| format!("Continue working on the task. Turn {t}/{m}.", t = t, m = m),
        )
        .await;

        match outcome {
            crate::agent_runner::AgentOutcome::Normal { entry, total_turns } => {
                total_tokens_in += entry.session.codex_input_tokens;
                total_tokens_out += entry.session.codex_output_tokens;

                turn_number = total_turns + 1;

                // Re-check issue state before continuing
                let refreshed = ctx
                    .tracker
                    .fetch_issue_states_by_ids(std::slice::from_ref(&ctx.issue_id))
                    .await
                    .ok();

                let is_active = refreshed
                    .as_ref()
                    .and_then(|issues| issues.first())
                    .map(|i| i.is_active(&ctx.config.tracker_active_states))
                    .unwrap_or(true); // if refresh fails, assume active

                if !is_active {
                    tracing::info!(
                        issue_id = %ctx.issue_id,
                        "issue no longer active, stopping turns"
                    );
                    return WorkerOutcome::Normal {
                        tokens_in: total_tokens_in,
                        tokens_out: total_tokens_out,
                        elapsed_seconds: start.elapsed().as_secs_f64(),
                    };
                }

                if turn_number > max_turns {
                    tracing::info!(issue_id = %ctx.issue_id, "max turns reached");
                    return WorkerOutcome::Normal {
                        tokens_in: total_tokens_in,
                        tokens_out: total_tokens_out,
                        elapsed_seconds: start.elapsed().as_secs_f64(),
                    };
                }
                // Continue loop for next turn
            }
            crate::agent_runner::AgentOutcome::Error { error, entry } => {
                total_tokens_in += entry.session.codex_input_tokens;
                total_tokens_out += entry.session.codex_output_tokens;
                return WorkerOutcome::Error {
                    error,
                    retry_attempt: ctx.attempt,
                    tokens_in: total_tokens_in,
                    tokens_out: total_tokens_out,
                    elapsed_seconds: start.elapsed().as_secs_f64(),
                };
            }
            crate::agent_runner::AgentOutcome::Cancelled { reason, entry } => {
                total_tokens_in += entry.session.codex_input_tokens;
                total_tokens_out += entry.session.codex_output_tokens;
                return WorkerOutcome::Error {
                    error: reason,
                    retry_attempt: ctx.attempt,
                    tokens_in: total_tokens_in,
                    tokens_out: total_tokens_out,
                    elapsed_seconds: start.elapsed().as_secs_f64(),
                };
            }
        }
    }
}

/// Compute exponential backoff in milliseconds.
pub fn compute_backoff(attempt: u32, max_backoff_ms: u64) -> u64 {
    let base: u64 = 10_000;
    let exp = base.saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1)));
    exp.min(max_backoff_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EmptyTracker;

    #[async_trait::async_trait]
    impl TrackerClient for EmptyTracker {
        async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, crate::tracker::TrackerError> {
            Ok(Vec::new())
        }

        async fn fetch_issue_states_by_ids(
            &self,
            _ids: &[String],
        ) -> Result<Vec<Issue>, crate::tracker::TrackerError> {
            Ok(Vec::new())
        }

        async fn fetch_terminal_issues(&self) -> Result<Vec<Issue>, crate::tracker::TrackerError> {
            Ok(Vec::new())
        }

        async fn transition_issue_state(
            &self,
            _issue_id: &str,
            _state_name: &str,
        ) -> Result<(), crate::tracker::TrackerError> {
            Ok(())
        }
    }

    fn sample_issue() -> Issue {
        Issue {
            id: "issue-1".into(),
            identifier: "T-1".into(),
            title: "Retry me".into(),
            description: None,
            priority: Some(1),
            state: "Todo".into(),
            branch_name: None,
            url: None,
            labels: Vec::new(),
            blocked_by: Vec::new(),
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn test_compute_backoff() {
        assert_eq!(compute_backoff(1, 300_000), 10_000);
        assert_eq!(compute_backoff(2, 300_000), 20_000);
        assert_eq!(compute_backoff(3, 300_000), 40_000);
        assert_eq!(compute_backoff(5, 300_000), 160_000);
        assert_eq!(compute_backoff(10, 300_000), 300_000);
    }

    #[tokio::test]
    async fn test_should_dispatch_blocks_retrying_issue() {
        let tmp = tempfile::tempdir().unwrap();
        let config = ServiceConfig {
            workspace_root: tmp.path().to_string_lossy().into_owned(),
            mock_mode: true,
            mock_agent: true,
            ..ServiceConfig::default()
        };
        let workspace_mgr = WorkspaceManager::new(tmp.path().to_path_buf());
        let orchestrator = Orchestrator::new(
            config.clone(),
            Arc::new(EmptyTracker),
            workspace_mgr,
            tmp.path().join("WORKFLOW.md"),
            None,
        );
        let issue = sample_issue();

        orchestrator.state.write().await.retry_attempts.insert(
            issue.id.clone(),
            RetryEntry {
                issue_id: issue.id.clone(),
                identifier: issue.identifier.clone(),
                attempt: 1,
                due_at_ms: 10_000,
                error: Some("backoff".into()),
            },
        );

        assert!(!orchestrator.should_dispatch(&issue, &config).await);
    }
}
