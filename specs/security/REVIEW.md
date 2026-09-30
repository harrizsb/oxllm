# Security Review — e01 Gateway Editor and e02 Provider Ping

## Scope
- e01 branch: `feat/tailscale-gateway-editor`
- e01 files: `crates/oxllm/src/main.rs`, `crates/oxllm/src/dashboard.html`, `crates/oxllm-core/src/config.rs`, `docs/architecture.md`, `CHANGELOG.md`
- e01 Threat Model: `specs/security/epics/e01/THREAT_MODEL.md`
- e02 branch reviewed: `feat/provider-model-ping` (PR #6)
- e02 files: `crates/oxllm/src/main.rs`, `crates/oxllm/src/routes.rs`, `crates/oxllm/src/dashboard.html`
- e02 Threat Model: `specs/security/epics/e02/THREAT_MODEL.md`
- e02 follow-up review findings: HalfOpen probe permit handling fixed; non-JSON ping rejection bodies now render as request-rejected results. `ChatAttempt::Skipped` still returns HTTP 500 with JSON status 502 and does not add a request-log entry; it can only occur on local serialization/URL-join failure and is a minor diagnostics mismatch.

## Findings Matrix

| Severity | Category | Status | Details |
|---|---|---|---|
| MEDIUM | Origin Verification | Resolved | `require_same_origin` rejects cross-origin browser requests on editor routes (`/config`, `/validate`, `/apply`) while allowing non-browser clients lacking `Origin`. |
| HIGH | Secret Leak via Backup Permissions | Resolved | Backup files and staged temp files inherit source config permissions at creation (`mode & 0777`), preventing leakage across umasks. Verified by unit test. |
| HIGH | Stale Backup on Publish Failure | Resolved | Pre-existing backup removal during rollback utilizes `remove_file_synced` ensuring filesystem journal durability. |
| HIGH | Concurrency / Race Hazard | Resolved | Shared mutex `reload_lock` across HTTP `/reload`, SIGHUP signal listener, and `/apply`. Concurrent applies are serialized; verified by concurrency test. |
| LOW | IPv6 CGNAT Bypass | Resolved | Server rejects IPv6 and wildcard `0.0.0.0` at validation; dual binds literal IPv4 and `127.0.0.1`. |

## Unresolved High Findings
None.
