# Handoff Completion — oxllm Extension (as-built)

**Reason for existence:** durable record of what `handoff.md` produced, which judgment calls were
made, and how each claim was verified. Read this before touching virtual-model routing, token
accounting, the request log, or the dashboard.

**Provenance:** branch `feat/virtual-models-swrr`, PR
[harrizsb/oxllm#1](https://github.com/harrizsb/oxllm/pull/1), spec `handoff.md` (in-repo, same PR).
Written after implementation as an as-built record — not a plan-first document.

## Commits

| Commit | Type | Scope |
|---|---|---|
| `983a0a6` | feat | Weighted SWRR selection for virtual-model targets |
| `89f1e7b` | test | Weighted distribution + name-collision warning |
| `f6e1021` | test | Config validation: zero weight, empty targets, unknown provider, omitted section |
| `6102293` | feat | Daily token accounting, last-3 request log, read-only dashboard |
| `721e1f6` | fix | Record attempted provider on transport failures (502 log entries) |
| `684e06b` | test | Dashboard must render virtual-model routing overview |
| `72249b3` | feat | Virtual-model routing overview on the dashboard (`weight` in `/status`) |

## What was built and where

1. **Virtual models with weighted routing** — `[virtual_models]` in `config.toml`
   (`crates/oxllm-core/src/config.rs`), SWRR cursor + candidate ordering
   (`crates/oxllm-core/src/state.rs`), resolution before the provider fallback chain
   (`crates/oxllm/src/routes.rs`). Equal weights reproduce the pre-existing sequential order per
   full cycle. Health-aware: open-circuit/rate-limited targets are skipped by the existing
   failover loop; they rejoin when the circuit closes.
2. **Daily token accounting** — `crates/oxllm-core/src/runtime.rs`: `DailyTokenAccounting` with
   lock-free `fetch_add` updates, CAS day rollover (`RESETTING_DAY` sentinel + `active_writers`
   gate). `uncached = prompt_tokens.saturating_sub(cached_tokens)`; missing
   `prompt_tokens_details` counts as cached 0. Late previous-day updates and clock skew never
   roll counters backward. Snapshots drive rollover too, so idle services reset without traffic.
3. **Last-3 request log** — same module, `Mutex<VecDeque>` capped at 3, exactly the handoff's
   field list. Logged at: unmapped-model 400s, chat/embeddings successes, streaming successes
   (zeros + `TODO(streaming)`), and final 502s.
4. **Read-only dashboard** — `crates/oxllm/src/dashboard.html`, embedded via `include_str!`,
   served at `GET /dashboard` behind the localhost-only middleware. Plain HTML, vanilla JS, no
   forms or buttons. Fetches the extended `/status` (added `daily_tokens`, `recent_requests`
   newest-first, `virtual_models` with weights).

## §7 judgment calls (decided, do not relitigate without cause)

| Decision | Choice |
|---|---|
| Code placement | New module `oxllm-core/src/runtime.rs` for metrics; routing state extended in `state.rs` to keep `AppState` the single routing source of truth |
| Dashboard exposure | Extend existing `/status` JSON; one extra route `/dashboard` serving the static page — no second data endpoint |
| Config schema | `[virtual_models]` = map name → inline-target array: `{ provider, model, weight }`; `weight` optional, positive, defaults 1; section optional (backward compatible) |
| Ring buffer | `Mutex<VecDeque>` — hot path does not contend it; poisoned lock recovers via `into_inner()` |
| SWRR state | One cursor per virtual model in `AppState.swrr_current` (`Mutex<HashMap<..>>`), reset only when target count changes |
| Name collisions | Virtual models win over same-named provider models; startup `warn!` for each shadow (recommended by handoff §9) |
| Metrics across reloads | `Arc<RuntimeMetrics>` shared by `AppState` and the SIGHUP/`/reload` `Reloader`, so counters and log survive config reloads |
| Transport failures | `last_failed_provider` captured so 502 log entries name the attempted provider (`721e1f6`) |

## Verification evidence (recorded in PR #1)

- `cargo test --workspace`: 40 passed, 0 failed (17 binary, 22 core, 1 integration harness).
- `cargo fmt --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean.
- Live run vs mock upstream: accounting 40 cached / 80 uncached / 150 total; ring order correct
  across 200 / 400 / 502; SIGHUP preserved metrics; `/proc/<pid>/fd` diff across traffic showed
  zero new files — no runtime disk writes from the new features.
- Release binary 4.01 MB vs 3.99 MB baseline (+19 KB, no new dependencies; limit < 15 MB).

## Known limitations

- **Streaming token accounting is not implemented.** SSE successes log zeros with
  `TODO(streaming)` at the call site (handoff §5 allowance). Final-chunk `usage` parsing is the
  next step if needed.
- Everything is in-memory; restart wipes metrics and log (by design).

## Pre-existing issues noted (handoff §10: note, don't fix)

- Startup writes `/tmp/oxllm.pid` and shutdown removes it (`crates/oxllm/src/main.rs`), which
  predates this work and conflicts with the strict no-disk reading of §3. Unchanged here.

## How to verify now

```sh
cargo test --workspace && cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings
./target/release/oxllm validate --config config.toml
./target/release/oxllm serve --config config.toml &   # then GET /dashboard and /status
```

## Amendment A1 (rev 2): Tailscale Gateway & Live Configuration Editor

Implemented under epic `e01` (`feat/tailscale-gateway-editor`):
1. **Tailscale & Loopback Security Boundary (`e01s01`)**: Gateway only accepts requests originating from loopback (`127.0.0.0/8`, `::1`) or the Tailscale IPv4 CGNAT range (`100.64.0.0/10`). All unauthorized requests receive a structured JSON 403 error retaining the request ID. IPv4 host binding is validated strictly; wildcard `0.0.0.0` and IPv6 are rejected. Coordinated dual listeners bind the configured IPv4 and `127.0.0.1`.
2. **Raw Config Inspection & Dry-Run Validation (`e01s02`)**: `GET /config` serves verbatim bytes of `config.toml` (`Cache-Control: no-store`) without expanding environment placeholders `${VAR}`. `POST /validate` dry-runs parsing, expansion, strict configuration validation, and `build_app_state` in memory without writing to disk. Editor routes enforce same-origin verification while allowing non-browser tools lacking `Origin`.
3. **Transactional Apply Pipeline & Dashboard Integration (`e01s03`)**: `POST /apply` coordinates with SIGHUP and HTTP reload using a shared async lock. It validates and prebuilds the new `AppState` before staging file writes. A one-generation backup (`config.toml.bak`) is staged and synced before the new configuration replaces `config.toml`. Failures before commit or during state publication restore original bytes atomically and sync the parent directory. Persisted changes to `host`, `port`, or `otel_endpoint` are marked with `restart_required`.
