# Verification Evidence — Story e01s03: Apply Pipeline

## Scope
- Story: `e01s03` (Apply: one-generation backup, atomic swap, in-process reload)
- Working tree: `/home/butler/oxllm-tailscale-gateway-editor`
- Target Branch: `feat/tailscale-gateway-editor`

## Preconditions & Automated Verification Runs

### Task 1: Serialization of Reload Paths
- Command: `cargo test -p oxllm reload_serial`
- Result: Passed (2 tests)
  - `reload_serial_lock_serializes_across_clones`
  - `reload_serial_concurrent_applies_cannot_interleave`

### Task 2: Candidate Validation & Pre-Commit Isolation
- Command: `cargo test -p oxllm apply_invalid_no_write`
- Result: Passed (1 test)
  - `apply_invalid_no_write_and_preserves_old_state`

### Task 3: Atomic Staging, Backup, and Fault Injection
- Command: `cargo test -p oxllm apply_atomic`
- Result: Passed (3 tests)
  - `apply_atomic_sync_failure_leaves_config_and_bak_untouched`
  - `apply_atomic_rename_failure_restores_previous_backup`
  - `apply_atomic_backup_rename_failure_preserves_config_and_existing_backup`

### Task 4: In-Process Continuity & Rollback Fault Injection
- Command: `cargo test -p oxllm apply_reload_continuity`
- Result: Passed (3 tests)
  - `apply_reload_continuity_swaps_config_writes_exact_backup_preserves_metrics`
  - `apply_reload_continuity_publish_failure_rolls_back_atomically`
  - `apply_reload_continuity_publish_rollback_failure_is_reported`

### Task 5: POST /apply Endpoint, Origin Enforcement, Dashboard
- Command: `cargo test -p oxllm apply_origin && grep -q apply-config crates/oxllm/src/dashboard.html`
- Result: Passed
  - `apply_origin_cross_origin_rejected_before_write`
  - Dashboard contains confirmation prompt, `apply-config` trigger button, busy state disabling, safe text rendering, and restart-required diagnostics.

### Task 6: Documentation & Changelog
- Command: `grep -q "Amendment A1" docs/handoff-completion.md && grep -q "Unreleased" CHANGELOG.md`
- Result: Verified. Reload semantics, metric persistence vs provider resetting, and restart constraints are recorded in `CHANGELOG.md` and `docs/handoff-completion.md`.

### Task 7: Repo Quality & Size Gates
- `cargo fmt --check`: Passed
- `cargo clippy --workspace --all-targets -- -D warnings`: Passed (0 warnings)
- `cargo test --workspace`: Passed (69 tests: 38 oxllm, 30 oxllm-core, 1 bench/perf)
- Release Binary Size:
  - Command: `cargo build --release -p oxllm && stat -c '%s' target/release/oxllm`
  - Output: 4,138,104 bytes (~3.95 MB)
  - Gate (< 15 MB / 15,728,640 bytes): PASSED

## Security & Concurrency Review Resolution
1. **Backup Mode Integrity**: Backup temp files are staged matching `config.toml` permissions (e.g. `0600`), preventing secret leaks across default umasks. Verified by `apply_backup_permissions_are_no_broader_than_config`.
2. **Directory Sync on Deletion**: Pre-existing backup removal during rollback utilizes `remove_file_synced` ensuring filesystem journal durability.
3. **Rollback Fault Visibility**: Unrecoverable rollback errors are surfaced explicitly in the error response format and logged via `error!`.
