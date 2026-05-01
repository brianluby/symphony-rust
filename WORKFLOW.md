---
tracker:
  kind: linear
  api_key: $LINEAR_API_KEY
  project_slug: 5692ee81fcd8

polling:
  interval_ms: 30000

workspace:
  root: ./workspaces

hooks:
  timeout_ms: 120000
  after_create: |
    set -euo pipefail
    workspace_path="$(pwd -P)"
    repo_root="${SYMPHONY_REPO_ROOT:-$(git -C "$workspace_path/../.." rev-parse --show-toplevel)}"
    worktree_path="$workspace_path/repo"
    branch="symphony/$SYMPHONY_WORKSPACE_KEY"

    if [ -d "$worktree_path/.git" ] || [ -f "$worktree_path/.git" ]; then
      exit 0
    fi

    if [ -d "$worktree_path" ]; then
      if [ -z "$(find "$worktree_path" -mindepth 1 -maxdepth 1 -print -quit)" ]; then
        rmdir "$worktree_path"
      else
        echo "$worktree_path exists but is not a Git worktree; refusing to overwrite" >&2
        exit 1
      fi
    fi

    if git -C "$repo_root" show-ref --verify --quiet "refs/heads/$branch"; then
      git -C "$repo_root" worktree add "$worktree_path" "$branch"
    else
      git -C "$repo_root" worktree add -b "$branch" "$worktree_path" HEAD
    fi

  before_run: |
    set -euo pipefail
    workspace_path="$(pwd -P)"
    repo_root="${SYMPHONY_REPO_ROOT:-$(git -C "$workspace_path/../.." rev-parse --show-toplevel)}"
    worktree_path="$workspace_path/repo"
    branch="symphony/$SYMPHONY_WORKSPACE_KEY"

    if [ ! -d "$worktree_path/.git" ] && [ ! -f "$worktree_path/.git" ]; then
      if [ -d "$worktree_path" ]; then
        if [ -z "$(find "$worktree_path" -mindepth 1 -maxdepth 1 -print -quit)" ]; then
          rmdir "$worktree_path"
        else
          echo "$worktree_path exists but is not a Git worktree; refusing to overwrite" >&2
          exit 1
        fi
      fi

      if git -C "$repo_root" show-ref --verify --quiet "refs/heads/$branch"; then
        git -C "$repo_root" worktree add "$worktree_path" "$branch"
      else
        git -C "$repo_root" worktree add -b "$branch" "$worktree_path" HEAD
      fi
    fi

    git -C "$worktree_path" status --short

  before_remove: |
    set -euo pipefail
    workspace_path="$(pwd -P)"
    repo_root="${SYMPHONY_REPO_ROOT:-$(git -C "$workspace_path/../.." rev-parse --show-toplevel)}"
    worktree_path="$workspace_path/repo"

    if [ -d "$worktree_path" ]; then
      git -C "$repo_root" worktree remove "$worktree_path"
    fi

agent:
  max_concurrent_agents: 1
  max_turns: 20
  max_retry_backoff_ms: 300000
  claim_state: In Progress
  completion_state: In Review

codex:
  command: codex app-server
  turn_timeout_ms: 3600000
  stall_timeout_ms: 300000
---

You are an expert software engineer working on issue {{ issue.identifier }}: {{ issue.title }}.

The repository for this issue is checked out as a dedicated Git worktree in
`repo/` inside the current workspace. Do all repository inspection, edits,
tests, commits, and pushes from `repo/`. Do not edit files outside `repo/`.
Symphony moves this issue to In Progress when it claims the work and to In
Review after a successful run.

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
8. When the pull request is ready, leave a comment on this Linear issue with
   the PR URL and a concise summary.

{% if attempt %}This is retry attempt {{ attempt }}.{% else %}This is the first run attempt.{% endif %}
If you get stuck, document what you tried and why it didn't work.
Keep changes minimal and focused on this issue only.
