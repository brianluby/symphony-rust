//! Issue tracker client — Linear adapter and mock tracker.
//! Maps to SPEC.md §11.

use crate::types::Issue;

/// Errors from tracker operations.
#[derive(Debug, thiserror::Error)]
pub enum TrackerError {
    #[error("HTTP request failed: {0}")]
    HttpError(#[from] reqwest::Error),
    #[error("HTTP request failed: status={status} body={body}")]
    HttpStatus {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("GraphQL error: {0}")]
    GraphQLError(String),
    #[error("malformed payload: {0}")]
    MalformedPayload(String),
    #[error("not authenticated")]
    NotAuthenticated,
}

/// Abstract tracker client trait — allows mock implementations for testing.
#[async_trait::async_trait]
pub trait TrackerClient: Send + Sync {
    /// Fetch candidate issues in active states for a project.
    async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, TrackerError>;

    /// Fetch current states for specific issue IDs (reconciliation).
    async fn fetch_issue_states_by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, TrackerError>;

    /// Fetch terminal-state issues for startup cleanup.
    async fn fetch_terminal_issues(&self) -> Result<Vec<Issue>, TrackerError>;

    /// Transition an issue to a workflow state by state name.
    async fn transition_issue_state(
        &self,
        issue_id: &str,
        state_name: &str,
    ) -> Result<(), TrackerError>;
}

// ── Linear Client (real) ────────────────────────────────────────────────────

pub struct LinearClient {
    endpoint: String,
    api_key: Option<String>,
    project_slug: String,
    active_states: Vec<String>,
    terminal_states: Vec<String>,
    client: reqwest::Client,
}

impl LinearClient {
    pub fn new(
        endpoint: String,
        api_key: Option<String>,
        project_slug: String,
        active_states: Vec<String>,
        terminal_states: Vec<String>,
    ) -> Self {
        Self {
            endpoint,
            api_key,
            project_slug,
            active_states,
            terminal_states,
            client: reqwest::Client::new(),
        }
    }

    async fn graphql_query(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value, TrackerError> {
        let api_key = self
            .api_key
            .as_deref()
            .ok_or(TrackerError::NotAuthenticated)?;

        let body = serde_json::json!({
            "query": query,
            "variables": variables,
        });

        let resp = self
            .client
            .post(&self.endpoint)
            .header("Authorization", format_authorization_header(api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp
                .text()
                .await
                .unwrap_or_else(|err| format!("<failed to read response body: {err}>"));
            let body = truncate_response_body(body, 1024);
            return Err(TrackerError::HttpStatus { status, body });
        }

        let json: serde_json::Value = resp.json().await?;

        // Check for GraphQL errors
        if let Some(errors) = json.get("errors") {
            return Err(TrackerError::GraphQLError(errors.to_string()));
        }

        Ok(json)
    }
}

#[async_trait::async_trait]
impl TrackerClient for LinearClient {
    async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        let query = r#"
        query($projectSlug: String!, $activeStates: [String!]!) {
          issues(first: 50, filter: {
            project: { slugId: { eq: $projectSlug } }
            state: { name: { in: $activeStates } }
          }) {
            nodes {
              id
              identifier
              title
              description
              priority
              state { name }
              branchName
              url
              labels { nodes { name } }
              relations { nodes {
                type
                issue {
                  id
                  identifier
                  state { name }
                }
                relatedIssue {
                  id
                  identifier
                  state { name }
                }
              }}
              createdAt
              updatedAt
            }
          }
        }
        "#;

        let variables = serde_json::json!({
            "projectSlug": self.project_slug,
            "activeStates": self.active_states,
        });

        let result = self.graphql_query(query, variables).await?;
        parse_candidate_issues(&result).map_err(TrackerError::MalformedPayload)
    }

    async fn fetch_issue_states_by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }

        let query = r#"
        query($ids: [ID!]!) {
          issues(filter: { id: { in: $ids } }) {
            nodes {
              id
              identifier
              title
              description
              priority
              state { name }
              branchName
              url
              labels { nodes { name } }
              createdAt
              updatedAt
            }
          }
        }
        "#;

        let variables = serde_json::json!({ "ids": ids });
        let result = self.graphql_query(query, variables).await?;
        parse_candidate_issues(&result).map_err(TrackerError::MalformedPayload)
    }

    async fn fetch_terminal_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        let query = r#"
        query($projectSlug: String!, $terminalStates: [String!]!) {
          issues(first: 50, filter: {
            project: { slugId: { eq: $projectSlug } }
            state: { name: { in: $terminalStates } }
          }) {
            nodes {
              id
              identifier
              title
              state { name }
            }
          }
        }
        "#;

        let variables = serde_json::json!({
            "projectSlug": self.project_slug,
            "terminalStates": self.terminal_states,
        });

