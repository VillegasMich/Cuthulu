Distilled from the Rust API Guidelines, Clippy (pedantic), the Rust Book/Reference,
tokio docs, and this repo's conventions. Repo rules in `CLAUDE.md` win on conflict.

### Before writing code
- Read the surrounding module and match its style, naming, comment density and
  error patterns. Reuse existing helpers before adding new ones.
- Smallest change that meets the acceptance criteria. No speculative abstraction,
  no new trait/generic until there is a second real implementation.

### Types and API design
- Make invalid states unrepresentable: enums over bool flags/stringly fields,
  newtypes for IDs and units (`ServiceId(String)`, `Duration` not `u64` secs).
- Names follow RFC 430: `snake_case` fns/modules, `UpperCamelCase` types,
  `as_`/`to_`/`into_` conversion semantics, `iter`/`iter_mut`/`into_iter`.
- Take borrowed inputs (`&str`, `&[T]`, `impl AsRef<Path>`), return owned outputs.
  Avoid needless `clone()`; clone only at ownership boundaries (e.g. into a spawned task).
- Derive the common traits that make sense (`Debug`, `Clone`, `PartialEq`, `Eq`,
  `Default`, `serde::{Serialize, Deserialize}`) — `Debug` on every public type.
- Keep visibility minimal: `pub(crate)` by default, `pub` only for real API.
- Prefer `#[must_use]` on pure functions returning values the caller must use
  (clippy pedantic will flag them).
- Builders or config structs instead of functions with many positional args.

### Errors
- Library code: typed errors with `thiserror`, one enum per module/domain,
  variants carrying context (`#[error("container {id} not found")]`). Use `#[from]`
  for wrapped sources. `anyhow` only in `main.rs`.
- No `unwrap()`/`expect()`/`panic!` on runtime data in non-test code. `expect` allowed
  only for true invariants, with a message stating the invariant.
- Propagate with `?`; don't log-and-return the same error twice.
- HTTP handlers map errors to proper status codes; never leak internal paths or
  secrets in error bodies.

### Async / tokio
- Never block the runtime: no `std::thread::sleep`, blocking IO or heavy CPU in async
  fns — use `tokio::time`, `tokio::fs`, or `spawn_blocking`.
- Never hold a `std::sync::Mutex` guard across `.await`; keep lock scopes tiny.
  Prefer message passing or `RwLock` reads for hot paths.
- Every channel/stream is bounded (`mpsc::channel(n)`, `broadcast::channel(n)`); handle
  `Lagged` / full-buffer cases explicitly.
- Spawned tasks must have a shutdown path (cancellation token, channel close, or
  `JoinHandle` abort) and must not silently swallow errors — log them with `tracing`.
- Use `tokio::select!` with cancellation-safe futures; document when one is not.
- Timeouts on every external call (Docker API, HTTP) that could hang.

### Performance and allocation
- Iterators over index loops; `collect` once; `with_capacity` when size is known.
- `&str`/`Cow` over `String` where ownership isn't needed. `Arc` for shared
  read-mostly state, not `Rc` (must be `Send`).
- No per-service polling (repo rule): event streams + periodic reconcile; per-service
  work is on demand and bounded.

### Safety and security
- No `unsafe`. No new dependency without asking the coordinator (license and
  `cargo deny` impact); prefer std or crates already in `Cargo.lock`.
- Treat container names, labels, logs and env as untrusted input. Never render
  env var values by default.
- State-changing routes: POST + same-origin check, honor `CUTHULU_READ_ONLY`,
  never allow stopping the `is_self` service.

### Tests
- Unit-test pure functions next to the code (`#[cfg(test)] mod tests`). Mapping
  and parsing logic must be pure and tested.
- HTTP/registry behavior: use `tests::MockProvider` + `tower::ServiceExt::oneshot`;
  no real Docker needed.
- Test names describe behavior (`stop_rejects_self_service`). Cover error paths and
  edge cases, not only the happy path. Follow existing tests' assertion style
  (e.g. `assert_eq!(v, [])` over `assert!(v.is_empty())`, which newer clippy flags).
- Tests must be deterministic: no sleeps for synchronization, use `tokio::time::pause`
  or explicit signals.

### Docs and comments
- `///` doc comments on public items: what, not how; include `# Errors` for fallible
  public fns (clippy pedantic `missing_errors_doc`).
- Comments explain *why*, not *what*. No commented-out code, no TODO without context.
- Update `docs/ARCHITECTURE.md` for new env vars, routes or provider changes;
  `docs/ROADMAP.md` ticks when an item is done.

### Lints and formatting (must pass before `worker_done`)
- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings` (pedantic on). Fix the cause; add
  `#[allow(clippy::...)]` only with a one-line justification comment, as narrowly as possible.
- `cargo test`
- `cargo deny check` if dependencies changed.
- MSRV is 1.88 — don't use newer std/language features.
