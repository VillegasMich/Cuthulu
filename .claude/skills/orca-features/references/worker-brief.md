# Feature: <slug>

You are a worker agent implementing ONE feature for Cuthulu in your own worktree
(`<worktree path>`, branch `<branch>`, based on `<target>`). A coordinator agent
supervises you and talks to the user on your behalf.

## Goal
<one paragraph: what the user should be able to do when this is done>

## Scope
- <item>

## Out of scope / do not touch
- <item — e.g. files owned by another parallel feature: ...>

## Design decisions already made with the user
- <decision — reason>

## Acceptance criteria
- <observable behavior / test that proves it>
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` all pass.
- Docs in `docs/` updated for any changed decision, new env var, API route or UI behavior.

## Ground rules

1. **Read first:** `CLAUDE.md`, then the `docs/` files it lists that relate to this
   feature. Follow `docs/DESIGN.md` exactly for any HTML/CSS. Follow the Rust
   guidelines below.
2. **Doubts → ask, don't guess.** If anything about behavior, API shape, UX, naming,
   scope, a new dependency, a new `CUTHULU_*` env var, or a trade-off is not settled
   above or in `docs/`, use the orchestration `ask` command from your preamble and wait
   for the answer. Give 2–3 concrete options and your recommendation. Do not open a local
   question prompt — nobody sees it. Purely internal choices (private helper names,
   test layout) you decide yourself.
3. **Dev server:** use port `<port>` only (never 8686). Start it in the background,
   save `$!`, and stop it with `kill <that PID>`. Never `pkill`/`killall` — other agents'
   servers and the user's own run on this machine.
4. **Docker:** never stop/restart existing containers. For action tests create
   throwaway containers named `cuthulu-<slug>-*` and remove them when done.
5. **Commits:** small, Conventional Commits (`feat(scope): ...`, `fix`, `test`, `docs`,
   `refactor`), commit locally on your branch. **Do not push, open PRs or merge** — the
   coordinator does that after the user approves.
6. **Stay in your worktree.** Don't edit other worktrees or the main checkout.
7. **Before `worker_done`:** working tree clean, all checks above green, then send
   `worker_done` with a three-sentence summary covering: what changed, how it was tested
   (exact commands), and any deviation or follow-up. Use `--outcome failed` if blocked.
8. If the coordinator sends follow-ups later, treat each as a new small task under the
   same rules.

## Rust guidelines

<paste the full contents of references/rust-guidelines.md here>