        let result = self.graphql_query(query, variables).await?;
        parse_candidate_issues(&result).map_err(TrackerError::MalformedPayload)
    }

    async fn transition_issue_state(
        &self,
        issue_id: &str,
        state_name: &str,
    ) -> Result<(), TrackerError> {
        let state_id = self.resolve_workflow_state_id(issue_id, state_name).await?;

        let query = r#"
        mutation($issueId: String!, $stateId: String!) {
          issueUpdate(id: $issueId, input: { stateId: $stateId }) {
            success
            issue {
              id
              identifier
              state { name }
            }
          }
        }
        "#;

        let variables = serde_json::json!({
            "issueId": issue_id,
            "stateId": state_id,
        });

        let result = self.graphql_query(query, variables).await?;
        let success = result
            .pointer("/data/issueUpdate/success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if success {
            Ok(())
        } else {
            Err(TrackerError::MalformedPayload(format!(
                "issueUpdate did not succeed: {result}"
            )))
        }
    }
}

impl LinearClient {
    async fn resolve_workflow_state_id(
        &self,
        issue_id: &str,
        state_name: &str,
    ) -> Result<String, TrackerError> {
        let issue_query = r#"
        query($issueId: String!) {
          issue(id: $issueId) {
            team { id }
          }
        }
        "#;

        let issue_result = self
            .graphql_query(
                issue_query,
                serde_json::json!({
                    "issueId": issue_id,
                }),
            )
            .await?;
        let team_id = issue_result
            .pointer("/data/issue/team/id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                TrackerError::MalformedPayload(format!(
                    "missing issue team in response: {issue_result}"
                ))
            })?;

        if let Some(state_id) = self
            .find_workflow_state_by_filter(serde_json::json!({
                "team": { "id": { "eq": team_id } },
                "name": { "eqIgnoreCase": state_name },
            }))
            .await?
        {
            return Ok(state_id);
        }

        let state_type = match state_name.to_lowercase().as_str() {
            "done" | "completed" | "complete" => Some("completed"),
            "canceled" | "cancelled" => Some("canceled"),
            "duplicate" => Some("duplicate"),
            _ => None,
        };

        if let Some(state_type) = state_type
            && let Some(state_id) = self
                .find_workflow_state_by_filter(serde_json::json!({
                    "team": { "id": { "eq": team_id } },
                    "type": { "eq": state_type },
                }))
                .await?
        {
            return Ok(state_id);
        }

        Err(TrackerError::MalformedPayload(format!(
            "could not find workflow state '{state_name}' for issue {issue_id}"
        )))
    }

    async fn find_workflow_state_by_filter(
        &self,
        filter: serde_json::Value,
    ) -> Result<Option<String>, TrackerError> {
        let query = r#"
        query($filter: WorkflowStateFilter) {
          workflowStates(first: 10, filter: $filter) {
            nodes {
              id
              name
              type
            }
          }
        }
        "#;

        let result = self
            .graphql_query(
                query,
                serde_json::json!({
                    "filter": filter,
                }),
            )
            .await?;

        Ok(result
            .pointer("/data/workflowStates/nodes")
            .and_then(|v| v.as_array())
            .and_then(|nodes| nodes.first())
            .and_then(|node| node["id"].as_str())
            .map(String::from))
    }
}

// ── Mock Tracker (for testing) ──────────────────────────────────────────────

pub struct MockTracker {
    pub issues: Vec<Issue>,
}

impl MockTracker {
    pub fn new(issues: Vec<Issue>) -> Self {
        Self { issues }
    }
}

#[async_trait::async_trait]
impl TrackerClient for MockTracker {
    async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        let active: Vec<String> = vec!["Todo".into(), "In Progress".into()];
        Ok(self
            .issues
            .iter()
            .filter(|i| i.is_active(&active))
            .cloned()
            .collect())
    }

    async fn fetch_issue_states_by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, TrackerError> {
        Ok(self
            .issues
            .iter()
            .filter(|i| ids.contains(&i.id))
            .cloned()
            .collect())
    }

    async fn fetch_terminal_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        let terminal: Vec<String> = vec![
            "Closed".into(),
            "Cancelled".into(),
            "Canceled".into(),
            "Duplicate".into(),
            "Done".into(),
        ];
        Ok(self
            .issues
            .iter()
            .filter(|i| i.is_terminal(&terminal))
            .cloned()
            .collect())
    }

    async fn transition_issue_state(
        &self,
        _issue_id: &str,
        _state_name: &str,
    ) -> Result<(), TrackerError> {
        Ok(())
    }
}

// ── Parsing helpers ─────────────────────────────────────────────────────────

