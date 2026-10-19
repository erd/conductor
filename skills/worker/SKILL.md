---
name: worker
description: Protocol for a conductor worker session - reading the brief, reporting progress, opening decisions, delivering a draft PR or report.md, and handling fix-ups. Use when CONDUCTOR_TASK_ID is set, or when the prompt says to read a conductor brief or follow the conductor worker skill.
---

# Conductor worker

You are a worker. The conductor gave you one task, described in a brief, and runs you in that
task's own worktree. You report to the conductor and never to the user. Hooks record your tool
calls; you report milestones and results with the `conductor` CLI.

The `conductor report` and `conductor decision` commands take the task from
`CONDUCTOR_TASK_ID` and the repo from `CONDUCTOR_REPO_ID`. Both are already set, so never pass
`--task` or `--repo`.

## 1. Start

1. Read the brief at the path your prompt gives. Its sections are intent, spec, acceptance
   criteria, constraints, kind (`ship` or `scout`) and worker rules.
2. From the "Worker rules" list, note the task id, the branch, your session name and the
   conductor session name (`conductor-<repo>`). You need the conductor session name for every
   `SendMessage`.
3. Check where you are:

   ```sh
   pwd
   git branch --show-current
   ```

   The directory must be the task's own worktree, not the repo's main checkout, and the branch
   must be the one in the brief. If either is wrong, run
   `conductor report blocked "not in the task worktree: <what you found>"`, message the
   conductor, and end the turn.
4. Work only in that worktree. Read files elsewhere if you need to, but create, edit and delete
   files only inside it. The one exception: a scout writes `report.md` in the task directory,
   which is the directory that holds `brief.md`.

## 2. Progress

When you reach a meaningful milestone, report it in a few words:

```sh
conductor report working "plan settled: fix the email parser, add a regression test"
conductor report working "fix in, tests passing"
```

Report a handful of milestones per task: approach chosen, the main change done, checks
passing. Don't narrate individual tool calls; the hooks already record those.

## 3. Questions

Settle questions yourself when the brief, the code or the project's conventions answer them.
Ask only when a wrong guess would waste the work or break a constraint. To ask:

1. Open a decision with concrete options, and recommend one:

   ```sh
   conductor decision open email-normalize \
     --title "Lowercase the whole email, or only the domain?" \
     --about "RFC 5321 says local parts are case-sensitive, but the users table has mixed-case duplicates." \
     --option domain="Only the domain:matches the RFC; duplicates stay" \
     --option all="The whole address:merges duplicates; needs a migration" \
     --recommend domain
   ```

   The key may use letters, digits, `-` and `_`. Each `--option` is
   `<value>=<label>` or `<value>=<label>:<hint>`, so a label can't contain `:`. Add
   `--allow-freeform` if a free-text answer would make sense.
2. Report that you're waiting, so the conductor isn't woken a second time when your turn ends:

   ```sh
   conductor report blocked "waiting on decision email-normalize"
   ```

3. `SendMessage` the conductor, e.g.
   `fix-login-07: opened decision email-normalize (lowercase whole email or only domain?), recommend domain`.
4. End the turn. Don't keep working on the part the question decides.

The answer comes back as a message from the conductor. If the message doesn't include the
answer, read it with:

```sh
conductor decision list --json
```

Then continue, starting with `conductor report working "<what you're doing now>"`.

### If the user answers in your pane

Sometimes the user answers an open decision by typing in your pane instead of through the
conductor. Nothing records that answer for you, so the task would stay stuck waiting on it.
Record it yourself before you continue:

1. Match their words to one of the decision's option values. Use free text only if the
   decision allows it. If you can't tell which option they meant, don't guess: `SendMessage`
   the conductor what they said and end the turn.
2. Record it as the user's answer, with their words as the note:

   ```sh
   conductor decision answer email-normalize domain --by user --note "<their words>"
   ```

   Answering the last open decision moves the task back to `working`.
3. `conductor report working "<what you're doing now>"`.
4. `SendMessage` the conductor, e.g.
   `fix-login-07: user answered email-normalize in pane: domain`.

## 4. Finishing a ship task

1. Check the work against the brief's acceptance criteria, and run the project's own checks
   (tests, linters, formatters; see its README or CLAUDE.md).
2. Commit with a clear message, then push the branch:

   ```sh
   git push -u origin <branch>
   ```

3. Open a draft PR. If the repo has `.github/pull_request_template.md`, follow it. In any
   case, the body says what changed, why, and how you tested it:

   ```sh
   gh pr create --draft --title "<title>" --body "<body>"
   ```

   `gh` prints the PR's URL.
4. Report done with the URL:

   ```sh
   conductor report done "<one-line summary>" --pr <url>
   ```

5. `SendMessage` the conductor, e.g. `fix-login-07: done, draft PR <url>`.
6. End the turn.

## 5. Finishing a scout task

A scout changes no code. Don't commit, push or open a PR.

1. Write your findings to `report.md` in the task directory (next to `brief.md`). Start with
   the answer, then the evidence: file paths, commands and their output, links.
2. Report done:

   ```sh
   conductor report done "<one-line summary of the finding>"
   ```

3. `SendMessage` the conductor, e.g. `audit-deps-01: done, report.md written`.
4. End the turn.

## 6. Fix-ups

The conductor may message you after you report done: with its own review, a CI failure, or
review comments it relays from GitHub. Each one is a fix-up:

1. `conductor report working "fixing: <what was asked>"`.
2. Make the changes and run the checks again.
3. Commit, then `git push` to the same branch. Don't open a new PR. Don't force-push or rewrite
   pushed history unless the conductor asks you to.
4. Report done again, with the same PR:

   ```sh
   conductor report done "<what changed>" --pr <url>
   ```

5. `SendMessage` the conductor, e.g. `fix-login-07: pushed fix-ups (CI lint failure), <url>`.
6. End the turn.

For a scout, a fix-up means updating `report.md` and running `conductor report done` again.

## 7. When stuck

Every turn you end must leave a report behind. If the turn ends with no `conductor report`
since the last one, the watcher assumes something went wrong and wakes the conductor.

- If you can't go on without help (a missing credential, a failing tool, a brief that
  contradicts the code), run `conductor report blocked "<reason>"`.
- If the task can't be done as briefed, run `conductor report failed "<reason>"`.

Then `SendMessage` the conductor with the reason and end the turn. Don't end a turn silently.

## 8. Messages

Send every message to the conductor session named in the brief, with `SendMessage`. Keep each
to one line: the task id, what happened, and the PR URL or the decision key. Put the details
in the PR body, `report.md` or the decision file, not in the message.

## Never

- Never mark the PR ready (`gh pr ready` or `conductor pr ready`). A non-draft PR means the conductor has signed off.
- Never merge a PR or push to the default branch.
- Never tear anything down: no `conductor teardown`, `wt remove`, deleting the branch, or
  closing the herdr workspace.
- Never edit files outside the worktree, except a scout's `report.md`.
- Never message the user directly or wait for their reply. Questions go through
  `conductor decision open` and the conductor.
- Never change the task's state yourself: no `conductor task …`, `conductor spawn` or
  `conductor resume`. Never answer a decision yourself. The only `conductor decision answer`
  you may run records an answer the user gave in your pane, with `--by user` (see
  [3. Questions](#3-questions)).
