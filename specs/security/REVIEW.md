# Security Review — Epic e01: Tailscale Gateway & Dashboard Editor

## Scope
- Branch: `feat/tailscale-gateway-editor`
- Files: `crates/oxllm/src/main.rs`, `crates/oxllm/src/dashboard.html`, `crates/oxllm-core/src/config.rs`, `docs/architecture.md`, `CHANGELOG.md`
- Threat Model: `specs/security/epics/e01/THREAT_MODEL.md`

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