/// Format an API key into a proper Authorization header value.
/// Adds "Bearer " prefix if the key does not already start with
/// "Bearer " or "lin_api_".
fn format_authorization_header(api_key: &str) -> String {
    if api_key.starts_with("Bearer ") || api_key.starts_with("lin_api_") {
        api_key.to_string()
    } else {
        format!("Bearer {api_key}")
    }
}

fn parse_candidate_issues(json: &serde_json::Value) -> Result<Vec<Issue>, String> {
    let nodes = json
        .pointer("/data/project/issues/nodes")
        .or_else(|| json.pointer("/data/issues/nodes"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("unexpected response shape: {}", json))?;

    nodes
        .iter()
        .map(|node| {
            let id = node["id"].as_str().ok_or("missing id")?.to_string();
            let identifier = node["identifier"]
                .as_str()
                .ok_or("missing identifier")?
                .to_string();
            let title = node["title"].as_str().ok_or("missing title")?.to_string();
            let description = node["description"].as_str().map(String::from);
            let priority = node["priority"].as_f64().map(|p| p as i32);
            let state = node["state"]["name"]
                .as_str()
                .unwrap_or("Unknown")
                .to_string();
            let branch_name = node["branchName"].as_str().map(String::from);
            let url = node["url"].as_str().map(String::from);

            let labels = node["labels"]["nodes"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|l| l["name"].as_str().map(|s| s.to_lowercase()))
                        .collect()
                })
                .unwrap_or_default();

            let blocked_by = node["relations"]["nodes"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|r| {
                            if r["type"].as_str() != Some("blocks") {
                                return None;
                            }

                            let issue_ref = &r["issue"];
                            let related_issue_ref = &r["relatedIssue"];
                            if related_issue_ref["id"].as_str() != Some(id.as_str()) {
                                return None;
                            }

                            Some(crate::types::BlockerRef {
                                id: issue_ref["id"].as_str().map(String::from),
                                identifier: issue_ref["identifier"].as_str().map(String::from),
                                state: issue_ref["state"]["name"].as_str().map(String::from),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();

            let created_at = node["createdAt"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc));
            let updated_at = node["updatedAt"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc));

            Ok(Issue {
                id,
                identifier,
                title,
                description,
                priority,
                state,
                branch_name,
                url,
                labels,
                blocked_by,
                created_at,
                updated_at,
            })
        })
        .collect()
}

fn truncate_response_body(body: String, max_chars: usize) -> String {
    let mut chars = body.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_candidate_issues() {
        let json = serde_json::json!({
            "data": {
                "issues": {
                    "nodes": [
                        {
                            "id": "abc123",
                            "identifier": "MT-1",
                            "title": "Test issue",
                            "description": "desc",
                            "priority": 2,
                            "state": { "name": "Todo" },
                            "branchName": null,
                            "url": "https://linear.app/MT-1",
                            "labels": { "nodes": [{ "name": "bug" }] },
                            "relations": {
                                "nodes": [
                                    {
                                        "type": "blocks",
                                        "issue": {
                                            "id": "blocker1",
                                            "identifier": "MT-0",
                                            "state": { "name": "In Progress" }
                                        },
                                        "relatedIssue": {
                                            "id": "abc123",
                                            "identifier": "MT-1",
                                            "state": { "name": "Todo" }
                                        }
                                    },
                                    {
                                        "type": "related",
                                        "issue": {
                                            "id": "abc123",
                                            "identifier": "MT-1",
                                            "state": { "name": "Todo" }
                                        },
                                        "relatedIssue": {
                                            "id": "other",
                                            "identifier": "MT-2",
                                            "state": { "name": "Todo" }
                                        }
                                    },
                                    {
                                        "type": "blocks",
                                        "issue": {
                                            "id": "abc123",
                                            "identifier": "MT-1",
                                            "state": { "name": "Todo" }
                                        },
                                        "relatedIssue": {
                                            "id": "blocked-by-current",
                                            "identifier": "MT-3",
                                            "state": { "name": "Todo" }
                                        }
                                    }
                                ]
                            },
                            "createdAt": "2024-01-01T00:00:00Z",
                            "updatedAt": null
                        }
                    ]
                }
            }
        });

        let issues = parse_candidate_issues(&json).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].identifier, "MT-1");
        assert_eq!(issues[0].labels, vec!["bug"]);
        assert_eq!(issues[0].blocked_by.len(), 1);
        assert_eq!(issues[0].blocked_by[0].identifier.as_deref(), Some("MT-0"));
    }

    #[test]
    fn truncates_long_response_bodies() {
        let body = "abcdef".to_string();
        assert_eq!(truncate_response_body(body, 3), "abc...");
        assert_eq!(truncate_response_body("abc".into(), 3), "abc");
    }
}
