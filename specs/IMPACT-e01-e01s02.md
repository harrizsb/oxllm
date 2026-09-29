# Impact assessment: e01s02 raw config and Validate

## Target

`Config` deserialization/validation, `GET /config`, `POST /validate`, and dashboard editor behavior.

## Dependents

- `crates/oxllm-core/src/config.rs`: config structs, `load_from_file`, `validate`, `expand_env_vars`.
- `crates/oxllm/src/main.rs`: serve, validate, HTTP reload, SIGHUP reload, router construction.
- `crates/oxllm/src/routes.rs` and `crates/oxllm/src/dashboard.html`: dashboard response and UI.
- Existing integration tests for CORS and `/v1` routes.

## Affected Stories

- e01s02: raw config view and dry-run validation.
- e01s03: Apply reuses the same candidate pipeline and Origin policy.

## Test Coverage

- Existing config parsing/validation tests in `oxllm-core`.
- Existing router and CORS tests in `oxllm`.
- New tests required for unknown fields, raw bytes, Origin matrix, parser diagnostics, and zero writes.

## Risk: Medium

Config parsing is shared by startup, reload, and Apply. Strict unknown-field rejection is intentionally breaking for stale configs and requires precise diagnostics.

## Recommended action

Proceed with the approved plan. Keep `/v1` CORS behavior unchanged, preserve existing valid configs, and add the security-review Origin and failure-injection cases to verification.
