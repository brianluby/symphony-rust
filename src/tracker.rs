//! Issue tracker client — Linear adapter and mock tracker.
//! Maps to SPEC.md §11.

use crate::types::Issue;

/// Errors from tracker operations.
#[derive(Debug, thiserror::Error)]
pub enum TrackerError {
    #[error("HTTP request failed: {0}")]
    HttpError(#[from] reqwest::Error),
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

        if !resp.status().is_success() {
            return Err(TrackerError::HttpError(
                resp.error_for_status().unwrap_err(),
            ));
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
          project(slugId: $projectSlug) {
            issues(filter: { state: { name: { in: $activeStates } } }) {
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
                relations(type: "blocks", direction: "inverse") { nodes {
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
          project(slugId: $projectSlug) {
            issues(filter: { state: { name: { in: $terminalStates } } }) {
              nodes {
                id
                identifier
                title
                state { name }
              }
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
                        .map(|r| {
                            let ri = &r["relatedIssue"];
                            crate::types::BlockerRef {
                                id: ri["id"].as_str().map(String::from),
                                identifier: ri["identifier"].as_str().map(String::from),
                                state: ri["state"]["name"].as_str().map(String::from),
                            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_candidate_issues() {
        let json = serde_json::json!({
            "data": {
                "project": {
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
                                "createdAt": "2024-01-01T00:00:00Z",
                                "updatedAt": null
                            }
                        ]
                    }
                }
            }
        });

        let issues = parse_candidate_issues(&json).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].identifier, "MT-1");
        assert_eq!(issues[0].labels, vec!["bug"]);
    }
}
