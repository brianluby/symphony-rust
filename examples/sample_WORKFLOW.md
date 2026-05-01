---
tracker:
  kind: linear
  api_key: $LINEAR_API_KEY
  project_slug: my-project
  active_states:
    - Todo
    - In Progress
  terminal_states:
    - Closed
    - Cancelled
    - Done

polling:
  interval_ms: 30000

workspace:
  root: ./workspaces

hooks:
  after_create: |
    echo "Workspace created"
    git clone git@github.com:my-org/my-project.git . 2>/dev/null || true
  before_run: |
    echo "Starting agent run"
    git fetch origin 2>/dev/null || true
  after_run: |
    echo "Agent run completed"

agent:
  max_concurrent_agents: 4
  max_turns: 20
  max_retry_backoff_ms: 300000

codex:
  command: codex app-server
  turn_timeout_ms: 3600000
  stall_timeout_ms: 300000
---

You are an expert software engineer working on issue {{ issue.identifier }}: {{ issue.title }}.

## Issue Details

- **ID**: {{ issue.identifier }}
- **Title**: {{ issue.title }}
- **Priority**: {{ issue.priority }}
- **State**: {{ issue.state }}

{% if issue.description != "" %}
### Description
{{ issue.description }}
{% endif %}

## Labels
{% for label in issue.labels %}- {{ label }}
{% endfor %}

## Instructions

1. Read and understand the issue description.
2. Explore the relevant code in the repository.
3. Implement the fix or feature described.
4. Write or update tests to cover your changes.
5. Run the test suite to verify nothing is broken.
6. Create a branch and commit your changes with a descriptive message.
7. Push your branch and create a pull request.

{% if attempt %}This is retry attempt {{ attempt }}.{% else %}This is the first run attempt.{% endif %}
If you get stuck, document what you tried and why it didn't work.
Keep changes minimal and focused on this issue only.
