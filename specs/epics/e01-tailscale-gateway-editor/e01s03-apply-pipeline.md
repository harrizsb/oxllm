# Story e01s03: Apply — one-generation backup, atomic swap, in-process reload

**1. Story ID:** e01s03 · **2. Title:** Apply: one-generation backup, atomic swap, in-process reload · **3. Type:** feat · **4. Risk:** P0 (disk writes + reload boundary) · **5. Context:** Apply makes validated editor content the active config without a restart, with exactly one generation of backup and no partial states on failure.

**6. Zoom-out:** *handle_http_reload/handle_sighup* — Purpose: reload config into live state. Callers: router (/reload), signal handler. Contracts: old state retained on failure, shared RuntimeMetrics across reloads. Apply joins this family behind one serialization lock. *Reloader* — carries watch sender, resolved config path, shared metrics; Apply reuses it.

**7. Requirements (deltas):**
#### MODIFIED: reload/apply ordering (critical fix found in review)
**Before:** /reload and SIGHUP load → validate → build → send with no serialization between them.
**After:** one lock serializes HTTP reload, SIGHUP reload, and Apply; Apply runs parse → expand → validate → **build_app_state → then** one-generation .bak (exact old bytes) → tmp write + flush + fsync + rename (same directory, permissions preserved) → publish prebuilt state via watch sender; publish failure restores the old file bytes atomically; any pre-commit failure leaves config, .bak, and live state untouched.
#### ADDED: POST /apply
application/json {"config": "<raw TOML>"}; bounded body; same-origin Origin/Host check when exactly one valid Origin is present (reject null, malformed and duplicate values; non-browser clients without Origin allowed); response includes restart_required listing changed startup-bound fields (host, port, otel_endpoint) — these save to disk but only take effect at restart; invalid input → 400 diagnostics, zero writes.
#### MODIFIED: dashboard
**Before:** editor with Validate only (from e01s02).
**After:** Apply button with confirm(), disabled while pending, result feedback; diagnostics renderer shared with Validate.
#### MODIFIED: docs/CHANGELOG
Records reload semantics truthfully: RuntimeMetrics (daily tokens + last-3 request log) persist; provider counters, circuit state, admin-disabled flags, SWRR cursors reset on every reload/apply; [server] listener/otel fields are startup-bound.

**8. Success criteria:** valid apply → .bak == old bytes, file == new bytes, /status reflects new providers, metrics counters continuous, no restart; invalid apply → 400, file and .bak byte-identical, service unaffected; second apply overwrites .bak (one generation); gates + size <15MB.

**9. Implementation steps:** e01s03-tasks.yaml tasks 1–7. **10. Verification script (manual):** editor → change a provider weight → Apply → confirm dialog → success notice; `ls config.toml.bak` exists with old content; dashboard routing weights update without restart; introduce syntax error → Apply → 400 diagnostics shown, `git status` shows config unchanged.

**11. Test plan:** tasks 2–4 unit/integration coverage incl. serialization (two concurrent applies can't interleave), staged-write/sync/backup/config-rename failure injection, restoration of prior config and .bak bytes, rollback on publish failure, and explicit reporting if rollback fails. **12. Security:** Editor endpoints GET /config, POST /validate and POST /apply reject a present cross-origin Origin; absent Origin remains allowed for direct clients. This prevents browser-based secret reads/writes from an unrelated webpage; it is not authentication. CORS Any remains on pre-existing routes (/v1 semantics unchanged); write path is single explicit action; only .bak + config.toml are ever written. **13. Observability:** info! on successful apply (request id), warn! on validation failure, error! on restore-after-publish-failure. **14. Out of scope:** tailnet ACLs; firewall; per-device auth; multi-generation backups. **15. Risks:** publish-after-write window — mitigated by prebuilt state + restore step; concurrent SIGHUP during apply — mitigated by the shared lock (SIGHUP handler acquires it). **16. Dependencies:** e01s01, e01s02. **17. Acceptance criteria (Gherkin):**
```gherkin
Scenario: Valid apply swaps config without restart
  Given the editor holds a valid config that adds provider "new-prov"
  When the operator confirms Apply
  Then config.toml.bak contains the previous config exactly
  And /status lists new-prov without the process restarting
  And daily token counters continue from their prior values

Scenario: Invalid apply writes nothing
  Given the editor holds a config with a syntax error
  When the operator confirms Apply
  Then the response is 400 with diagnostics
  And config.toml and config.toml.bak are byte-identical to before

Scenario: Cross-origin browser write is refused
  Given a browser page served from another origin
  When it POSTs /apply with its Origin header
  Then the request is rejected before any write
```
**18. Definition of done:** tasks passing; evidence in specs/verifications/. **19. Open questions:** none. **20. References:** SCOPE S5/S7/S8; IMPACT_LATEST; handoff.md A1 rev2; subagent review findings (apply-ordering flaw, serialization gap, CSRF) — verified against main.rs/config.rs.
