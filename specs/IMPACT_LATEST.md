# Impact Assessment — provider-model-ping

## Target
`crates/oxllm/src/routes.rs` — `create_chat_completions` (hot path, ~200 lines): extract the per-provider attempt into a shared helper reused by the new `POST /admin/ping` handler.

## Dependents (4)
- `crates/oxllm/src/main.rs` `build_router` — registers `/v1/chat/completions` (call site unchanged after refactor)
- `crates/oxllm/src/main.rs` integration tests — `test_integration_sse_streaming`, `test_integration_circuit_breaker_failover`, `test_integration_all_providers_fail_gives_502`, `test_integration_rate_limit_failover` exercise the chat path via mock upstreams
- `crates/oxllm/src/dashboard.html` — gains Ping buttons (additive only)
- `crates/oxllm/src/routes.rs` `create_embeddings` — NOT modified (separate inline attempt logic; out of scope)

## Affected Stories
- e02s01 (new): Ping provider×model through the main request path
- e01s01–e01s03 (shipped): regression risk confined to the chat-completions refactor

## Test Coverage
- `crates/oxllm/src/main.rs` integration tests: chat success, streaming, circuit-breaker failover, all-providers-fail 502, rate-limit failover — all via `spawn_mock_upstream`
- Gap: no test yet for the ping endpoint or the shared-attempt extraction (added by e02s01)

## Risk: Medium
Hot-path refactor of the primary request route, but the change is a mechanical extraction (move the loop body into a helper, call it from both sites) and the integration suite covers the chat path end-to-end. No interface change; no new dependencies.

## Recommended action
Proceed tests-first: write the ping integration test against the extracted helper before refactoring, then refactor, then add the endpoint and dashboard. Run the full gate suite before commit.
