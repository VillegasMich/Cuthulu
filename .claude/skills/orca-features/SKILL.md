---
name: orca-features
description: >-
  Start one or more new features in parallel, one supervised Orca agent per
  feature, each in its own branch + worktree. The current session acts as the
  coordinator: agrees the plan with the user, spawns workers via `orca
  orchestration`, relays every implementation doubt to the user, verifies each
  branch, and after explicit user approval opens the PRs and merges them into
  the target branch (default branch unless told otherwise). Use when the user
  says "start a feature", "new features with orca", "spawn agents per
  feature", "/orca-features", or lists several features to build in parallel.
---

# Orca features — coordinator workflow

You are the **coordinator**. You never implement feature code yourself; workers
do. You own: planning with the user, briefs, answering/relaying questions,
independent verification, PRs and merges.

Hard gates (never skip):

1. **Plan gate** — user approves the feature list + design decisions before any worker starts.
2. **Doubt gate** — a worker question that is a design/behavior choice goes to the user, not answered by you.
3. **Merge gate** — no push, PR, or merge until the user explicitly approves *that* feature after verification.

## 0. Resolve the Orca CLI and load its guide

- `ORCA_CLI_COMMAND` set → use it. Inside an Orca terminal (`TERM_PROGRAM=Orca`) → `orca`.
  On Linux outside Orca → `orca-ide` (bare `orca` there is the GNOME screen reader).
- Run `orca status --json`. If not running: `orca open --json`.
- Run `orca skills get orchestration` and follow it; it is version-matched and wins over
  anything below if they disagree. Load
  `--reference references/placement-and-remote.md` before creating worktrees.

## 1. Plan with the user (plan gate)

1. Read `CLAUDE.md` and the docs it lists (`docs/VISION.md`, `ARCHITECTURE.md`,
   `DESIGN.md`, `ROADMAP.md`) enough to place each feature.
2. For each feature, draft: slug (kebab-case, becomes branch `VillegasMich/<slug>`),
   goal, scope / out of scope, files likely touched, acceptance criteria, open questions.
3. Detect **overlap**: features touching the same files (e.g. `static/app.js`,
   `src/server.rs`, `docs/ARCHITECTURE.md`) will conflict at merge. Propose either
   ownership boundaries or ordering (dependency → later wave).
4. Ask the user the open questions with `AskUserQuestion` (batch ≤4 per call; give a
   recommended option first). The user wants to decide design together — do not assume.
5. Confirm: target branch (default: repo default branch, `gh repo view --json defaultBranchRef`),
   agent (default `claude`), and any model/effort override.
6. Show the final plan table (slug · goal · port · base · depends-on) and get an explicit "go".

## 2. Write one brief per feature

Build each brief in the scratchpad from `references/worker-brief.md` + the full text of
`references/rust-guidelines.md` (workers run in other worktrees and cannot read this
skill folder unless it is committed — inline it). Fill every placeholder. The brief must
satisfy the Task-spec contract: Target, Change, Constraints, Ownership, Observable acceptance.

