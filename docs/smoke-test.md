# Smoke test

A manual end-to-end check of the whole system: the CLI, the plugin's hooks and skills, Herdr,
worktrunk and GitHub, driven through the conductor the way a user would. Run it before a
release, or after a change that touches more than one of those pieces.

It takes about 30 minutes, most of it waiting on workers. Tick each box as you go, then fill in
[Results](#results) and paste them into the PR.

## Setup

- [ ] The [requirements](../README.md#requirements) are installed, `gh auth status` passes,
      and you're in a Herdr terminal (`echo $HC_TERMINAL_ID` prints `t…`).
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
- [ ] If the deck mod is installed, the deck opens to the right.
- [ ] `repo.json` in `$CONDUCTOR_HOME/repos/<repo-id>/` records the conductor's
      `terminal_id` and `space_id`.
- [ ] In another tab, `conductor status` prints an empty table, and `conductor deck --json`
      prints the repo with no tasks.

## 2. One ship task and one scout task

Give the conductor a goal that splits into one change and one question, e.g.:

> Fix the bug in `add` in calc.py. Separately, find out which licence this repo uses and
> whether it has a CONTRIBUTING file; just report back.

- [ ] It restates the goal and proposes two tasks: a **ship** task for the fix and a **scout**
      task for the question.
- [ ] Once you agree, it briefs and spawns both. `conductor status` shows them `working`, each
      with a branch.
- [ ] Herdr shows two new tabs in the conductor's space, named after the task ids, each running
      a worker `claude` session in its worktree (`.worktrees/…`). `conductor task show <id>`
      lists each one's terminal.
- [ ] `conductor status` shows live activity (tools, idle) that changes as the workers run.

## 3. A decision answered from the conductor

To force a decision, message the ship worker through the conductor right after it's spawned
(a small task can open its draft PR within a minute):

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
      tab are gone, and `hc ls` no longer lists its terminal.

## 5. Draft PR → ready → merged → automatic teardown

- [ ] The ship worker pushes, opens a **draft** PR, and reports `done` with its URL.
- [ ] The conductor wakes, reviews the diff, and either sends a fix-up or runs
      `conductor pr ready <id>` and gives you the URL. The task moves to `awaiting_merge`.
- [ ] `conductor status` at once shows the PR as `open` (not `draft`), with its checks and
      review state.
- [ ] Merge the PR on GitHub (any merge style).
- [ ] Within the PR poll interval, `conductor watch` wakes the conductor. The task moves to
      `merged`, and the conductor runs `conductor teardown` without being asked.
- [ ] Teardown pulls `main` in the main checkout, stops the worker (its tab closes), and
      removes the worktree and branch. `conductor status --all` shows `torn_down` and the PR
      as `merged`.

## 6. Close the conductor's tab, then resume

Set up a live task to resume:

- [ ] Give the conductor a small new ship task (e.g. "add a `sub` function to calc.py"), and
      wait until it's `working` and the worker has made at least one tool call.
- [ ] Note the task id and its terminal (`conductor task show <id>`).

Close everything:

- [ ] In Herdr, close the conductor's tab. The worker's terminal is its child, so the
      worker's tab closes with it.
- [ ] `conductor status` still lists the task in its phase, with activity `exited`: the
      worker's session-end hook runs as its terminal is killed.
- [ ] `ls .worktrees/` and `git branch` show the worktree and branch still there.

Bring it back:

- [ ] Open a new tab in the space, in the repo's main checkout. In its shell, before starting
      the conductor, `conductor resume --dry-run` prints `hc rm <old terminal>` (if hc still
      lists it) and an `hc run … claude --resume <session-id> …` line for the task.
- [ ] Run `conductor start` there (with the same `CONDUCTOR_HOME`). The fresh conductor sees
      the live task and runs `conductor resume` itself, which prints
      `<id> resumed in terminal <terminal>`. If it doesn't, ask it to resume the workers.
- [ ] A new tab named after the task appears in the space. Its worker is the old session, with
      its history, in the worktree.
- [ ] `conductor task show <id>` shows the new terminal. `conductor status` shows live activity
      again once the worker does something.
- [ ] Running `conductor resume` again prints `<id> skipped: terminal <terminal> is still
      running`.
- [ ] The resumed worker can still reach the new conductor: when it next reports (or when you
      ask it to, through the conductor), the report shows up in `conductor status` and the
      conductor acts on it.

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
- Herdr.app / herdr-core / worktrunk / Claude Code versions:
- 1. Start: pass / fail
- 2. Ship and scout tasks: pass / fail
- 3. Decision: pass / fail
- 4. Scout teardown: pass / fail
- 5. PR → merged → teardown: pass / fail
- 6. Close the tab and resume: pass / fail
- Notes:
- Follow-up issues:
```
