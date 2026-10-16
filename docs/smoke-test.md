# Smoke test

A manual end-to-end check of the whole system: the CLI, the plugin's hooks and skills, herdr,
worktrunk and GitHub, driven through the conductor the way a user would. Run it before a
release, or after a change that touches more than one of those pieces.

It takes about 30 minutes, most of it waiting on workers. Tick each box as you go, then fill in
[Results](#results) and paste them into the PR.

## Setup

- [ ] The [requirements](../README.md#requirements) are installed, `gh auth status` passes,
      and you're inside herdr.
- [ ] Install the build under test, with either of these:

  ```sh
  cargo install --path crates/conductor                              # from a checkout
  cargo install --git https://github.com/erd/conductor --branch <branch> conductor
  ```

- [ ] Install the plugin from the same code. For a local checkout, add it as the marketplace
      (after `claude plugin marketplace remove conductor` if you added the GitHub one); for
      `main`, add `erd/conductor`. Then check that `/plugin` lists `conductor@conductor` as
      enabled:

  ```sh
  claude plugin marketplace add <path-to-checkout>   # or: erd/conductor
  claude plugin install conductor@conductor
  ```

- [ ] Use a throwaway state dir, so the run leaves your real one alone. Export it in the shell
      you start the conductor from:

  ```sh
  export CONDUCTOR_HOME="$(mktemp -d)/conductor"
  ```

- [ ] Create a scratch GitHub repo with something for the workers to change, and clone it:

  ```sh
  gh repo create conductor-smoke --private --clone --add-readme
  cd conductor-smoke
  printf 'def add(a, b):\n    return a - b\n' > calc.py
  git add calc.py && git commit -m "Add calc" && git push
  ```

## 1. Start

- [ ] `conductor start` opens the conductor's Claude session and loads the `conductor` skill.
      The conductor greets you as the conductor for `conductor-smoke`.
- [ ] If the deck plugin is installed, the deck opens to the right.
- [ ] In another pane, `conductor status` prints an empty table.

## 2. One ship task and one scout task

Give the conductor a goal that splits into one change and one question, e.g.:

> Fix the bug in `add` in calc.py. Separately, find out which licence this repo uses and
> whether it has a CONTRIBUTING file; just report back.

- [ ] It restates the goal and proposes two tasks: a **ship** task for the fix and a **scout**
      task for the question.
- [ ] Once you agree, it briefs and spawns both. `conductor status` shows them `working`, each
      with a branch.
- [ ] herdr shows two new workspaces folded under the conductor's, labelled with the task ids,
      each running a worker `claude` session in its worktree (`.worktrees/…`).
- [ ] `conductor status` shows live activity (tools, idle) that changes as the workers run.

## 3. A decision answered from the conductor

To force a decision, message the ship worker through the conductor:

> Tell the fix task to open a decision asking whether to also add a test file, with options
> `yes` and `no`, before it continues.

- [ ] The worker runs `conductor decision open`. The conductor wakes (via `conductor watch`),
      moves the task to `needs_decision`, and asks you the question. `conductor status` shows
      the open decision.
- [ ] Answer it in the conductor's session ("yes"). The conductor records it with
      `conductor decision answer … --by user`, messages the worker, and moves the task back
      to `working`.
- [ ] `conductor decision list` shows the decision answered. The worker carries on.

## 4. Scout report and teardown

- [ ] The scout worker writes `report.md` and reports `done`. The conductor wakes, summarizes
      the report for you, and tears the scout task down.
- [ ] `conductor status --all` shows the scout task `torn_down`. Its worktree, branch and
      herdr workspace are gone.

## 5. Draft PR → ready → merged → automatic teardown

- [ ] The ship worker pushes, opens a **draft** PR, and reports `done` with its URL.
- [ ] The conductor wakes, reviews the diff, and either sends a fix-up or runs `gh pr ready`
      and gives you the URL. The task moves to `awaiting_merge`.
- [ ] `conductor status` shows the PR's checks and review state.
- [ ] Merge the PR on GitHub (any merge style).
- [ ] Within the PR poll interval, `conductor watch` wakes the conductor. The task moves to
      `merged`, and the conductor runs `conductor teardown` without being asked.
- [ ] Teardown pulls `main` in the main checkout, stops the worker, removes the worktree and
      branch, and closes the workspace. `conductor status --all` shows `torn_down`.

## 6. Close the conductor's workspace, then resume

Set up a live task to resume:

- [ ] Give the conductor a small new ship task (e.g. "add a `sub` function to calc.py"), and
      wait until it's `working` and the worker has made at least one tool call.
- [ ] Note the task id and its pane id (`conductor task show <id>`).

Close everything:

- [ ] In herdr, close the conductor's workspace row. The worker's workspace closes with it.
- [ ] `conductor status` still lists the task as `working`. Its activity may lag, since the
      session ended without a clean exit.
- [ ] `ls .worktrees/` and `git branch` show the worktree and branch still there.

Bring it back:

- [ ] Open a new herdr workspace in the repo's main checkout and run `conductor start` (with
      the same `CONDUCTOR_HOME`). A fresh conductor session starts.
- [ ] `conductor resume --dry-run` prints a `herdr worktree open` and a `herdr pane run … claude
      --resume <session-id> …` line for the task.
- [ ] `conductor resume` (or ask the conductor to resume the workers) prints
      `<id> resumed in pane <pane>`.
- [ ] A new workspace for the task appears under the conductor's. Its worker is the old
      session, with its history, in the worktree.
- [ ] `conductor task show <id>` shows the new workspace and pane ids. `conductor status`
      shows live activity again once the worker does something.
- [ ] Running `conductor resume` again prints `<id> skipped: pane <pane> is still open`.
- [ ] The resumed worker can still reach the conductor: ask it, through the conductor, to
      report its status. The report shows up in `conductor status`.

## Cleanup

- [ ] `conductor teardown <id> --abandon --force-delete-branch` for the resumed task.
- [ ] Close any draft PRs left open, and delete the scratch repo:
      `gh repo delete conductor-smoke --yes`.
- [ ] `rm -rf "$CONDUCTOR_HOME"`.

## Results

Copy this into the PR and fill it in.

```markdown
### Smoke test

- Date / commit:
- herdr / worktrunk / Claude Code versions:
- 1. Start: pass / fail
- 2. Ship and scout tasks: pass / fail
- 3. Decision: pass / fail
- 4. Scout teardown: pass / fail
- 5. PR → merged → teardown: pass / fail
- 6. Close and resume: pass / fail
- Notes:
- Follow-up issues:
```
