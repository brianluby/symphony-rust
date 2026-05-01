//! Prompt template rendering with Liquid (strict mode).
//! Maps to SPEC.md §5.4.

use liquid::ParserBuilder;
use liquid::model::{Object, Value};

use crate::config::ConfigError;
use crate::types::Issue;

/// Render a prompt template for a given issue and attempt number.
///
/// Variables available in template scope:
/// - `issue` — the full Issue object
/// - `attempt` — attempt number (null/absent for first run)
pub fn render_prompt(
    template: &str,
    issue: &Issue,
    attempt: Option<u32>,
) -> Result<String, ConfigError> {
    let parser = ParserBuilder::with_stdlib()
        .build()
        .map_err(|e| ConfigError::TemplateParseError(e.to_string()))?;

    let template = parser
        .parse(template)
        .map_err(|e| ConfigError::TemplateParseError(e.to_string()))?;

    let mut globals = Object::new();

    // Build issue object
    let mut issue_obj = Object::new();
    issue_obj.insert("id".into(), Value::Scalar(issue.id.clone().into()));
    issue_obj.insert(
        "identifier".into(),
        Value::Scalar(issue.identifier.clone().into()),
    );
    issue_obj.insert("title".into(), Value::Scalar(issue.title.clone().into()));
    issue_obj.insert(
        "description".into(),
        Value::Scalar(issue.description.clone().unwrap_or_default().into()),
    );
    issue_obj.insert("state".into(), Value::Scalar(issue.state.clone().into()));

    if let Some(prio) = issue.priority {
        issue_obj.insert("priority".into(), Value::Scalar((prio as i64).into()));
    }

    let labels: Vec<Value> = issue
        .labels
        .iter()
        .map(|l| Value::Scalar(l.clone().into()))
        .collect();
    issue_obj.insert("labels".into(), Value::Array(labels));

    if let Some(url) = &issue.url {
        issue_obj.insert("url".into(), Value::Scalar(url.clone().into()));
    }
    if let Some(branch) = &issue.branch_name {
        issue_obj.insert("branch_name".into(), Value::Scalar(branch.clone().into()));
    }

    // Blockers
    let blockers: Vec<Value> = issue
        .blocked_by
        .iter()
        .map(|b| {
            let mut obj = Object::new();
            if let Some(id) = &b.id {
                obj.insert("id".into(), Value::Scalar(id.clone().into()));
            }
            if let Some(ident) = &b.identifier {
                obj.insert("identifier".into(), Value::Scalar(ident.clone().into()));
            }
            if let Some(state) = &b.state {
                obj.insert("state".into(), Value::Scalar(state.clone().into()));
            }
            Value::Object(obj)
        })
        .collect();
    issue_obj.insert("blocked_by".into(), Value::Array(blockers));

    globals.insert("issue".into(), Value::Object(issue_obj));

    // Attempt: null is represented as nil
    match attempt {
        Some(n) => {
            globals.insert("attempt".into(), Value::Scalar((n as i64).into()));
        }
        None => {
            globals.insert("attempt".into(), Value::Nil);
        }
    }

    let rendered = template
        .render(&globals)
        .map_err(|e| ConfigError::TemplateRenderError(e.to_string()))?;

    // Trim leading/trailing whitespace but preserve interior blank lines
    Ok(rendered.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_issue() -> Issue {
        Issue {
            id: "abc123".into(),
            identifier: "MT-649".into(),
            title: "Fix login bug".into(),
            description: Some("Users cannot log in with SSO".into()),
            priority: Some(2),
            state: "In Progress".into(),
            branch_name: None,
            url: Some("https://linear.app/issue/MT-649".into()),
            labels: vec!["bug".into(), "p1".into()],
            blocked_by: vec![],
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn test_render_simple_template() {
        let tmpl = "Fix issue {{ issue.identifier }}: {{ issue.title }}";
        let result = render_prompt(tmpl, &sample_issue(), None).unwrap();
        assert_eq!(result, "Fix issue MT-649: Fix login bug");
    }

    #[test]
    fn test_render_with_attempt() {
        let tmpl = "Retry {{ issue.id }} attempt {{ attempt }}";
        let result = render_prompt(tmpl, &sample_issue(), Some(3)).unwrap();
        assert_eq!(result, "Retry abc123 attempt 3");
    }

    #[test]
    fn test_render_with_null_attempt() {
        let tmpl = "{% if attempt == nil %}First run!{% else %}Retry {{ attempt }}{% endif %}";
        let result = render_prompt(tmpl, &sample_issue(), None).unwrap();
        assert_eq!(result, "First run!");
    }

    #[test]
    fn test_unknown_variable_fails() {
        let tmpl = "Hello {{ nonexistent }}";
        let result = render_prompt(tmpl, &sample_issue(), None);
        assert!(result.is_err());
    }

    #[test]
    fn test_labels_rendering() {
        let tmpl = "Labels: {% for label in issue.labels %}{{ label }} {% endfor %}";
        let result = render_prompt(tmpl, &sample_issue(), None).unwrap();
        assert!(result.contains("bug"));
        assert!(result.contains("p1"));
    }
}