Assign per worker: dev port `8691 + i` (never 8686, the user's own `cargo run`),
throwaway container prefix `cuthulu-<slug>-`.

## 3. Spawn workers

```sh
orca orchestration run-create --objective "Features: <slug1>, <slug2>" --json
orca orchestration worker-start \
  --spec "$(cat <scratchpad>/brief-<slug>.md)" \
  --task-title "<slug>" \
  --worktree new-top-level --name <slug> --base-branch <target> \
  --repo path:<repo root> --agent claude --setup run --json
```

- Start the whole independent wave before waiting. Dependent features: start after their
  dependency is merged, with `--base-branch` on the updated target.
- Record per feature: task id, dispatch id, terminal handle, worktree path, branch.
- `worker-start` exits non-zero → do **not** relaunch; read `failedStage` /
  `residualResources` and load `references/recovery-and-cleanup.md` from the Orca guide.
- Tell the user which agents are running and where (worktree paths) so they can watch in the Orca app.

## 4. Supervise and relay doubts (doubt gate)

Loop:

```sh
orca orchestration check --wait --types "worker_done,escalation,question" --timeout-ms 900000 --json
```

For every message in the delivery, before `--ack`:

- **question** — classify:
  - Already decided in the approved plan, `CLAUDE.md` or `docs/` → reply citing the source.
  - Anything else (API shape, UX, naming visible to users, new dependency, new env var,
    scope change, trade-off) → `AskUserQuestion` to the user, prefixed with the slug,
    with the worker's options + your recommendation. Reply with the user's answer verbatim
    plus any context: `orca orchestration reply --id <message_id> --body "..." --json`.
  - Several questions pending at once → batch them in one `AskUserQuestion` call.
- **escalation** — summarize to the user, ask how to proceed.
- **worker_done** — validate it matches the active dispatch, then go to step 5 for that
  feature. Do **not** release the worker yet (it may need follow-ups).

Timeouts/empty waits are checkpoints. After 3 empty waits run
`orca orchestration worker-list --run <run_id> --json` and act on `projection.nextAction`.
Never stop/abandon a worker without positive proof it exited.

## 5. Verify each finished branch yourself

In the worker's worktree (don't trust the summary alone):

```sh
git -C <wt> log --oneline <target>..HEAD
git -C <wt> status --short                       # must be clean
cd <wt> && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
cargo deny check                                  # if Cargo.toml/Cargo.lock changed
```

Also check: rules in `CLAUDE.md` respected (provider-agnostic API, bounded buffers, no
`innerHTML` with service data, `is_self` protection, POST + same-origin, read-only mode),
docs updated in the same branch, ROADMAP ticked if applicable, Conventional Commits.
For UI changes, take a headless-Chrome screenshot on the worker's port (see `CLAUDE.md`
→ Testing) and show it to the user.

Problems → send a follow-up to the **same terminal** as a new dispatch:

```sh
orca orchestration worker-start --spec "<fix list>" --terminal <handle> --worktree <selector> --json
```

## 6. User review (merge gate)

Present per feature: commits, `git diff --stat <target>...<branch>`, check results,
screenshot if UI, deviations from plan, and how to try it
(`cd <wt> && CUTHULU_BIND=127.0.0.1:<port> cargo run`, or whatever the brief used).
Ask the user to approve, request changes (→ step 5 follow-up loop), or drop it.
Approval of one feature does not approve the others.

## 7. PRs and merge (only approved features)

Merge order: dependencies first, then smallest/least-overlapping first. For each:

```sh
git -C <wt> fetch origin
git -C <wt> merge origin/<target>   # only if behind; resolve conflicts, re-run step 5 checks
git -C <wt> push -u origin <branch>
gh pr create --base <target> --head <branch> --title "<type>(<scope>): <summary>" --body-file <scratchpad>/pr-<slug>.md
gh pr checks <pr> --watch            # CI must be green
gh pr merge <pr> --merge             # repo uses merge commits; branch auto-deletes on merge
```

PR body: Summary (bullets), Changes, Testing (exact commands + results), Screenshots if UI,
Notes/decisions made with the user, then the attribution line required by the session.

- Conflicts that need a judgment call → ask the user, don't guess.
- CI red → fix via follow-up dispatch to the worker, re-verify, push again.
- After each merge, the next branch must re-merge `origin/<target>` and re-run checks.

## 8. Cleanup and report

- `orca orchestration worker-release --dispatch <id> --json` for each settled worker
  (or `worker-retain` if the user wants to keep it). Then
  `worker-list --run <run_id> --terminal-state reclaimable --json` must return none.
- Ask before removing merged worktrees (`orca worktree rm --help`).
- Locally: `git checkout <target> && git pull --ff-only`.
- Stop any throwaway `cuthulu-<slug>-*` containers left behind.
- Final report per feature: PR URL, merge commit, outcome, anything dropped or deferred.
