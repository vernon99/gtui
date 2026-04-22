# Legacy webui snapshot fixtures

Captured via `python3 webui/server.py --gt-root <root>` followed by
`curl http://127.0.0.1:8420/api/snapshot` for use as parity anchors while
porting `build_snapshot` (webui/server.py:1623) to Rust.

Files:

- `webui_snapshot_populated.json` — captured against `$HOME/gt` with the full
  polecat swarm running. Terminal transcripts inside `actions[].terminal.*_view`
  were truncated to 2 items per view (with a `_fixture_note` marker) to keep the
  fixture ~320KB instead of 11MB; nothing else was edited.
- `webui_snapshot_empty.json` — captured against an empty directory
  (`mkdir /tmp/empty-gt && python3 webui/server.py --gt-root /tmp/empty-gt`).
  The `gt` CLI still returns data for globally-registered rigs, so
  `git`/`agents`/`crews` are non-empty; `graph`, `stores`, and `actions` are the
  fields that go to zero.

## Parity target (per gui-cqe.1)

JSON shape + key counts on the fields below. Exact values drift with wall
clock and live state; tests should assert on structure, not payload equality.

### Top-level (18 keys, always present)

```
actions, activity, agents, alerts, convoys, crews, errors,
generated_at, generation_ms, git, graph, gt_root, status,
status_legend, stores, summary, timings, vitals_raw
```

### Counts observed in fixtures

| Field                          | populated | empty |
|--------------------------------|-----------|-------|
| `graph.nodes`                  | 110       | 0     |
| `graph.edges`                  | 179       | 0     |
| `activity.groups`              | 4         | 0     |
| `activity.unassigned_agents`   | 4         | 4     |
| `git.repos`                    | ≥1        | ≥1    |
| `git.recent_commits`           | ≥1        | ≥1    |
| `convoys.convoys`              | ≥1        | 0     |
| `stores`                       | 4         | 0     |
| `agents`                       | 11        | 7     |
| `alerts`                       | 2         | 1     |
| `summary` (keys)               | 11        | 11    |
| `status_legend` (entries)      | 7         | 7     |
| `actions`                      | 12 (cap)  | 0     |

### Inner key schemas

Stable keys observed on populated records. Serialize-with-default is fine for
any missing keys on sparse rows.

- `graph.nodes[i]` — `agent_targets, assignee, blocked_by, blocked_by_count,
  closed_at, created_at, dependency_count, dependent_count, description, id,
  is_system, kind, labels, linked_commit_count, linked_commits, owner, parent,
  priority, scope, status, title, type, ui_status, updated_at`
- `graph.edges[i]` — `kind, source, target`
- `activity.groups[i]` — `agent_count, agents, events, is_system, memory,
  scope, stored_status, task_id, title, ui_status`
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
- `summary` — `active_agents, command_errors, derived_status_counts,
  done_tasks, ready_tasks, repos, running_tasks, stored_status_counts,
  stuck_tasks, system_running, task_groups`
- `status` — `overseer, raw, root_path, services, tmux_socket, town`
- `alerts` — `Vec<String>` (e.g. "Gas Town daemon is stopped.")

### Recapture recipe

```bash
python3 webui/server.py --gt-root "$HOME/gt" --port 8420 &
sleep 3  # let the background poller hydrate
curl -s http://127.0.0.1:8420/api/snapshot > /tmp/snapshot_full.json
kill %1
# then trim actions[].terminal.{claude,codex,transcript}_view.items to 2 entries
```

For the empty fixture, point `--gt-root` at an empty directory instead.
