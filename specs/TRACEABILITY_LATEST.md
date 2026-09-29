# Traceability Matrix — Epic e01: Tailscale Gateway & Dashboard Editor

## Plan & Specification Coverage

| Story ID | Title | Tasks YAML | Verification Evidence | Status |
|---|---|---|---|---|
| `e01s01` | Tailnet reachability: dual bind + tailnet/loopback guard | `e01s01-tasks.yaml` | `specs/verifications/e01-plan-consistency.md` | Done |
| `e01s02` | Raw config view + Validate dry-run on dashboard | `e01s02-tasks.yaml` | `specs/verifications/e01s02-raw-config-validate.md` | Done |
| `e01s03` | Apply: one-generation backup, atomic swap, reload | `e01s03-tasks.yaml` | `specs/verifications/e01s03-apply-pipeline.md`, `specs/verifications/e01s03-verify.yaml` | Done |

## Code Implementation Mapping
- `e01s01`: `crates/oxllm/src/main.rs` (`tailnet_only` middleware, `is_tailnet_or_loopback`, `bind_server_listeners`), `crates/oxllm-core/src/config.rs` (`bind_family = "ipv4"` validation, literal IPv4 host validation).
- `e01s02`: `crates/oxllm/src/main.rs` (`GET /config`, `POST /validate`, `require_same_origin`), `crates/oxllm/src/dashboard.html` (editor UI, Validate action), `crates/oxllm-core/src/config.rs` (`deny_unknown_fields`).
- `e01s03`: `crates/oxllm/src/main.rs` (`POST /apply`, `Reloader.reload_lock`, `apply_candidate`, atomic staging, `remove_file_synced`, `ReloadIo` fault injection), `crates/oxllm/src/dashboard.html` (Apply action, confirmation prompt, busy state, restart required feedback).
