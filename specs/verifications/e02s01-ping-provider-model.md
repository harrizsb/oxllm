# Verification — e02s01 Ping provider-model via shared chat path

Date: 2026-09-30. Worktree: `/home/butler/oxllm-ping`, branch
`feat/provider-model-ping` rebased on `origin/main` (`0134e75`, includes
per-provider `user_agent`).

## What was built

- Task 1: `attempt_chat_completion` helper extracted from
  `create_chat_completions` (`crates/oxllm/src/routes.rs`). One shared request
  path for the normal chat loop and the ping. The helper preserves the
  `user_agent` header behavior added on `origin/main`.
- Task 2: `POST /admin/ping` accepts `{provider, model}`, validates both
  against config state (400 otherwise), builds a fixed minimal payload
  (`max_tokens: 1`, single `ping` user message), calls the shared helper once,
  and returns `{ok, status, latency_ms, error?}`.
- Task 3: Route registered in `build_router` behind the tailnet guard and the
  same-origin layer (same as `/config`, `/validate`, `/apply`). A 4 KiB body
  limit is scoped to `/admin/ping`.
- Task 4: `dashboard.html` "Provider model checks" section renders one row per
  configured provider model with a Ping button (busy-state disabled while
  in-flight) and an inline result cell (success with latency, or upstream
  error). Vanilla JS, `include_str!` embedded, no new dependencies.
- Task 5: Integration tests with mock upstream.

## Red-first evidence

The 5 endpoint tests were written and run before the route was registered:
all failed with status 404 (route missing). The body-cap test was added with
the middleware change. After registering the route all tests pass.

## Gates

- `cargo fmt --all -- --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo test --workspace`: 48 + 32 + 1 tests, 0 failed.
- `cargo build --release`: `target/release/oxllm` 4,170,528 bytes
  (< 15,728,640 limit).

## Coverage of success criteria

- (a) Ping sends one real minimal completion to the chosen provider+model and
  renders inline: `test_integration_ping_success` (mock upstream returns
  usage; endpoint returns ok + latency) and dashboard Ping button.
- (b) Full parity: `test_integration_ping_success` asserts provider
  requests/successes counters, a `recent_requests` entry with the requested
  model and provider, and `daily_tokens.total_tokens == 13` (10 prompt + 3
  completion from the mock upstream usage).
- (c) Failed ping feeds circuit-breaker feedback:
  `test_integration_ping_failure_feeds_circuit` asserts requests == 1,
  successes == 0, request-log status 500, and `ok: false` with the upstream
  status and a non-empty error.
- (d) Unknown provider/model return 400 with a diagnostic and nothing sent
  upstream (`test_integration_ping_unknown_provider_returns_400`,
  `test_integration_ping_unknown_model_returns_400`); cross-origin browser
  request returns 403 before any upstream call
  (`test_integration_ping_cross_origin_forbidden`). Also
  `router_builds_and_guards_all_routes` covers the tailnet guard for
  `/admin/ping` (loopback allowed, public peer 403 with `x-request-id` and
  JSON error), and `test_integration_ping_body_too_large_is_413` covers the
  scoped body cap.
- (e) Gates listed above are green and the binary stays under the size limit.

## Security notes

- The endpoint sits behind the tailnet guard (defense in depth) and the
  same-origin layer (blocks third-party pages in a tailnet-connected browser).
- The ping payload is fixed server-side; callers cannot shape the upstream
  request, amplify tokens (`max_tokens: 1`), or reach models outside the
  provider's configured list.
- The 400/403/413 paths send nothing upstream.
- No `unwrap`/`expect` was added in handler paths (shared helper keeps the
  existing error-tolerant style).

## Manual verification performed

- `cargo test -p oxllm ping_` run before and after route registration (red,
  then green).
- The dashboard change is markup/JS exercised by the dashboard content test
  (`/admin/ping` and `Ping` assertions); full browser interaction was not
  exercised in this environment.
