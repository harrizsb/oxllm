# Impact Assessment — e01 tailscale-gateway-editor (assess-impact, default mode)

## Target
- `crates/oxllm/src/main.rs`: `run_serve` listener setup (bind_family match block), `localhost_only` middleware, route table.
- `crates/oxllm-core/src/config.rs`: `Config`/`ServerConfig`/`ProviderConfig`/`VirtualModelTarget` structs + `validate()`.
- `crates/oxllm/src/routes.rs` + `dashboard.html`: new handlers, editor UI.
- Decision logged (user, this session): option 1 — `[server].host` authoritative; `bind_family` restricted to `"ipv4"`; ipv6/dual wildcard binding removed; socket2 dropped.

## Dependents (5)
- `main.rs run_serve` — sole consumer of bind_family/host/port; called once from CLI dispatch.
- `localhost_only` — layered on 7 routes (status, dashboard, health, reload, admin x3). `/v1/models|embeddings|chat/completions` currently UNGUARDED (verified — true before-state).
- `Config` structs — main.rs (load/validate/reload paths), state.rs (`VirtualModelTarget`), routes.rs (indirect), config.rs tests.
- `dashboard.html` — `include_str!` in `routes::dashboard`; commit 684e06b added a page-content test.
- `Cargo.toml` — socket2 removal affects both workspace and oxllm crate manifests.

## Affected Stories
- e01s01 (binding + guard) — main.rs, Cargo.toml
- e01s02 (raw config + validate) — config.rs, routes.rs, dashboard.html
- e01s03 (apply pipeline) — routes.rs, main.rs (reload machinery), dashboard.html

## Test Coverage
- `oxllm-core/src/config.rs` tests: weight defaults/zero-reject, omitted virtual_models, env expansion.
- `oxllm-core/tests/performance.rs`: latency perf only.
- `oxllm` crate: dashboard page-content test only. **Gap: zero tests for listener setup, guard middleware, /reload, admin routes.**
- Gap: no test binds a real server socket today; S1 introduces the first (ephemeral-port).

## Risk: Medium
Few callers, structs are private to the workspace, but the guard and bind changes are a security boundary and the apply path writes disk; coverage gaps on exactly the touched paths require new tests before merge (each story carries its own).

## Recommended action
Proceed — each story's tasks include the missing coverage as acceptance; strict gates (fmt/clippy/tests) run per story.

## Unknowns / preflight gaps
- Unknown: [tech-stack.md absent; no authoritative architecture doc available].
- Unknown: [GLOSSARY_LATEST.yaml absent; domain glossary unavailable].
- Unknown: [e01 test-plan artifact absent; risk mapping uses plan-work heuristics].
- Unknown: [THREAT_MODEL.md absent; security fields derive from code review and scope, not threat-model artifact].
- `scripts/lib/plan-consistency-check.sh`, `scripts/bp-timing.sh`, `scripts/sync-status-from-epics.sh`, and the referenced countable-story-format doc are not present in this checkout/agent docs; consistency is checked manually and format uses all 20 numbered slots from the plan-work format mandate.

## Security finding added during plan review
- `main.rs` configures permissive CORS (`allow_origin(Any)`, any headers) for all routes. A third-party webpage opened in a tailnet-connected browser could otherwise GET `/config` and read secrets because the server sees the browser’s Tailscale source IP. Plan mitigation: editor endpoints reject a present Origin unless it matches Host; non-browser requests without Origin remain allowed. This is a browser-origin check, not app authentication.
