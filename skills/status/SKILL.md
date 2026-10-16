---
name: status
description: Summarize this repo's conductor tasks - what needs attention, what's in progress, what's awaiting merge, and what's done. Use when the user asks how the conductor's tasks, workers or PRs stand.
---

# Conductor status

Summarize the repo's tasks for the user. This only reads state; never change a task.

## 1. Read

```sh
conductor status --json --all
```

`--all` includes torn-down tasks. Each element has `id`, `title`, `kind`, `phase` and
`activity`, and may have:

- `detail`: the running tool, the permission message, the turn's error, or the exit reason;
- `open_decisions`: keys of the task's unanswered decisions;
- `branch`, `pr_url`;
- `pr`: `state` (`draft`, `open`, `merged`, `closed`), `checks` (`pass`, `fail`, `pending`) and
  `review` (`approved`, `changes_requested`, `review_required`).

If it prints `[]`, say there are no tasks and stop. If the command fails, show its error.

## 2. Group

A task whose `phase` is `torn_down` or `abandoned` is **done**. Put each other task in the first
group it matches:

1. **Needs attention:**
   - `activity` is `needs_permission`;
   - `open_decisions` isn't empty, or `phase` is `needs_decision` or `failed`;
   - `activity` is `exited` and `phase` isn't `merged`;
   - `activity` is `idle` with a `detail` (the turn ended on an error);
   - `pr.checks` is `fail`, `pr.review` is `changes_requested`, or `pr.state` is `closed`.
2. **In progress:** `phase` is `intake`, `briefed`, `spawning`, `working` or `review`.
3. **Awaiting merge:** `phase` is `awaiting_merge` or `merged`.

## 3. Report

One heading per group, in the order above with done last, leaving out empty groups. One line per task: the id,
the title, then the one fact that matters most, and the PR URL when there is one.

- Needs attention: what it's waiting on, e.g. "decision `db`", "needs permission (Bash)",
  "CI failing", "changes requested", "PR closed without merging", "failed: <detail>".
- In progress: the phase, and what the worker is doing when it's working, e.g.
  "working · Edit src/login.rs", "in review".
- Awaiting merge: the PR's checks and review, e.g. "checks pass · approved".
- Done: "merged" or "abandoned".

End with what the user can do next, if anything needs them: answer a decision (here or with `d`
in the deck), approve a permission prompt (focus the worker's pane with `Enter` in the deck),
reopen exited workers whose panes were closed (`conductor resume`), or merge a PR on GitHub.
