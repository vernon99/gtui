# Snapshot Contract Fixtures

These fixtures pin the JSON shape consumed by the GTUI frontend. They were
captured from a populated Gas Town workspace during the desktop migration and
are kept as stable contract fixtures, not as live data.

Files:

- `snapshot_contract_populated.json` — populated workspace with graph, agents,
  git history, stores, convoys, actions, and transcript views. Long transcript
  arrays inside action payloads were trimmed to keep the fixture small.
- `snapshot_contract_empty.json` — empty workspace root. The `gt` CLI can still
  report globally registered rigs, so `git`/`agents`/`crews` may be non-empty;
  `graph`, `stores`, and `actions` are the fields expected to go to zero.

## Contract Target

Tests assert on structure and presence, not exact live values. Timestamps,
counts, and workspace state can drift across real runs; these fixtures exist to
catch accidental removal or reshaping of frontend-facing sections.

### Top-Level Keys

```text
actions, activity, agents, alerts, convoys, crews, errors,
generated_at, generation_ms, git, graph, gt_root, status,
stores, summary, timings, vitals_raw
```

### Counts Observed In Fixtures

| Field                          | populated | empty |
|--------------------------------|-----------|-------|
| `graph.nodes`                  | 110       | 0     |
| `graph.edges`                  | 179       | 0     |
| `activity.groups`              | 4         | 0     |
| `activity.unassigned_agents`   | 4         | 4     |
| `git.repos`                    | >=1       | >=1   |
| `git.recent_commits`           | >=1       | >=1   |
| `convoys.convoys`              | >=1       | 0     |
| `stores`                       | 4         | 0     |
| `agents`                       | 11        | 7     |
| `alerts`                       | 2         | 1     |
| `summary` keys                 | 4         | 4     |
| `actions`                      | 12 cap    | 0     |

### Inner Key Schemas

- `graph.nodes[i]` — `agent_targets, assignee, blocked_by, blocked_by_count,
  closed_at, created_at, dependency_count, dependent_count, description, id,
  is_system, kind, labels, linked_commit_count, linked_commits, owner, parent,
  priority, scope, status, title, type, updated_at`
- `graph.edges[i]` — `kind, source, target`
- `activity.groups[i]` — `agent_count, agents, events, is_system, memory,
  scope, stored_status, task_id, title`
- `git.repos[i]` — `branches, id, label, recent_commits, root, scope, scopes,
  status, worktrees`
- `git.recent_commits[i]` — `committed_at, refs, repo_id, repo_label, sha,
  short_sha, subject, task_ids`
- `git.task_memory` — object keyed by task id; values carry commit refs
- `convoys` — `{ convoys: [...], task_index: {...} }`
- `convoys.convoys[i]` — `completed, id, status, title, total, tracked_ids`
- `stores[i]` — `available, blocked, closed, error, exact_status_counts,
  hooked, name, open, path, scope, summary, total`
- `agents[i]` — `crew, current_command, current_path, events, has_session,
  hook, kind, label, pane_id, recent_task, role, scope, session_name, target,
  task_events`
- `summary` — `active_agents, command_errors, repos, task_groups`
- `status` — `overseer, raw, root_path, services, tmux_socket, town`
- `alerts` — `Vec<String>`
